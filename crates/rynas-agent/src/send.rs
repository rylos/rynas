//! The btrfs method without btrbk (`docs/research/pc-backup.md` §11.9): the agent takes
//! read-only snapshots of the PC's subvolumes, picks each parent from what the NAS already
//! has, and streams `btrfs send` (the system's btrfs-progs), compressed with zstd, over
//! HTTPS to rynas, which receives it into the PC's folder. The NAS owns retention; the PC
//! keeps a couple of local snapshots as parents.
//!
//! Names are btrbk's (`<name>.YYYYMMDDTHHMM[_N]`, local time) and so is the default local
//! folder ([`AGENT_SNAPSHOT_DIR`]); a PC with a `btrbk.conf` keeps its names and snapshot
//! folder, so it can switch between btrbk and the agent without a new full send.
//!
//! One run: lock `/.rynas/.lock` → refuse while `btrbk.timer` is active → manifest first
//! (inside the root subvolume, so its snapshot carries it) → per subvolume (the mounted
//! ones, then the ones nested in them without a mount of their own, each a group of its
//! own: [`nested_candidates`]): snapshot, parent, send, keep. The pure parts ([`plan_subvolumes`], [`pick_parent`],
//! [`unique_name`], [`to_delete`]) need no btrfs.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use rynas_api_types::pcagent::*;
use rynas_api_types::pcbackup::{long_timestamp, split_long_snapshot, valid_snapshot_prefix};

use crate::backup::{Counters, Failure, say};
use crate::btrbk::{self, ConfSubvolume, Layout};
use crate::http::Client;
use crate::mounts::{self, Mount};
use crate::state::now_ms;
use crate::{btrfs, manifest};

/// Local snapshots kept per subvolume (the newest sent one, the next parent, among them).
pub const KEEP_LOCAL: usize = 2;
/// How long the NAS may take to answer once the whole stream is sent.
const ANSWER_WAIT: Duration = Duration::from_secs(3600);
/// zstd level of the stream (fast; the NAS stores what btrfs receive writes).
const ZSTD_LEVEL: i32 = 3;

static CANCEL: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    CANCEL.store(true, Ordering::SeqCst);
}

/// SIGTERM/SIGINT (the service's Cancel) stop the current send at the next read, and a wait
/// for the NAS within a second ([`crate::http::cancel_on`]): the new local snapshot is
/// deleted (unless the NAS has it complete), the NAS discards a broken stream.
fn install_cancel() {
    crate::http::cancel_on(&CANCEL);
    // SAFETY: the handler only stores to an atomic
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
}

fn cancelled() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

// ------------------------------------------------------------------ what is sent

/// One subvolume to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sub {
    /// btrbk's name (the prefix of its snapshots).
    pub name: String,
    /// Path from the top level (empty when unknown, the test command).
    pub path: String,
    /// Where it is mounted, or reached through its parent when nested: the snapshot's source.
    pub mount: String,
    /// The local snapshot folder, absolute on the PC.
    pub snap_dir: String,
    /// Not mounted: the top-level path of the sent subvolume it is nested in.
    pub nested_in: Option<String>,
}

/// A subvolume found inside a sent one ([`nested_candidates`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Path relative to the sent subvolume (`games`, `games/deep`).
    pub rel: String,
    pub readonly: bool,
    /// Has a received UUID (a snapshot received onto this PC).
    pub received: bool,
    /// What is nested in it could not be listed (the error): those are missing.
    pub unlisted: Option<String>,
}

/// The subvolumes nested inside the sent ones with no mount of their own (a `@home/games`
/// made inside `@home`): the parent's snapshot holds only an empty folder there, so each is
/// sent as a group of its own, the way btrbk needs a `subvolume` section for each one.
/// `list(mount)` lists what is nested below a mount, at every depth. Left out quietly:
/// snapshot folders (the agent's, btrbk.conf's, snapper's `.snapshots`, Timeshift's),
/// container storage, [`crate::btrfs::excluded_subvolume`], cache and log paths
/// ([`btrbk::NOT_SENT`]), received snapshots, the plan's excludes naming the place or a
/// folder above it, and whatever lies inside one left out. Returns the candidates (parents
/// first) and the notes for the run: a listing that failed (of a sent subvolume or of a
/// nested one: the others go on), a place another mount hides (`mounts`: every mount point),
/// a read-only subvolume, a path that cannot be used.
pub fn nested_candidates(l: &Layout, snap_dirs: &[String], excludes: &[String], mounts: &[String], list: &dyn Fn(&str) -> Result<Vec<Found>, String>) -> (Vec<btrbk::Candidate>, Vec<String>) {
    use crate::mounts::{safe_rel, under};
    let mut out: Vec<btrbk::Candidate> = Vec::new();
    let mut notes = Vec::new();
    for c in l.subvolumes.iter().filter(|c| c.skip.is_none() && c.nested_in.is_none()) {
        let mut found = match list(&c.mount) {
            Ok(f) => f,
            Err(e) => {
                notes.push(format!("{}: the subvolumes nested in it could not be listed ({e}); any of them is not sent", c.mount));
                continue;
            }
        };
        found.sort_by_key(|f| (f.rel.matches('/').count(), f.rel.clone()));
        let mut left_out: Vec<String> = Vec::new();
        for f in found {
            let Some(rel) = safe_rel(&f.rel).filter(|r| !r.is_empty()) else { continue };
            let path = format!("{}/{rel}", c.path);
            let place = place_in(&c.mount, &rel);
            // inside one left out (its own reason was said, or it is a rule)
            if left_out.iter().any(|k| under(k, &rel)) {
                continue;
            }
            let quiet = |left_out: &mut Vec<String>| left_out.push(rel.clone());
            // mounted elsewhere: handled as a mounted subvolume (sent or not); what is
            // nested in it is found from its own mount
            if l.subvolumes.iter().any(|x| x.path == path) || not_looked_into(&rel, &place) || snap_dirs.iter().any(|d| !d.trim_matches('/').is_empty() && under(d, &place)) || f.received {
                quiet(&mut left_out);
                continue;
            }
            // left out by the plan (its place, or a folder it lies in): kept as a skipped
            // candidate, so the plan knows the exclusion names a subvolume
            if excludes.iter().map(|e| e.trim()).any(|e| e.starts_with('/') && under(e, &place)) {
                quiet(&mut left_out);
                out.push(btrbk::Candidate { path, mount: place, skip: Some("left out by the plan"), nested_in: Some(c.path.clone()) });
                continue;
            }
            // another mount over it (or over a folder on the way): the path leads there,
            // not to this subvolume
            if let Some(m) = hidden_by(&c.mount, &place, mounts) {
                notes.push(format!("{place}: a subvolume nested in {} under the mount at {m}, which hides it: not sent", c.mount));
                quiet(&mut left_out);
                continue;
            }
            if f.readonly {
                notes.push(format!("{place}: a read-only subvolume nested in {} (a snapshot?), not sent", c.mount));
                quiet(&mut left_out);
                continue;
            }
            if place.chars().any(|ch| ch.is_whitespace() || ch == '#' || ch == '"' || ch == '\'' || ch.is_control()) {
                notes.push(format!("{place}: a subvolume nested in {} whose path cannot be used in a snapshot name, not sent", c.mount));
                quiet(&mut left_out);
                continue;
            }
            if let Some(e) = &f.unlisted {
                notes.push(format!("{place}: the subvolumes nested in it could not be listed ({e}); any of them is not sent"));
            }
            let parent = out
                .iter()
                .filter(|o| o.skip.is_none())
                .filter(|o| o.path != path && under(&o.path, &path) && under(&c.path, &o.path))
                .max_by_key(|o| o.path.len())
                .map(|o| o.path.clone())
                .unwrap_or_else(|| c.path.clone());
            out.push(btrbk::Candidate { path, mount: place, skip: None, nested_in: Some(parent) });
        }
    }
    (out, notes)
}

