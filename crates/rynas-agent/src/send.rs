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
//! (inside the root subvolume, so its snapshot carries it) → per subvolume: snapshot,
//! parent, send, keep. The pure parts ([`plan_subvolumes`], [`pick_parent`],
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

/// SIGTERM/SIGINT (the service's Cancel) stop the current send at the next read: the new
/// local snapshot is deleted, the NAS discards the broken stream.
fn install_cancel() {
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
    /// Where it is mounted: the snapshot's source.
    pub mount: String,
    /// The local snapshot folder, absolute on the PC.
    pub snap_dir: String,
}

fn basename(p: &str) -> &str {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or(p)
}

/// The subvolumes to send from the PC's layout: the ones [`btrbk::layout`] sends, minus the
/// plan's excludes that name a mount point. Names: `btrbk.conf`'s for that subvolume (the
/// chain goes on), else the subvolume's own name; a second subvolume with the same name gets
/// its whole path (`/` as `_`). Snapshot folder: `btrbk.conf`'s when it is an existing folder
/// on the same filesystem (under `root`), else [`AGENT_SNAPSHOT_DIR`]. Returns the problems
/// too (subvolumes that cannot be sent).
pub fn plan_subvolumes(l: &Layout, conf: &[ConfSubvolume], all: &[Mount], excludes: &[String], root: &Path) -> (Vec<Sub>, Vec<String>) {
    let mut out: Vec<Sub> = Vec::new();
    let mut problems = Vec::new();
    for c in l.subvolumes.iter().filter(|c| c.skip.is_none()) {
        if excludes.iter().any(|e| e.trim_end_matches('/') == c.mount || (e == "/" && c.mount == "/")) {
            continue;
        }
        let from_conf = conf.iter().find(|x| btrbk::locate(&x.dir, all).is_some_and(|(d, p)| d == l.dev && p == c.path));
        let mut name = from_conf.map(|x| x.name.clone()).filter(|n| valid_snapshot_prefix(n)).unwrap_or_else(|| basename(&c.path).to_string());
        if out.iter().any(|s| s.name == name) {
            name = c.path.replace('/', "_");
        }
        if !valid_snapshot_prefix(&name) || out.iter().any(|s| s.name == name) {
            problems.push(format!("{} ({}): no usable snapshot name ({name:?} is taken or not allowed)", c.mount, c.path));
            continue;
        }
        let snap_dir = from_conf
            .map(|x| x.snapshot_dir.clone())
            .filter(|d| !d.is_empty() && root.join(d.trim_start_matches('/')).is_dir())
            .filter(|d| btrbk::locate(d, all).is_some_and(|(dev, _)| dev == l.dev))
            .unwrap_or_else(|| AGENT_SNAPSHOT_DIR.to_string());
        out.push(Sub { name, path: c.path.clone(), mount: c.mount.clone(), snap_dir });
    }
    (out, problems)
}

// ------------------------------------------------------------------ names and parents

/// A local snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Local {
    pub name: String,
    pub uuid: Option<String>,
    pub received_uuid: Option<String>,
    pub readonly: bool,
}

impl Local {
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
/// whose send UUID is the `received_uuid` of a complete snapshot on the NAS.
pub fn pick_parent<'a>(prefix: &str, locals: &'a [Local], nas: &AgentBtrfsListing) -> Option<&'a Local> {
    locals
        .iter()
        .filter(|l| l.readonly)
        .filter_map(|l| order(prefix, &l.name).map(|k| (k, l)))
        .filter(|(_, l)| {
            l.send_uuid().is_some_and(|u| nas.snapshots.iter().any(|s| s.complete && s.received_uuid.as_deref() == Some(u)))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, l)| l)
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