/// The place of `rel` nested below the subvolume mounted at `mount`.
fn place_in(mount: &str, rel: &str) -> String {
    if mount == "/" { format!("/{rel}") } else { format!("{}/{rel}", mount.trim_end_matches('/')) }
}

/// Nested subvolumes never sent and never looked into: the rules of
/// [`btrfs::excluded_subvolume`] (by the path below the mount or by the place) and cache and
/// log places ([`btrbk::NOT_SENT`]).
fn not_looked_into(rel: &str, place: &str) -> bool {
    btrfs::excluded_subvolume(rel) || btrfs::excluded_subvolume(place.trim_start_matches('/')) || btrbk::NOT_SENT.iter().any(|p| crate::mounts::under(p, place))
}

/// A mount point strictly inside the parent's mount `mount`, at `place` or above it: the
/// path to `place` goes into that mount, so the nested subvolume cannot be reached there.
fn hidden_by<'a>(mount: &str, place: &str, mounts: &'a [String]) -> Option<&'a str> {
    use crate::mounts::under;
    mounts.iter().map(String::as_str).find(|m| m.trim_end_matches('/') != mount.trim_end_matches('/') && under(mount, m) && under(m, place))
}

/// What is nested below the subvolume mounted at `mount` (at most 8 levels; snapshot
/// folders, cache places and places under another mount are not looked into), with each
/// one's read-only and received state and whether its own listing failed.
fn list_nested(root: &Path, mount: &str, mounts: &[String]) -> Result<Vec<Found>, String> {
    let p = root.join(mount.trim_start_matches('/'));
    let descend = |rel: &str| {
        let place = place_in(mount, rel);
        !not_looked_into(rel, &place) && hidden_by(mount, &place, mounts).is_none()
    };
    let (v, failed) = btrfs::nested_checked(&p, 8, &descend)?;
    Ok(v.into_iter()
        .map(|(rel, _)| {
            let i = btrfs::subvol_info(&p.join(&rel));
            Found {
                readonly: i.as_ref().is_some_and(|i| i.readonly),
                received: i.as_ref().is_some_and(|i| i.received_uuid.is_some()),
                unlisted: failed.iter().find(|(r, _)| *r == rel).map(|(_, e)| e.clone()),
                rel,
            }
        })
        .collect())
}

/// The manifest's `btrbk.subvolumes`: name → path → mount, and for a nested one its
/// parent's name and its path inside it (no mount).
pub fn manifest_subvolumes(subs: &[Sub]) -> Vec<BtrbkSubvolume> {
    subs.iter()
        .map(|s| {
            let parent = s.nested_in.as_ref().and_then(|pp| subs.iter().find(|p| &p.path == pp));
            match parent {
                Some(p) => BtrbkSubvolume {
                    name: s.name.clone(),
                    path: s.path.clone(),
                    nested_in: p.name.clone(),
                    inside: s.path.strip_prefix(&format!("{}/", p.path)).unwrap_or(&s.path).to_string(),
                    ..Default::default()
                },
                None => BtrbkSubvolume { name: s.name.clone(), path: s.path.clone(), mount: s.mount.clone(), ..Default::default() },
            }
        })
        .collect()
}

/// The snapshot name of a nested subvolume without a `btrbk.conf` section: its path from the
/// top level with `/` as `_` (`@home/games` → `@home_games`). Stable from run to run (a
/// basename would change with the other subvolumes that happen to exist), and distinct from
/// every other subvolume's path.
pub fn nested_name(path: &str) -> String {
    path.replace('/', "_")
}

/// The subvolumes to send from the PC's layout: the ones [`btrbk::layout`] sends, minus the
/// plan's excludes that name a mount point. Names: `btrbk.conf`'s for that subvolume (the
/// chain goes on), else the subvolume's own name (a second mounted one with the same name gets
/// its whole path, `/` as `_`), and for a nested one always its whole path ([`nested_name`]);
/// a nested one without a usable name is a notice, not a problem. Snapshot folder: `btrbk.conf`'s when it is an existing folder
/// on the same filesystem (under `root`), else [`AGENT_SNAPSHOT_DIR`]. Returns the problems
/// too (subvolumes that cannot be sent: the run fails), and the notices: plan excludes that
/// this method cannot apply (a path or pattern inside a sent subvolume: a snapshot is whole).
pub fn plan_subvolumes(l: &Layout, conf: &[ConfSubvolume], all: &[Mount], excludes: &[String], root: &Path) -> (Vec<Sub>, Vec<String>, Vec<String>) {
    let mut out: Vec<Sub> = Vec::new();
    let mut problems = Vec::new();
    let mut nested_notes: Vec<String> = Vec::new();
    let names_mount = |e: &str, m: &str| e.trim_end_matches('/') == m || (e == "/" && m == "/");
    for c in l.subvolumes.iter().filter(|c| c.skip.is_none()) {
        if excludes.iter().any(|e| names_mount(e, &c.mount)) {
            continue;
        }
        // a nested one goes only with its parent
        if let Some(pp) = &c.nested_in
            && !out.iter().any(|s| &s.path == pp)
        {
            nested_notes.push(format!("{} is nested in a subvolume that is not sent ({pp}): not sent either", c.mount));
            continue;
        }
        let from_conf = conf.iter().find(|x| btrbk::locate(&x.dir, all).is_some_and(|(d, p)| d == l.dev && p == c.path));
        let conf_name = from_conf.map(|x| x.name.clone()).filter(|n| valid_snapshot_prefix(n));
        let name = match (&c.nested_in, conf_name) {
            (_, Some(n)) => n,
            // nested: its whole path, which never changes from one run to the next
            (Some(_), None) => nested_name(&c.path),
            (None, None) => {
                let base = btrbk::basename(&c.path).to_string();
                if out.iter().any(|s| s.name == base) { c.path.replace('/', "_") } else { base }
            }
        };
        if !valid_snapshot_prefix(&name) || out.iter().any(|s| s.name == name) {
            let text = format!("{} ({}): no usable snapshot name ({name:?} is taken or not allowed)", c.mount, c.path);
            // a nested one is said on the run's root line; a mounted one fails the run
            if c.nested_in.is_some() {
                nested_notes.push(format!("{text}, not sent"));
            } else {
                problems.push(text);
            }
            continue;
        }
        let snap_dir = from_conf
            .map(|x| x.snapshot_dir.clone())
            .filter(|d| !d.is_empty() && root.join(d.trim_start_matches('/')).is_dir())
            .filter(|d| btrbk::locate(d, all).is_some_and(|(dev, _)| dev == l.dev))
            .unwrap_or_else(|| AGENT_SNAPSHOT_DIR.to_string());
        out.push(Sub { name, path: c.path.clone(), mount: c.mount.clone(), snap_dir, nested_in: c.nested_in.clone() });
    }
    // an exclude that is not a subvolume's mount point but lies in a sent one (or is a
    // pattern, which could): sent anyway, and said so
    let inside = |e: &str, m: &str| m == "/" || e == m || e.starts_with(&format!("{m}/"));
    let notices = excludes
        .iter()
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
        .filter(|e| !l.subvolumes.iter().any(|c| names_mount(e, &c.mount)))
        .filter(|e| !e.starts_with('/') || out.iter().any(|s| inside(e.trim_end_matches('/'), &s.mount)))
        .map(|e| format!("the exclusion {e} is not applied: the btrfs method sends whole subvolumes (only a subvolume's mount point can be left out)"))
        .chain(nested_notes)
        .collect();
    (out, problems, notices)
}

// ------------------------------------------------------------------ names and parents

/// A local snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Local {
    pub name: String,
    pub uuid: Option<String>,
    /// The UUID of the subvolume it is a snapshot of.
    pub parent_uuid: Option<String>,
    pub received_uuid: Option<String>,
    pub readonly: bool,
    /// Creation time (seconds). Names are local time, so after a change from summer time
    /// they can sort before older ones: the order of snapshots is (otime, name).
    pub otime: i64,
}

impl Local {
    pub fn of(name: String, i: btrfs::SubvolInfo) -> Local {
        Local { name, uuid: i.uuid, parent_uuid: i.parent_uuid, received_uuid: i.received_uuid, readonly: i.readonly, otime: i.otime }
    }

    /// The UUID `btrfs send -p` names in the stream for this parent (its received UUID when
    /// it was itself received, else its own), which the NAS keeps as `received_uuid`.
    pub fn send_uuid(&self) -> Option<&str> {
        self.received_uuid.as_deref().or(self.uuid.as_deref())
    }
}

/// Sort key of a `<prefix>.YYYYMMDDTHHMM[_N]` name of `prefix`: (timestamp, N).
fn order(prefix: &str, name: &str) -> Option<(String, u64)> {
    let (p, ts) = split_long_snapshot(name)?;
    if p != prefix {
        return None;
    }
    let (stamp, n) = match ts.split_once('_') {
        Some((s, n)) => (s, n.parse().ok()?),
        None => (ts, 0),
    };
    Some((stamp.to_string(), n))
}

/// The parent of the next incremental send of `prefix`: the newest local read-only snapshot
/// (by creation time, then name) whose send UUID is the `received_uuid` of a complete
/// snapshot of the same subvolume on the NAS. Returns it with the NAS' name of that copy,
/// which is what the NAS looks the parent up by (the two names can differ, e.g. a local
/// snapshot renamed, or a `_N` suffix given on one side only). The snapshot must also come
/// from `source`, the subvolume about to be sent ([`taken_of`]): a name alone never makes a
/// parent (no source known: no parent, a full send).
pub fn pick_parent<'a>(prefix: &str, locals: &'a [Local], nas: &AgentBtrfsListing, source: Option<&Local>) -> Option<(&'a Local, String)> {
    let source = source?;
    let nas_copy = |u: &str| {
        nas.snapshots
            .iter()
            .filter(|s| s.complete && s.received_uuid.as_deref() == Some(u))
            .find(|s| split_long_snapshot(&s.name).is_some_and(|(p, _)| p == prefix))
            .map(|s| s.name.clone())
    };
    locals
        .iter()
        .filter(|l| l.readonly && taken_of(l, source))
        .filter_map(|l| order(prefix, &l.name).map(|k| ((l.otime, k), l)))
        .filter_map(|(k, l)| l.send_uuid().and_then(nas_copy).map(|n| (k, l, n)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, l, n)| (l, n))
}

/// The local snapshot `l` is one of the subvolume `source`: a snapshot of it (its parent UUID
/// is the source's UUID), or the snapshot the source was made from (a restore makes the
/// subvolume as a writable snapshot of the received one).
pub fn taken_of(l: &Local, source: &Local) -> bool {
    let same = |a: &Option<String>, b: &Option<String>| a.is_some() && a == b;
    same(&l.parent_uuid, &source.uuid) || same(&source.parent_uuid, &l.uuid)
}

/// `<prefix>.<ts>`, or with btrbk's `_N` suffix when that name is taken (locally or on the
/// NAS).
pub fn unique_name(prefix: &str, ts: &str, taken: impl Fn(&str) -> bool) -> String {
    let base = format!("{prefix}.{ts}");
    if !taken(&base) {
        return base;
    }
    (1..).map(|n| format!("{base}_{n}")).find(|n| !taken(n)).unwrap_or(base)
}

/// Local snapshots of `prefix` to delete, from (name, creation time): all but the newest
/// `keep` (by creation time, then name), never one of `never` (the snapshot just sent, the
/// parent it was sent on), never a name that is not `<prefix>.<long timestamp>`.
pub fn to_delete(prefix: &str, snaps: &[(String, i64)], keep: usize, never: &[&str]) -> Vec<String> {
    type Keyed<'a> = ((i64, (String, u64)), &'a String);
    let mut mine: Vec<Keyed> = snaps.iter().filter_map(|(n, t)| order(prefix, n).map(|k| ((*t, k), n))).collect();
    mine.sort_by(|a, b| b.0.cmp(&a.0));
    let mut kept = 0;
    let mut out = Vec::new();
    for (_, n) in mine {
        if never.contains(&n.as_str()) || kept < keep {
            kept += 1;
        } else {
            out.push(n.clone());
        }
    }
    out
}

/// btrbk's `long` timestamp of `epoch_s` in the PC's local time.
pub fn local_timestamp(epoch_s: i64) -> String {
    // SAFETY: localtime_r fills the struct it is given
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t: libc::time_t = epoch_s as libc::time_t;
        libc::localtime_r(&t, &mut tm);
        tm
    };
    long_timestamp(tm.tm_year + 1900, (tm.tm_mon + 1) as u32, tm.tm_mday as u32, tm.tm_hour as u32, tm.tm_min as u32)
}

/// The snapshots of `prefix` in the folder `dir` (subvolumes only).
fn list_local(dir: &Path, prefix: &str) -> Vec<Local> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            order(prefix, &name)?;
            let i = btrfs::subvol_info(&e.path())?;
            Some(Local::of(name, i))
        })
        .collect()
}

// ------------------------------------------------------------------ the stream

/// `btrfs send` read as a stream: bytes counted, Cancel honoured, and the end only when the
/// process ended well (so a failed send never looks like a complete stream).
struct SendStream {
    child: Child,
    out: ChildStdout,
    err: Option<std::thread::JoinHandle<String>>,
    raw: Arc<AtomicU64>,
    progress: Option<Arc<Counters>>,
}

impl Read for SendStream {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        if cancelled() {
            return Err(std::io::Error::other("cancelled"));
        }
        let n = self.out.read(b)?;
        if n == 0 {
            let st = self.child.wait()?;
            if !st.success() {
                let e = self.err.take().and_then(|h| h.join().ok()).unwrap_or_default();
                return Err(std::io::Error::other(format!("btrfs send failed: {}", e.trim())));
            }
            return Ok(0);
        }
        self.raw.fetch_add(n as u64, Ordering::Relaxed);
        if let Some(c) = &self.progress {
            c.bytes_done.fetch_add(n as u64, Ordering::Relaxed);
        }
        Ok(n)
    }
}

impl Drop for SendStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Counted<R> {
    inner: R,
    n: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(b)?;
        self.n += n as u64;
        Ok(n)
    }
}

/// What one send did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub name: String,
    pub parent: Option<String>,
    pub raw: u64,
    pub compressed: u64,
}

/// A send that failed: what was tried (when it got that far), why in a code
/// ([`btrfs_step_error`] or the NAS' error code), and the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendError {
    pub name: Option<String>,
    pub parent: Option<String>,
    pub code: Option<String>,
    pub text: String,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// The step record of a subvolume for the report.
fn step_of(prefix: &str, r: &Result<Sent, SendError>) -> BtrfsStep {
    match r {
        Ok(s) => BtrfsStep { subvolume: prefix.into(), snapshot: s.name.clone(), parent: s.parent.clone(), ..Default::default() },
        Err(e) => BtrfsStep {
            subvolume: prefix.into(),
            snapshot: e.name.clone().unwrap_or_default(),
            parent: e.parent.clone(),
            error_code: e.code.clone(),
            error: Some(e.text.clone()),
        },
    }
}