/// Local snapshots of `prefix` to delete: all but the newest `keep`, never `parent`, never a
/// name that is not `<prefix>.<long timestamp>`.
pub fn to_delete(prefix: &str, names: &[String], keep: usize, parent: &str) -> Vec<String> {
    let mut mine: Vec<((String, u64), &String)> = names.iter().filter_map(|n| order(prefix, n).map(|k| (k, n))).collect();
    mine.sort_by(|a, b| b.0.cmp(&a.0));
    let mut kept = 0;
    let mut out = Vec::new();
    for (_, n) in mine {
        if n == parent || kept < keep {
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
            Some(Local { name, uuid: i.uuid, received_uuid: i.received_uuid, readonly: i.readonly })
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

/// The NAS' answer to a stream: the receive, or (status, error text).
type Answer = Result<AgentBtrfsReceived, (u16, String)>;

/// Streams `btrfs send` of `snap` to the NAS: (raw bytes, compressed bytes, answer).
fn stream_to_nas(client: &Client, snap: &Path, parent: Option<&Path>, query: &str, progress: Option<Arc<Counters>>) -> Result<(u64, u64, Answer), String> {
    let mut cmd = Command::new("btrfs");
    cmd.arg("send").arg("-q");
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
    let enc = zstd::stream::read::Encoder::new(stream, ZSTD_LEVEL).map_err(|e| format!("zstd: {e}"))?;
    let mut body = Counted { inner: enc, n: 0 };
    let r = client
        .upload(query, &[("Content-Type", "application/zstd"), ("Accept", "application/json")], &mut body, ANSWER_WAIT)
        .map_err(|e| format!("sending to {}: {e}", client.ep.url()))?;
    let (rawn, comp) = (raw.load(Ordering::Relaxed), body.n);
    if r.ok() {
        let got = serde_json::from_slice::<AgentBtrfsReceived>(&r.body).map_err(|e| format!("bad answer from the NAS: {e}"))?;
        return Ok((rawn, comp, Ok(got)));
    }
    Ok((rawn, comp, Err((r.status, r.error_text()))))
}

/// Snapshot, parent, send and local keep for one subvolume. `root`: where the PC's paths
/// are (`/`).
pub fn send_one(client: &Client, sub: &Sub, root: &Path, ts: &str, nas: &AgentBtrfsListing, progress: Option<Arc<Counters>>) -> Result<Sent, String> {
    let dir: PathBuf = root.join(sub.snap_dir.trim_start_matches('/'));
    if !dir.is_dir() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let locals = list_local(&dir, &sub.name);
    let name = unique_name(&sub.name, ts, |n| locals.iter().any(|l| l.name == n) || dir.join(n).exists() || nas.snapshots.iter().any(|s| s.name == n));
    let parent = pick_parent(&sub.name, &locals, nas).map(|l| l.name.clone());
    let snap = dir.join(&name);
    let source = root.join(sub.mount.trim_start_matches('/'));
    btrfs::snapshot_ro(&source, &snap)?;
    if let Some(c) = &progress
        && let Ok(mut m) = c.message.lock()
    {
        *m = Some(match &parent {
            Some(p) => format!("{}: incremental from {p}", sub.name),
            None => format!("{}: full", sub.name),
        });
    }
    let query = format!("{AGENT_BTRFS_RECEIVE}?snapshot={name}&parent={}", parent.as_deref().unwrap_or("none"));
    let parent_path = parent.as_ref().map(|p| dir.join(p));
    let result = stream_to_nas(client, &snap, parent_path.as_deref(), &query, progress).and_then(|(raw, comp, answer)| match answer {
        Ok(_) => Ok((raw, comp)),
        // already there (an earlier attempt finished on the NAS after the connection broke)
        Err((409, text)) => {
            let again: AgentBtrfsListing = client.get_json(AGENT_BTRFS_SNAPSHOTS)?;
            if again.snapshots.iter().any(|s| s.name == name && s.complete) {
                Ok((raw, comp))
            } else {
                Err(text)
            }
        }
        Err((_, text)) => Err(text),
    });
    match result {
        Ok((raw, compressed)) => {
            let mut names: Vec<String> = locals.iter().map(|l| l.name.clone()).collect();
            names.push(name.clone());
            for old in to_delete(&sub.name, &names, KEEP_LOCAL, &name) {
                if let Err(e) = btrfs::delete(&dir.join(&old)) {
                    say(format!("local snapshot {old}: {e}"));
                }
            }
            Ok(Sent { name, parent, raw, compressed })
        }
        Err(e) => {
            // not on the NAS: useless as a parent
            if let Err(d) = btrfs::delete(&snap) {
                say(format!("{}: {d}", snap.display()));
            }
            Err(e)
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
    let layout = btrbk::layout(&all).map_err(|e| fail(e, Vec::new(), None))?;
    let conf = match btrbk::read_conf(root) {
        Ok(c) => c.map(|t| btrbk::parse_conf(&t)).unwrap_or_default(),
        Err(e) => {
            say(format!("{e}: btrbk's names and snapshot folder are not used"));
            Vec::new()
        }
    };
    let (subs, problems) = plan_subvolumes(&layout, &conf, &all, &st.plan.plan.excludes, root);
    if subs.is_empty() {
        return Err(fail("no subvolume to send", Vec::new(), None));
    }
    // the manifest first: the root snapshot carries it
    let planned: BTreeMap<String, CaptureMethod> = subs.iter().map(|s| (s.mount.clone(), CaptureMethod::BtrfsSnapshot)).collect();
    let mut man = manifest::collect(&planned);
    let snapshot_dir = subs.iter().find(|s| s.mount == "/").unwrap_or(&subs[0]).snap_dir.clone();
    let block = BtrbkLayout {
        snapshot_dir,
        subvolumes: subs.iter().map(|s| BtrbkSubvolume { name: s.name.clone(), path: s.path.clone(), mount: s.mount.clone() }).collect(),
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
        let note = match send_one(client, s, root, &ts, &nas, Some(counters.clone())) {
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
        captures.push(Capture { path: s.mount.clone(), fstype: "btrfs".into(), method: CaptureMethod::BtrfsSnapshot, note: Some(note) });
    }
    set_phase(AgentPhase::Finishing);
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
        let sub = Sub { name: name.clone(), path: String::new(), mount: path.clone(), snap_dir: snap_dir.to_string() };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(name: &str, received: Option<&str>) -> AgentBtrfsSnapshot {
        AgentBtrfsSnapshot { name: name.into(), received_uuid: received.map(Into::into), complete: received.is_some() }
    }

    fn local(name: &str, uuid: &str, received: Option<&str>, ro: bool) -> Local {
        Local { name: name.into(), uuid: Some(uuid.into()), received_uuid: received.map(Into::into), readonly: ro }
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
        assert_eq!(pick_parent("@home", &locals, &nas).unwrap().name, "@home.20261112T0300");
        // writable snapshots are never parents
        let w = vec![local("@home.20261112T0300", "u2", None, false)];
        assert!(pick_parent("@home", &w, &nas).is_none());
        // a snapshot received on this PC (after a restore) is matched by its received UUID
        let restored = vec![local("@home.20261112T0300", "new-local-uuid", Some("u2"), true)];
        assert_eq!(pick_parent("@home", &restored, &nas).unwrap().name, "@home.20261112T0300");
        // `_N` sorts after the plain name of the same minute
        let nas2 = AgentBtrfsListing { snapshots: vec![snap("a.20261112T0300", Some("x")), snap("a.20261112T0300_1", Some("y"))], receiving: false };
        let l2 = vec![local("a.20261112T0300_1", "y", None, true), local("a.20261112T0300", "x", None, true)];
        assert_eq!(pick_parent("a", &l2, &nas2).unwrap().name, "a.20261112T0300_1");
        // nothing in common: a full send
        assert!(pick_parent("@home", &locals, &AgentBtrfsListing::default()).is_none());
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
    fn keep_policy_never_deletes_the_parent() {
        let names: Vec<String> = ["@.20261110T0300", "@.20261111T0300", "@.20261112T0300", "@.20261113T0300", "@home.20261101T0300", "@.manual", "@.20261109T0300+0100"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(to_delete("@", &names, 2, "@.20261113T0300"), ["@.20261111T0300", "@.20261110T0300"]);
        // the parent kept even when it is not among the newest
        assert_eq!(to_delete("@", &names, 1, "@.20261110T0300"), ["@.20261112T0300", "@.20261111T0300"]);
        assert!(to_delete("@", &names, 5, "@.20261113T0300").is_empty());
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
        let (subs, problems) = plan_subvolumes(&l, &conf, &all, &["/data/".into()], root);
        assert!(problems.is_empty(), "{problems:?}");
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
        let (subs, _) = plan_subvolumes(&l, &conf, &all, &[], t2.path());
        assert_eq!(subs[1].snap_dir, AGENT_SNAPSHOT_DIR);
        // a name that cannot be made unique is reported, not sent
        let clash = btrbk::parse_conf("subvolume /srv/a\n  snapshot_name a_home\n  target ssh://n/x\nsubvolume /home\n  snapshot_name a_home\n  target ssh://n/x\n");
        let (subs, problems) = plan_subvolumes(&l, &clash, &all, &[], root);
        assert_eq!(problems.len(), 1, "{subs:?}");
        assert!(problems[0].contains("no usable snapshot name"));
    }
}