/// A refusal or failure of the NAS: status, error code, text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Refused {
    status: u16,
    code: Option<String>,
    text: String,
}

/// The NAS' answer to a stream.
type Answer = Result<AgentBtrfsReceived, Refused>;

/// `btrfs send --proto 1` when this btrfs-progs knows the option (5.19 and later; older ones
/// only send v1 anyway). The NAS takes v1 streams only.
fn proto_args() -> &'static [&'static str] {
    static KNOWS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let knows = *KNOWS.get_or_init(|| {
        Command::new("btrfs")
            .args(["send", "--help"])
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("--proto") || String::from_utf8_lossy(&o.stderr).contains("--proto"))
    });
    if knows { &["--proto", "1"] } else { &[] }
}

/// Streams `btrfs send` of `snap` to the NAS: (raw bytes, compressed bytes, answer).
fn stream_to_nas(client: &Client, snap: &Path, parent: Option<&Path>, query: &str, progress: Option<Arc<Counters>>) -> Result<(u64, u64, Answer), String> {
    let mut cmd = Command::new("btrfs");
    cmd.arg("send").arg("-q").args(proto_args());
    if let Some(p) = parent {
        cmd.arg("-p").arg(p);
    }
    let mut child = cmd
        .arg(snap)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("btrfs send: {e} (install btrfs-progs)"))?;
    let out = child.stdout.take().ok_or("btrfs send: no output")?;
    let mut errp = child.stderr.take();
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(e) = errp.as_mut() {
            let _ = e.take(16 << 10).read_to_string(&mut s);
        }
        s
    });
    let raw = Arc::new(AtomicU64::new(0));
    let stream = SendStream { child, out, err: Some(err), raw: raw.clone(), progress };
    let mut enc = zstd::stream::read::Encoder::new(stream, ZSTD_LEVEL).map_err(|e| format!("zstd: {e}"))?;
    // the NAS decodes windows up to AGENT_ZSTD_WINDOW_LOG_MAX
    enc.window_log(AGENT_ZSTD_WINDOW_LOG).map_err(|e| format!("zstd: {e}"))?;
    let mut body = Counted { inner: enc, n: 0 };
    let r = client
        .upload(query, &[("Content-Type", "application/zstd"), ("Accept", "application/json")], &mut body, ANSWER_WAIT)
        .map_err(|e| format!("sending to {}: {e}", client.ep.url()))?;
    let (rawn, comp) = (raw.load(Ordering::Relaxed), body.n);
    if r.ok() {
        let got = serde_json::from_slice::<AgentBtrfsReceived>(&r.body).map_err(|e| format!("bad answer from the NAS: {e}"))?;
        return Ok((rawn, comp, Ok(got)));
    }
    Ok((rawn, comp, Err(Refused { status: r.status, code: r.error_code(), text: r.error_text() })))
}

/// Snapshot, parent, send and local keep for one subvolume. `root`: where the PC's paths
/// are (`/`).
///
/// - The parent is sent by the NAS' name of its copy; when the NAS no longer has it
///   (`parent_missing`, a retention pass came first) the same snapshot goes again in full.
/// - Whatever goes wrong (a refusal, a connection that broke after the NAS had finished, a
///   Cancel), the NAS' listing decides before the new local snapshot is deleted: when the
///   NAS has it complete, it stays (it is the next parent).
pub fn send_one(client: &Client, sub: &Sub, root: &Path, ts: &str, nas: &AgentBtrfsListing, progress: Option<Arc<Counters>>) -> Result<Sent, SendError> {
    let local_err = |text: String| SendError { name: None, parent: None, code: Some(btrfs_step_error::SNAPSHOT.into()), text };
    let dir: PathBuf = root.join(sub.snap_dir.trim_start_matches('/'));
    if !dir.is_dir() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| local_err(format!("{}: {e}", dir.display())))?;
    }
    let locals = list_local(&dir, &sub.name);
    let name = unique_name(&sub.name, ts, |n| locals.iter().any(|l| l.name == n) || dir.join(n).exists() || nas.snapshots.iter().any(|s| s.name == n));
    let source = root.join(sub.mount.trim_start_matches('/'));
    let source_info = btrfs::subvol_info(&source).map(|i| Local::of(String::new(), i));
    let mut parent = pick_parent(&sub.name, &locals, nas, source_info.as_ref()).map(|(l, nas_name)| (l.name.clone(), nas_name));
    let snap = dir.join(&name);
    btrfs::snapshot_ro(&source, &snap).map_err(|e| SendError { name: Some(name.clone()), ..local_err(e) })?;
    let attempt = |parent: Option<&(String, String)>, progress: Option<Arc<Counters>>| {
        if let Some(c) = &progress {
            if let Ok(mut m) = c.message.lock() {
                *m = Some(match parent {
                    Some((p, _)) => format!("{}: incremental from {p}", sub.name),
                    None => format!("{}: full", sub.name),
                });
            }
            if let Ok(mut s) = c.step.lock() {
                *s = Some(BtrfsStep { subvolume: sub.name.clone(), snapshot: name.clone(), parent: parent.map(|p| p.0.clone()), ..Default::default() });
            }
        }
        let query = format!("{AGENT_BTRFS_RECEIVE}?snapshot={name}&parent={}", parent.map(|p| p.1.as_str()).unwrap_or("none"));
        let parent_path = parent.map(|p| dir.join(&p.0));
        stream_to_nas(client, &snap, parent_path.as_deref(), &query, progress)
    };
    let mut result = attempt(parent.as_ref(), progress.clone());
    if parent.is_some()
        && matches!(&result, Ok((_, _, Err(r))) if r.status == 409 && r.code.as_deref() == Some(PARENT_MISSING))
        && !cancelled()
    {
        say(format!("{}: the NAS no longer has the parent {}: sending {name} in full", sub.name, parent.as_ref().map(|p| p.1.as_str()).unwrap_or("")));
        parent = None;
        result = attempt(None, progress);
    }
    let result = match result {
        Ok((raw, comp, Ok(_))) => Ok((raw, comp)),
        Ok((_, _, Err(r))) => Err((r.code, r.text)),
        Err(e) => {
            let code = if cancelled() { btrfs_step_error::CANCELLED } else { btrfs_step_error::TRANSPORT };
            Err((Some(code.to_string()), e))
        }
    };
    // the NAS' word on what it has, whatever happened above
    let result = result.or_else(|e| match client.get_json::<AgentBtrfsListing>(AGENT_BTRFS_SNAPSHOTS) {
        Ok(again) if again.snapshots.iter().any(|s| s.name == name && s.complete) => {
            say(format!("{}: {}; the NAS has {name} complete all the same", sub.name, e.1));
            Ok((0, 0))
        }
        _ => Err(e),
    });
    match result {
        Ok((raw, compressed)) => {
            let mut snaps: Vec<(String, i64)> = locals.iter().map(|l| (l.name.clone(), l.otime)).collect();
            snaps.push((name.clone(), i64::MAX));
            let mut never = vec![name.as_str()];
            if let Some((p, _)) = &parent {
                never.push(p.as_str());
            }
            for old in to_delete(&sub.name, &snaps, KEEP_LOCAL, &never) {
                if let Err(e) = btrfs::delete(&dir.join(&old)) {
                    say(format!("local snapshot {old}: {e}"));
                }
            }
            Ok(Sent { name, parent: parent.map(|p| p.0), raw, compressed })
        }
        Err((code, text)) => {
            // not on the NAS: useless as a parent
            if let Err(d) = btrfs::delete(&snap) {
                say(format!("{}: {d}", snap.display()));
            }
            Err(SendError { name: Some(name), parent: parent.map(|p| p.0), code, text })
        }
    }
}

// ------------------------------------------------------------------ the run

/// `btrbk.timer` is active: btrbk would take and prune snapshots of the same chain.
fn btrbk_timer_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "btrbk.timer"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn fail(e: impl Into<String>, captures: Vec<Capture>, man: Option<Manifest>) -> Failure {
    Box::new((e.into(), captures, man))
}

/// One run of the btrfs method (the service's child or `rynas-agent backup`).
pub fn run(st: &crate::state::State, client: &Arc<Client>, counters: &Arc<Counters>, set_phase: &dyn Fn(AgentPhase), run_id: &str, started_ms: u64) -> Result<(PcRun, Option<Manifest>), Failure> {
    install_cancel();
    let root = Path::new("/");
    let _lock = manifest::lock(root, Duration::from_secs(60)).map_err(|e| fail(e, Vec::new(), None))?;
    if btrbk_timer_active() {
        return Err(fail(
            "btrbk.timer is active: btrbk and rynas-agent would both take and delete snapshots of the same chain. Turn it off (systemctl disable --now btrbk.timer) to use the agent's btrfs method",
            Vec::new(),
            None,
        ));
    }
    set_phase(AgentPhase::Snapshot);
    crate::bootcopy::trigger_automounts(root);
    let all = mounts::read();
    let mut layout = btrbk::layout(&all).map_err(|e| fail(e, Vec::new(), None))?;
    let conf = match btrbk::read_conf(root) {
        Ok(c) => c.map(|t| btrbk::parse_conf(&t)).unwrap_or_default(),
        Err(e) => {
            say(format!("{e}: btrbk's names and snapshot folder are not used"));
            Vec::new()
        }
    };
    // the subvolumes nested in the sent ones without a mount of their own
    let snap_dirs: Vec<String> = std::iter::once(AGENT_SNAPSHOT_DIR.to_string()).chain(conf.iter().map(|c| c.snapshot_dir.clone())).collect();
    let mount_points: Vec<String> = all.iter().map(|m| m.path.clone()).collect();
    let (nested, nested_notes) = nested_candidates(&layout, &snap_dirs, &st.plan.plan.excludes, &mount_points, &|mount| list_nested(root, mount, &mount_points));
    layout.subvolumes.extend(nested);
    let (subs, problems, mut notices) = plan_subvolumes(&layout, &conf, &all, &st.plan.plan.excludes, root);
    notices.extend(nested_notes);
    for n in &notices {
        say(n.clone());
    }
    if subs.is_empty() {
        return Err(fail("no subvolume to send", Vec::new(), None));
    }
    // the manifest first: the root snapshot carries it
    let planned: BTreeMap<String, CaptureMethod> = subs.iter().filter(|s| s.nested_in.is_none()).map(|s| (s.mount.clone(), CaptureMethod::BtrfsSnapshot)).collect();
    let mut man = manifest::collect(&planned);
    let snapshot_dir = subs.iter().find(|s| s.mount == "/").unwrap_or(&subs[0]).snap_dir.clone();
    let block = BtrbkLayout {
        snapshot_dir,
        subvolumes: manifest_subvolumes(&subs),
        boot_copies: Vec::new(),
        agent: true,
    };
    for p in manifest::add_block(&mut man, &all, block, root, true) {
        say(format!("manifest: {p}"));
    }
    if let Err(e) = manifest::write(&man, root) {
        say(format!("manifest: {e}"));
    }
    // SAFETY: plain syscall (the manifest on disk before the snapshots)
    unsafe { libc::sync() };
    let nas: AgentBtrfsListing = client.get_json(AGENT_BTRFS_SNAPSHOTS).map_err(|e| fail(e, Vec::new(), Some(man.clone())))?;
    if nas.receiving {
        return Err(fail("the NAS is still receiving a snapshot of this PC", Vec::new(), Some(man)));
    }
    let ts = local_timestamp((now_ms() / 1000) as i64);
    set_phase(AgentPhase::Uploading);
    let mut captures = Vec::new();
    let mut errors: Vec<String> = problems.clone();
    let (mut raw, mut comp) = (0, 0);
    for s in &subs {
        if cancelled() {
            errors.push("cancelled".into());
            break;
        }
        let sent = send_one(client, s, root, &ts, &nas, Some(counters.clone()));
        let step = step_of(&s.name, &sent);
        let note = match sent {
            Ok(sent) => {
                raw += sent.raw;
                comp += sent.compressed;
                say(format!("{}: {} sent ({} bytes, {} compressed)", s.mount, sent.name, sent.raw, sent.compressed));
                match &sent.parent {
                    Some(p) => format!("{}: incremental from {p}", sent.name),
                    None => format!("{}: full", sent.name),
                }
            }
            Err(e) => {
                say(format!("{}: {e}", s.mount));
                errors.push(format!("{}: {e}", s.name));
                format!("error: {e}")
            }
        };
        captures.push(Capture { path: s.mount.clone(), fstype: "btrfs".into(), method: CaptureMethod::BtrfsSnapshot, note: Some(note), btrfs: Some(step) });
    }
    set_phase(AgentPhase::Finishing);
    // the excludes this method could not apply, on the root's line (where the NAS shows it)
    let at = captures.iter().position(|c| c.path == "/").or((!captures.is_empty()).then_some(0));
    if !notices.is_empty()
        && let Some(c) = at.and_then(|i| captures.get_mut(i))
    {
        let extra = notices.join("; ");
        c.note = Some(match c.note.take() {
            Some(n) => format!("{n}; {extra}"),
            None => extra,
        });
    }
    if !errors.is_empty() {
        return Err(fail(errors.join("; "), captures, Some(man)));
    }
    let run = PcRun {
        run_id: run_id.to_string(),
        started_ms,
        ended_ms: now_ms(),
        ok: true,
        snapshot: Some(ts),
        bytes_read: raw,
        bytes_added: comp,
        captures,
        ..Default::default()
    };
    Ok((run, Some(man)))
}

/// `rynas-agent btrfs-send` (hidden, tests): snapshot → parent → send → keep for the given
/// subvolumes only, against a NAS given on the command line; no manifest, no lock.
pub fn test_command(client: &Client, snap_dir: &str, subvols: &[(String, String)]) -> Result<(), String> {
    install_cancel();
    let nas: AgentBtrfsListing = client.get_json(AGENT_BTRFS_SNAPSHOTS)?;
    let ts = local_timestamp((now_ms() / 1000) as i64);
    let mut failed = 0;
    for (name, path) in subvols {
        if !valid_snapshot_prefix(name) {
            return Err(format!("{name}: not a snapshot name"));
        }
        let sub = Sub { name: name.clone(), path: String::new(), mount: path.clone(), snap_dir: snap_dir.to_string(), nested_in: None };
        match send_one(client, &sub, Path::new("/"), &ts, &nas, None) {
            Ok(s) => println!(
                "{name}: {} sent ({}), {} bytes, {} compressed",
                s.name,
                s.parent.map(|p| format!("incremental from {p}")).unwrap_or_else(|| "full".into()),
                s.raw,
                s.compressed
            ),
            Err(e) => {
                failed += 1;
                eprintln!("{name}: {e}");
            }
        }
    }
    if failed > 0 { Err(format!("{failed} of {} failed", subvols.len())) } else { Ok(()) }
}

/// `rynas-agent btrfs-nested` (hidden, tests): what a run would send of the subvolumes nested
/// in the given ones (`(path from the top level, where it is)`), with the run's rules, names
/// and notes, printed as `send <name> <path> <place>` and `note: …` lines. The places are
/// below `root` (a throwaway tree standing for `/`; the mount table is read relative to it).
pub fn nested_test_command(root: &Path, parents: &[(String, String)]) -> Result<(), String> {
    let all = mounts::read();
    let layout = Layout {
        dev: String::new(),
        subvolumes: parents.iter().map(|(p, m)| btrbk::Candidate { path: p.clone(), mount: m.clone(), skip: None, nested_in: None }).collect(),
    };
    let base = root.to_string_lossy().trim_end_matches('/').to_string();
    let mount_points: Vec<String> = all.iter().filter_map(|m| if base.is_empty() { Some(m.path.clone()) } else { m.path.strip_prefix(&base).filter(|r| r.starts_with('/')).map(str::to_string) }).collect();
    let (nested, mut notes) = nested_candidates(&layout, &[AGENT_SNAPSHOT_DIR.to_string()], &[], &mount_points, &|m| list_nested(root, m, &mount_points));
    let mut l = layout.clone();
    l.subvolumes.extend(nested);
    let (subs, problems, notices) = plan_subvolumes(&l, &[], &all, &[], root);
    notes.extend(notices);
    for s in subs.iter().filter(|s| s.nested_in.is_some()) {
        println!("send {} {} {}", s.name, s.path, s.mount);
    }
    for n in notes.iter().chain(&problems) {
        println!("note: {n}");
    }
    if problems.is_empty() { Ok(()) } else { Err(problems.join("; ")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(name: &str, received: Option<&str>) -> AgentBtrfsSnapshot {
        AgentBtrfsSnapshot { name: name.into(), received_uuid: received.map(Into::into), complete: received.is_some() }
    }

    fn local(name: &str, uuid: &str, received: Option<&str>, ro: bool) -> Local {
        Local { name: name.into(), uuid: Some(uuid.into()), parent_uuid: Some("src".into()), received_uuid: received.map(Into::into), readonly: ro, otime: 0 }
    }

    /// The subvolume being sent (the `local` snapshots are of it).
    fn src() -> Local {
        Local { uuid: Some("src".into()), ..Default::default() }
    }

    fn picked(prefix: &str, locals: &[Local], nas: &AgentBtrfsListing) -> Option<(String, String)> {
        pick_parent(prefix, locals, nas, Some(&src())).map(|(l, n)| (l.name.clone(), n))
    }

    fn same(n: &str) -> Option<(String, String)> {
        Some((n.to_string(), n.to_string()))
    }

    #[test]
    fn parent_is_the_newest_local_snapshot_the_nas_has() {
        let nas = AgentBtrfsListing {
            snapshots: vec![snap("@home.20261111T0300", Some("u1")), snap("@home.20261112T0300", Some("u2")), snap("@home.20261112T0400", None)],
            receiving: false,
        };
        let locals = vec![
            local("@home.20261111T0300", "u1", None, true),
            local("@home.20261112T0300", "u2", None, true),
            // newer, but the NAS has it only interrupted
            local("@home.20261112T0400", "u3", None, true),
            // another subvolume with the same UUID text never counts
            local("@.20261113T0300", "u2", None, true),
        ];
        assert_eq!(picked("@home", &locals, &nas), same("@home.20261112T0300"));
        // writable snapshots are never parents
        let w = vec![local("@home.20261112T0300", "u2", None, false)];
        assert!(pick_parent("@home", &w, &nas, Some(&src())).is_none());
        // a snapshot received on this PC (after a restore) is matched by its received UUID
        let restored = vec![local("@home.20261112T0300", "new-local-uuid", Some("u2"), true)];
        assert_eq!(picked("@home", &restored, &nas), same("@home.20261112T0300"));
        // `_N` sorts after the plain name of the same minute
        let nas2 = AgentBtrfsListing { snapshots: vec![snap("a.20261112T0300", Some("x")), snap("a.20261112T0300_1", Some("y"))], receiving: false };
        let l2 = vec![local("a.20261112T0300_1", "y", None, true), local("a.20261112T0300", "x", None, true)];
        assert_eq!(picked("a", &l2, &nas2), same("a.20261112T0300_1"));
        // nothing in common: a full send
        assert!(pick_parent("@home", &locals, &AgentBtrfsListing::default(), Some(&src())).is_none());
    }

    #[test]
    fn the_parent_goes_by_the_nas_name_and_by_creation_time() {
        // the NAS keeps the copy under another name: that name is what the NAS looks up
        let nas = AgentBtrfsListing { snapshots: vec![snap("@home.20261112T0300_1", Some("u2"))], receiving: false };
        let l = vec![local("@home.20261112T0300", "u2", None, true)];
        assert_eq!(picked("@home", &l, &nas), Some(("@home.20261112T0300".into(), "@home.20261112T0300_1".into())));
        // a copy of another subvolume with that UUID is not this chain's
        let other = AgentBtrfsListing { snapshots: vec![snap("@.20261112T0300", Some("u2"))], receiving: false };
        assert!(pick_parent("@home", &l, &other, Some(&src())).is_none());
        // end of summer time: 02:15 (winter time) was taken after 02:30 (summer time)
        let nas = AgentBtrfsListing { snapshots: vec![snap("@.20261025T0230", Some("a")), snap("@.20261025T0215", Some("b"))], receiving: false };
        let mut first = local("@.20261025T0230", "a", None, true);
        first.otime = 1_793_000_000;
        let mut later = local("@.20261025T0215", "b", None, true);
        later.otime = 1_793_002_700;
        assert_eq!(picked("@", &[first.clone(), later.clone()], &nas), same("@.20261025T0215"));
        // and the local keep goes by the same order
        let snaps = vec![(first.name.clone(), first.otime), (later.name.clone(), later.otime), ("@.20261025T0300".to_string(), i64::MAX)];
        assert_eq!(to_delete("@", &snaps, 2, &["@.20261025T0300"]), ["@.20261025T0230"]);
    }

    #[test]
    fn names_get_btrbk_suffixes() {
        assert_eq!(unique_name("@", "20261112T0300", |_| false), "@.20261112T0300");
        let taken = ["@.20261112T0300", "@.20261112T0300_1"];
        assert_eq!(unique_name("@", "20261112T0300", |n| taken.contains(&n)), "@.20261112T0300_2");
        let ts = local_timestamp(1_762_905_600);
        assert!(split_long_snapshot(&format!("@.{ts}")).is_some(), "{ts}");
    }

    #[test]
    fn steps_carry_fields_for_the_interface() {
        let ok = Ok(Sent { name: "@.20261112T0300".into(), parent: Some("@.20261111T0300".into()), raw: 1, compressed: 1 });
        let s = step_of("@", &ok);
        assert_eq!((s.subvolume.as_str(), s.snapshot.as_str(), s.parent.as_deref(), s.error_code), ("@", "@.20261112T0300", Some("@.20261111T0300"), None));
        let full = Ok(Sent { name: "@.20261112T0300".into(), parent: None, raw: 1, compressed: 1 });
        assert!(step_of("@", &full).parent.is_none());
        let err: Result<Sent, SendError> = Err(SendError { name: Some("@.20261112T0300".into()), parent: None, code: Some("busy".into()), text: "HTTP 409: …".into() });
        let s = step_of("@", &err);
        assert_eq!((s.error_code.as_deref(), s.error.as_deref()), (Some("busy"), Some("HTTP 409: …")));
    }

    #[test]
    fn keep_policy_never_deletes_the_parent() {
        let snaps: Vec<(String, i64)> = ["@.20261110T0300", "@.20261111T0300", "@.20261112T0300", "@.20261113T0300", "@home.20261101T0300", "@.manual", "@.20261109T0300+0100"]
            .iter()
            .map(|s| (s.to_string(), 0))
            .collect();
        assert_eq!(to_delete("@", &snaps, 2, &["@.20261113T0300"]), ["@.20261111T0300", "@.20261110T0300"]);
        // the parent kept even when it is not among the newest
        assert_eq!(to_delete("@", &snaps, 1, &["@.20261110T0300"]), ["@.20261112T0300", "@.20261111T0300"]);
        assert_eq!(to_delete("@", &snaps, 1, &["@.20261113T0300", "@.20261110T0300"]), ["@.20261112T0300", "@.20261111T0300"]);
        assert!(to_delete("@", &snaps, 5, &["@.20261113T0300"]).is_empty());
    }

    const MI: &str = "22 1 0:25 /@ / rw shared:1 - btrfs /dev/vda2 rw,subvolid=256,subvol=/@
40 22 0:25 /@home /home rw shared:2 - btrfs /dev/vda2 rw,subvolid=257,subvol=/@home
41 22 0:25 /a/home /srv/a rw shared:3 - btrfs /dev/vda2 rw,subvolid=258,subvol=/a/home
42 22 0:25 /b/home /srv/b rw shared:4 - btrfs /dev/vda2 rw,subvolid=259,subvol=/b/home
43 22 0:25 /@data /data rw shared:5 - btrfs /dev/vda2 rw,subvolid=260,subvol=/@data
44 22 0:25 / /mnt/pool rw shared:6 - btrfs /dev/vda2 rw,subvolid=5,subvol=/
";

    #[test]
    fn subvolumes_names_and_folders() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        std::fs::create_dir_all(root.join("mnt/pool/btrbk_snapshots")).unwrap();
        let all = mounts::parse(MI);
        let l = btrbk::layout(&all).unwrap();
        // btrbk's name and folder for @home (the chain goes on), excluded /data
        let conf = btrbk::parse_conf("volume /mnt/pool\n  snapshot_dir btrbk_snapshots\n  target ssh://nas/x\n  subvolume @home\n    snapshot_name home\n");
        let (subs, problems, notices) = plan_subvolumes(&l, &conf, &all, &["/data/".into()], root);
        assert!(problems.is_empty() && notices.is_empty(), "{problems:?} {notices:?}");
        let got: Vec<_> = subs.iter().map(|s| (s.name.as_str(), s.path.as_str(), s.mount.as_str(), s.snap_dir.as_str())).collect();
        assert_eq!(
            got,
            [
                ("@", "@", "/", AGENT_SNAPSHOT_DIR),
                ("home", "@home", "/home", "/mnt/pool/btrbk_snapshots"),
                // `home` is taken by @home (btrbk's name): the others get their whole path
                ("a_home", "a/home", "/srv/a", AGENT_SNAPSHOT_DIR),
                ("b_home", "b/home", "/srv/b", AGENT_SNAPSHOT_DIR),
            ]
        );
        // a btrbk folder that does not exist (pool not mounted) falls back to the default
        let t2 = tempfile::tempdir().unwrap();
        let (subs, _, _) = plan_subvolumes(&l, &conf, &all, &[], t2.path());
        assert_eq!(subs[1].snap_dir, AGENT_SNAPSHOT_DIR);
        // a name that cannot be made unique is reported, not sent
        let clash = btrbk::parse_conf("subvolume /srv/a\n  snapshot_name a_home\n  target ssh://n/x\nsubvolume /home\n  snapshot_name a_home\n  target ssh://n/x\n");
        let (subs, problems, _) = plan_subvolumes(&l, &clash, &all, &[], root);
        assert_eq!(problems.len(), 1, "{subs:?}");
        assert!(problems[0].contains("no usable snapshot name"));
        // excludes inside a sent subvolume (or patterns) cannot be applied: said, not silent;
        // a subvolume's mount point is applied
        let ex: Vec<String> = ["/home/me/.cache", "*.iso", "/srv/a/", "/srv/b/tmp"].iter().map(|s| s.to_string()).collect();
        let (subs, problems, notices) = plan_subvolumes(&l, &[], &all, &ex, root);
        assert!(problems.is_empty());
        assert!(!subs.iter().any(|s| s.mount == "/srv/a"));
        assert_eq!(notices.len(), 3, "{notices:?}");
        assert!(notices.iter().all(|n| n.contains("not applied")));
        assert!(notices[0].contains("/home/me/.cache") && notices[1].contains("*.iso") && notices[2].contains("/srv/b/tmp"));
    }

    #[test]
    fn nested_subvolumes_without_a_mount_are_sent_as_their_own_groups() {
        let t = tempfile::tempdir().unwrap();
        let all = mounts::parse(MI);
        let l = btrbk::layout(&all).unwrap();
        let mount_points: Vec<String> = all.iter().map(|m| m.path.clone()).collect();
        let list = |mount: &str| -> Result<Vec<Found>, String> {
            match mount {
                "/" => Ok(vec![
                    f("var/lib/machines"),
                    f(".btrbk_snapshots"),
                    f(".snapshots/1/snapshot"),
                    f("var/lib/docker/btrfs/subvolumes/abc"),
                    f("var/cache/thing"),
                    Found { received: true, ..f("backup/old") },
                    Found { readonly: true, ..f("frozen") },
                    f("frozen/inner"),
                    f("bad name"),
                ]),
                "/home" => Ok(vec![f("games"), f("games/deep"), f("skipme"), f("skipme/below")]),
                "/srv/a" => Err("permission denied".into()),
                _ => Ok(Vec::new()),
            }
        };
        let excludes = vec!["/home/skipme".to_string()];
        let (nested, notes) = nested_candidates(&l, &[AGENT_SNAPSHOT_DIR.into()], &excludes, &mount_points, &list);
        let got: Vec<_> = nested.iter().filter(|c| c.skip.is_none()).map(|c| (c.path.as_str(), c.mount.as_str(), c.nested_in.as_deref().unwrap())).collect();
        // the plan's exclusion: kept as a skipped candidate (nothing below it)
        let skipped: Vec<_> = nested.iter().filter(|c| c.skip.is_some()).map(|c| c.mount.as_str()).collect();
        assert_eq!(skipped, ["/home/skipme"]);
        assert_eq!(
            got,
            [("@/var/lib/machines", "/var/lib/machines", "@"), ("@home/games", "/home/games", "@home"), ("@home/games/deep", "/home/games/deep", "@home/games")]
        );
        // a failed listing, a read-only subvolume and an unusable name are said, never silent
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("/frozen") && n.contains("read-only")));
        assert!(notes.iter().any(|n| n.contains("/bad name")));
        assert!(notes.iter().any(|n| n.contains("/srv/a") && n.contains("could not be listed")));
        // into the plan: stable names (the whole path), the parent kept in the manifest
        let mut l2 = l.clone();
        l2.subvolumes.extend(nested.clone());
        let (subs, problems, notices) = plan_subvolumes(&l2, &[], &all, &excludes, t.path());
        assert!(problems.is_empty(), "{problems:?}");
        assert!(notices.is_empty(), "the exclusion of a nested place is applied: {notices:?}");
        let games = subs.iter().find(|s| s.path == "@home/games").unwrap();
        assert_eq!((games.name.as_str(), games.mount.as_str(), games.snap_dir.as_str()), ("@home_games", "/home/games", AGENT_SNAPSHOT_DIR));
        let man = manifest_subvolumes(&subs);
        let g = man.iter().find(|s| s.name == "@home_games").unwrap();
        assert_eq!((g.mount.as_str(), g.nested_in.as_str(), g.inside.as_str()), ("", "@home", "games"));
        let d = man.iter().find(|s| s.name == "@home_games_deep").unwrap();
        assert_eq!((d.nested_in.as_str(), d.inside.as_str()), ("@home_games", "deep"));
        let b = BtrbkLayout { subvolumes: man.clone(), ..Default::default() };
        assert_eq!(b.place_of(d).as_deref(), Some("/home/games/deep"));
        // the parent left out by the plan: the nested ones go with it, said
        let (subs, _, notices) = plan_subvolumes(&l2, &[], &all, &["/home".into()], t.path());
        assert!(!subs.iter().any(|s| s.path.starts_with("@home")));
        assert_eq!(notices.iter().filter(|n| n.contains("nested in a subvolume that is not sent")).count(), 2, "{notices:?}");
    }

    fn f(rel: &str) -> Found {
        Found { rel: rel.into(), readonly: false, received: false, unlisted: None }
    }

    /// Item: a child that cannot be listed, and a nested subvolume under another mount.
    #[test]
    fn nested_listing_errors_and_hidden_places_are_said_and_the_rest_goes_on() {
        let all = mounts::parse(&format!("{MI}50 40 0:60 / /home/me/mnt rw shared:9 - tmpfs tmpfs rw\n"));
        let l = btrbk::layout(&all).unwrap();
        let mount_points: Vec<String> = all.iter().map(|m| m.path.clone()).collect();
        let list = |mount: &str| -> Result<Vec<Found>, String> {
            match mount {
                "/home" => Ok(vec![
                    Found { unlisted: Some("No such file".into()), ..f("vms") },
                    f("games"),
                    // reached through /home/me/mnt (a tmpfs over the folder): not this subvolume
                    f("me/mnt/data"),
                ]),
                _ => Ok(Vec::new()),
            }
        };
        let (nested, notes) = nested_candidates(&l, &[], &[], &mount_points, &list);
        let sent: Vec<_> = nested.iter().filter(|c| c.skip.is_none()).map(|c| c.mount.as_str()).collect();
        // the failing child is still sent, and so are its siblings
        assert_eq!(sent, ["/home/games", "/home/vms"]);
        assert!(notes.iter().any(|n| n.starts_with("/home/vms: ") && n.contains("could not be listed")), "{notes:?}");
        assert!(notes.iter().any(|n| n.starts_with("/home/me/mnt/data: ") && n.contains("/home/me/mnt")), "{notes:?}");
        assert_eq!(notes.len(), 2, "{notes:?}");
        // the mount of the parent itself never hides anything
        assert!(hidden_by("/home", "/home/games", &mount_points).is_none());
        assert_eq!(hidden_by("/", "/home/x", &mount_points), Some("/home"));
    }

    /// Item: names a nested subvolume cannot have are notes on the root line, never errors
    /// that fail every run; a mounted one with no name still fails it.
    #[test]
    fn a_nested_subvolume_without_a_usable_name_is_a_note() {
        let t = tempfile::tempdir().unwrap();
        let all = mounts::parse(MI);
        let mut l = btrbk::layout(&all).unwrap();
        let long = format!("@home/{}", "x".repeat(70));
        for p in ["@home/Giochi_è", "@home/vm+win", long.as_str(), "@home/ok"] {
            l.subvolumes.push(btrbk::Candidate { path: p.into(), mount: format!("/home/{}", &p[6..]), skip: None, nested_in: Some("@home".into()) });
        }
        let (subs, problems, notices) = plan_subvolumes(&l, &[], &all, &[], t.path());
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(notices.iter().filter(|n| n.contains("no usable snapshot name") && n.ends_with("not sent")).count(), 3, "{notices:?}");
        assert!(subs.iter().any(|s| s.name == "@home_ok"));
        // a btrbk.conf section for the nested path gives its name (the chain goes on)
        let conf = btrbk::parse_conf("subvolume /home/ok\n  snapshot_name games\n  target ssh://n/x\n");
        let (subs, _, _) = plan_subvolumes(&l, &conf, &all, &[], t.path());
        assert!(subs.iter().any(|s| s.path == "@home/ok" && s.name == "games"));
    }

    /// Item: a folder the plan leaves out takes the nested subvolumes inside it along.
    #[test]
    fn an_excluded_folder_leaves_out_the_nested_subvolumes_inside_it() {
        let all = mounts::parse(MI);
        let l = btrbk::layout(&all).unwrap();
        let list = |mount: &str| -> Result<Vec<Found>, String> { if mount == "/home" { Ok(vec![f("me/vms/win10"), f("me/vms/win10/disk"), f("me/vmsx")]) } else { Ok(Vec::new()) } };
        let (nested, _) = nested_candidates(&l, &[], &["/home/me/vms/".into(), "*.iso".into()], &[], &list);
        let r: Vec<_> = nested.iter().map(|c| (c.mount.as_str(), c.skip.is_some())).collect();
        assert_eq!(r, [("/home/me/vmsx", false), ("/home/me/vms/win10", true)]);
    }

    /// Item: rootless engines' layers under a home, Incus/LXD pools: never sent.
    #[test]
    fn container_storage_anywhere_is_left_out() {
        for p in [
            "home/me/.local/share/docker/btrfs/subvolumes/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "me/.local/share/containers/storage/btrfs/subvolumes/abc",
            "var/lib/incus/storage-pools/default/containers/c1",
            "var/snap/lxd/common/lxd/storage-pools/default/images/abc",
            "var/lib/docker/btrfs/subvolumes/x",
        ] {
            assert!(btrfs::excluded_subvolume(p), "{p}");
            assert!(!btrfs::recreated_on_restore(p), "{p}");
        }
        assert!(!btrfs::excluded_subvolume("home/me/docker-notes/btrfs"));
        assert!(btrfs::recreated_on_restore(".snapshots"));
        let all = mounts::parse(MI);
        let l = btrbk::layout(&all).unwrap();
        let list = |mount: &str| -> Result<Vec<Found>, String> {
            if mount == "/home" { Ok(vec![f("me/.local/share/docker/btrfs/subvolumes/abc"), f("me/.local/share/containers/storage/btrfs/subvolumes/x")]) } else { Ok(Vec::new()) }
        };
        let (nested, notes) = nested_candidates(&l, &[], &[], &[], &list);
        assert!(nested.is_empty() && notes.is_empty(), "{nested:?} {notes:?}");
    }

    /// Item: a parent is a snapshot of the subvolume being sent, whatever its name says.
    #[test]
    fn the_parent_must_come_from_the_same_subvolume() {
        let nas = AgentBtrfsListing { snapshots: vec![snap("games.20261112T0300", Some("u1"))], receiving: false };
        let mut other = local("games.20261112T0300", "u1", None, true);
        other.parent_uuid = Some("another-subvolume".into());
        assert!(pick_parent("games", std::slice::from_ref(&other), &nas, Some(&src())).is_none());
        // unknown source: no parent
        assert!(pick_parent("games", &[local("games.20261112T0300", "u1", None, true)], &nas, None).is_none());
        // after a restore: the subvolume is a writable snapshot of the received one
        let received = Local { parent_uuid: None, ..local("games.20261112T0300", "local-r", Some("u1"), true) };
        let restored = Local { uuid: Some("new".into()), parent_uuid: Some("local-r".into()), ..Default::default() };
        assert_eq!(pick_parent("games", std::slice::from_ref(&received), &nas, Some(&restored)).map(|(l, n)| (l.name.clone(), n)), same("games.20261112T0300"));
        assert!(pick_parent("games", &[received], &nas, Some(&src())).is_none());
    }
}
