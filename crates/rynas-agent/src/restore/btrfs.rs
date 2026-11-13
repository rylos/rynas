//! The btrfs method as a restore source (`docs/research/pc-backup.md` §11.4): the PC's
//! snapshots received by the NAS (sent by `rynas-agent` itself, or by btrbk over SSH),
//! streamed back over the pinned HTTPS connection and **received into the new btrfs pool**.
//!
//! - Versions are runs: the snapshots with one timestamp in their names; each chosen
//!   subvolume can take another snapshot instead (`--subvol NAME=SNAPSHOT`, or the TUI).
//! - Modes over the manifest's subvolumes: whole PC, system only, a custom list, or *add*
//!   (into an existing pool, nothing formatted). The rules come from the mount points and
//!   the paths: a subvolume mounted inside another, or nested in it without a mount of its
//!   own (`@home/games`, placed after its parent where the parent's snapshot left an empty
//!   folder), needs that one too (restored with it, or already in the pool when adding);
//!   formatting needs the root subvolume; at least one subvolume.
//! - Each stream goes through the NAS' own stream guard ([`crate::send_stream`]: exactly the
//!   announced snapshot, v1, paths inside the subvolume, clones only from itself) into
//!   `btrfs receive -e --chroot` in an empty staging folder of the pool, which must then hold
//!   exactly that read-only subvolume with the stream's received UUID and a generation.
//! - Then a writable snapshot at the subvolume's own path (an existing one is renamed
//!   `<path>_rynas_old_<btrbk timestamp>`, never deleted), and the received snapshot moves
//!   into the PC's snapshot folder with its btrbk name (an empty folder there, the place a
//!   nested snapshot left in the received root, makes way): it stays the parent of the next
//!   incremental send (B2 picks it by its received UUID).
//! - Only the subvolumes of the filesystem mounted at `/` are received (one on another btrfs
//!   filesystem is said, and left as it is).
//! - A PC without manifest (older btrbk snapshots): [`super::legacy`].

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use rynas_api_types::pcagent::*;
use rynas_api_types::pcbackup::{PcRun, ReceivedSnapshot};

use super::exec::{self, Log};
use super::legacy::{self, Legacy};
use super::paths::{in_pool, safe_rel, under};
use super::source::{Fill, PoolTarget, Progress, RestoreSource, Version};
use crate::btrfs;
use crate::http::Client;
use crate::send_stream::{Expect, Guard};

/// Where the streams land in the pool before they are checked: a plain folder at the top
/// level with one folder per run (`<btrbk timestamp>/<n>`), removed at the end.
pub const STAGING: &str = ".rynas-restore-incoming";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Format; every subvolume of the manifest.
    Whole,
    /// Format; the root subvolume and the ones mounted at system paths.
    System,
    /// Format; the subvolumes named.
    Custom,
    /// No format: the subvolumes named, into the target's existing filesystem.
    Add,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Whole, Mode::System, Mode::Custom, Mode::Add];
    /// The command line's name.
    pub fn cli(self) -> &'static str {
        match self {
            Mode::Whole => "whole",
            Mode::System => "system",
            Mode::Custom => "custom",
            Mode::Add => "add",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.cli() == s)
    }
    /// What the TUI and the plan say.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Whole => "Whole PC: format the disk, restore every subvolume",
            Mode::System => "System only: format the disk, restore / and the system subvolumes",
            Mode::Custom => "Custom: format the disk, restore the subvolumes you tick",
            Mode::Add => "Add subvolumes: into the PC's existing btrfs filesystem, nothing formatted",
        }
    }
    pub fn formats(self) -> bool {
        self != Mode::Add
    }
}

/// One subvolume to restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// btrbk's name (the prefix of its snapshots).
    pub name: String,
    /// Path from the top level (a plain path).
    pub path: String,
    pub mount: String,
    /// Where it is on the PC: the mount point, or for a nested subvolume without a mount its
    /// place inside its parent (`/home/games`).
    pub place: String,
    pub snapshot: String,
    pub received_uuid: String,
    pub referenced: Option<u64>,
}

impl Choice {
    /// Its mount point, or what the plan and the log say when it has none.
    pub fn where_text(&self) -> String {
        match (self.mount.is_empty(), self.place.is_empty()) {
            (false, _) => self.mount.clone(),
            (true, false) => format!("not mounted, inside {}", self.place),
            (true, true) => "not mounted".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub mode: Mode,
    pub chosen: Vec<Choice>,
    /// Subvolumes of the PC not restored (paths): not created, fstab lines commented.
    pub skipped: Vec<String>,
    pub notes: Vec<String>,
}

impl Selection {
    /// The referenced bytes of the chosen snapshots (`None` when one is unknown).
    pub fn referenced(&self) -> Option<u64> {
        self.chosen.iter().map(|c| c.referenced).sum()
    }
}

/// Mount points of the "system only" mode besides `/`.
const SYSTEM_MOUNTS: &[&str] = &["/usr", "/var", "/opt", "/boot", "/etc", "/root"];

fn system_mount(m: &str) -> bool {
    m == "/" || SYSTEM_MOUNTS.iter().any(|s| under(s, m))
}

/// `parent` is a proper mount-point parent of `child` (both mounted).
fn mounted_inside(parent: &str, child: &str) -> bool {
    !parent.is_empty() && !child.is_empty() && parent.trim_end_matches('/') != child.trim_end_matches('/') && under(parent, child)
}

/// `parent` is a proper ancestor of `child` by their paths from the top level.
fn nested_by_path(parent: &str, child: &str) -> bool {
    let (p, c) = (parent.trim_matches('/'), child.trim_matches('/'));
    !p.is_empty() && p != c && under(p, c)
}

/// A complete received snapshot (read-only with a received UUID).
pub fn complete(s: &ReceivedSnapshot) -> bool {
    !s.incomplete && !s.not_subvolume && s.readonly && s.received_uuid.is_some()
}

/// The snapshot of `group` for run `run`: the run's own when complete, else the newest
/// complete one not newer than the run, else the newest complete one.
pub fn snapshot_for<'a>(snaps: &'a [ReceivedSnapshot], run: &PcRun, group: &str) -> Option<&'a ReceivedSnapshot> {
    let mine = || snaps.iter().filter(|s| s.group == group && complete(s));
    mine()
        .find(|s| run.snapshots.contains(&s.name))
        .or_else(|| mine().filter(|s| s.created_ms.is_some() && s.created_ms <= run.created_ms).max_by_key(|s| s.created_ms))
        .or_else(|| mine().max_by_key(|s| s.created_ms))
}

/// The subvolumes to restore, by the rules of the mode (pure: tested). `names`: the custom
/// or added subvolumes; `overrides`: (name, snapshot); `pool_has`: a path exists in the
/// target filesystem (adding). Only the subvolumes of the filesystem mounted at `/` take part.
pub fn select(
    m: &Manifest,
    snaps: &[ReceivedSnapshot],
    run: &PcRun,
    mode: Mode,
    names: &[String],
    overrides: &[(String, String)],
    pool_has: &dyn Fn(&str) -> bool,
) -> Result<Selection, String> {
    let all = m.btrbk.as_ref().map(|b| b.subvolumes.clone()).unwrap_or_default();
    if all.is_empty() {
        return Err("the manifest lists no btrfs subvolumes of the PC".into());
    }
    let mut notes = Vec::new();
    // the root's filesystem only: another one is not formatted by this restore
    let root_uuid = m.root().map(|f| f.uuid.clone()).unwrap_or_default();
    let (layout, elsewhere): (Vec<BtrbkSubvolume>, Vec<BtrbkSubvolume>) = all.into_iter().partition(|s| s.fs_uuid.is_empty() || root_uuid.is_empty() || s.fs_uuid.eq_ignore_ascii_case(&root_uuid));
    for s in &elsewhere {
        notes.push(format!("{}: on another btrfs filesystem ({}), not restored: that filesystem stays as it is", s.name, s.fs_uuid));
    }
    for s in &layout {
        safe_rel(&s.path).filter(|p| !p.is_empty()).ok_or_else(|| format!("{}: the manifest's path {:?} is not a plain path", s.name, s.path))?;
    }
    let root = layout.iter().find(|s| s.mount == "/");
    // where each one is on the PC: its mount point, or for a nested one its parent's place
    // with its path inside it
    let whole = BtrbkLayout { subvolumes: layout.clone(), ..Default::default() };
    let place = |s: &BtrbkSubvolume| whole.place_of(s).unwrap_or_default();
    for s in layout.iter().filter(|s| !s.nested_in.is_empty()) {
        if place(s).is_empty() || safe_rel(&s.inside).is_none_or(|p| p.is_empty()) {
            return Err(format!("{}: nested in {:?}, which the manifest does not list (or a path {:?} that is not plain)", s.name, s.nested_in, s.inside));
        }
    }
    let wanted: Vec<&BtrbkSubvolume> = match mode {
        Mode::Whole => layout.iter().collect(),
        Mode::System => layout.iter().filter(|s| system_mount(&place(s))).collect(),
        Mode::Custom | Mode::Add => {
            let mut v = Vec::new();
            for n in names {
                if elsewhere.iter().any(|s| &s.name == n) {
                    return Err(format!("{n} is on another btrfs filesystem than /: this restore receives only into the root's filesystem"));
                }
                let s = layout.iter().find(|s| &s.name == n).ok_or_else(|| format!("{n}: not a subvolume of this PC (it has {})", layout.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")))?;
                if !v.contains(&s) {
                    v.push(s);
                }
            }
            v
        }
    };
    if wanted.is_empty() {
        return Err("choose at least one subvolume".into());
    }
    if mode.formats() {
        match root {
            Some(r) if wanted.contains(&r) => {}
            Some(r) => return Err(format!("the disk is formatted: the root subvolume {} (mounted at /) must be restored too", r.name)),
            None => return Err("the manifest has no subvolume mounted at /".into()),
        }
    } else if let Some(r) = root
        && !wanted.contains(&r)
        && !pool_has(&r.path)
    {
        return Err(format!("adding needs the PC's filesystem with its root subvolume {} ({}); it is not in the chosen filesystem", r.name, r.path));
    }
    for c in &wanted {
        let parent = layout.iter().filter(|p| p.name != c.name && mounted_inside(&p.mount, &c.mount)).max_by_key(|p| p.mount.len());
        if let Some(p) = parent
            && !wanted.contains(&p)
            && (mode.formats() || !pool_has(&p.path))
        {
            return Err(format!("{} is mounted inside {} ({} in {}): restore {} too", c.name, p.name, c.mount, p.mount, p.name));
        }
        // a subvolume nested in another by path (a `@home/games` without a mount of its
        // own) lives inside that one: it needs it too
        let holder = layout.iter().filter(|p| p.name != c.name && nested_by_path(&p.path, &c.path)).max_by_key(|p| p.path.len());
        if let Some(p) = holder
            && !wanted.contains(&p)
            && (mode.formats() || !pool_has(&p.path))
        {
            return Err(format!("{} is nested inside {} ({} in {}): restore {} too", c.name, p.name, c.path, p.path, p.name));
        }
    }
    for (n, s) in overrides {
        if !wanted.iter().any(|c| &c.name == n) {
            notes.push(format!("{n}: the snapshot {s} asked for it is not used, {n} is not restored"));
        }
    }
    let mut chosen = Vec::new();
    for c in &wanted {
        let snap = match overrides.iter().find(|(n, _)| n == &c.name) {
            Some((_, s)) => snaps.iter().find(|x| &x.name == s && x.group == c.name && complete(x)).ok_or_else(|| format!("{s}: not a complete snapshot of {}", c.name))?,
            None => {
                let s = snapshot_for(snaps, run, &c.name).ok_or_else(|| format!("{}: the NAS has no complete snapshot of it", c.name))?;
                if !run.snapshots.contains(&s.name) {
                    notes.push(format!("{}: not in the run {}, {} is used", c.name, run.timestamp, s.name));
                }
                s
            }
        };
        chosen.push(Choice {
            name: c.name.clone(),
            path: c.path.trim_matches('/').to_string(),
            mount: c.mount.clone(),
            place: if c.nested_in.is_empty() { String::new() } else { place(c) },
            snapshot: snap.name.clone(),
            received_uuid: snap.received_uuid.clone().unwrap_or_default(),
            referenced: snap.size.map(|z| z.referenced_bytes),
        });
    }
    chosen.sort_by_key(|c| (c.path.matches('/').count(), c.path.clone()));
    let skipped = if mode.formats() {
        layout.iter().filter(|s| !wanted.contains(s)).map(|s| s.path.trim_matches('/').to_string()).collect()
    } else {
        Vec::new()
    };
    Ok(Selection { mode, chosen, skipped, notes })
}

/// The PC's snapshot folder as a plain path from the top level of the pool: its
/// `snapshot_dir` (a path on the PC) below the mount whose subvolume holds it;
/// `btrbk_snapshots` at the top level when the manifest has none.
pub fn snapshot_dir_in_pool(m: &Manifest) -> Result<String, String> {
    let sd = m.btrbk.as_ref().map(|b| b.snapshot_dir.trim().to_string()).unwrap_or_default();
    let bad = || format!("the snapshot folder {sd:?} is not a plain path");
    if sd.is_empty() {
        return Ok("btrbk_snapshots".into());
    }
    if !sd.starts_with('/') {
        return safe_rel(&sd).ok_or_else(bad);
    }
    let mounts: Vec<&MountEntry> = m.root().map(|f| f.mounts.iter().collect()).unwrap_or_default();
    let best = mounts.into_iter().filter(|me| under(&me.path, &sd)).max_by_key(|me| me.path.trim_end_matches('/').len());
    let joined = match best {
        Some(me) => {
            let rest = sd[me.path.trim_end_matches('/').len()..].trim_matches('/');
            let base = me.subvol.as_deref().unwrap_or("").trim_matches('/');
            [base, rest].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("/")
        }
        None => sd.trim_matches('/').to_string(),
    };
    safe_rel(&joined).ok_or_else(bad)
}

/// The decoded stream, counted: the progress is told at most every 500 ms.
struct Counting<'a, R> {
    inner: R,
    n: &'a std::cell::Cell<u64>,
    base: u64,
    total: u64,
    p: Progress<'a>,
    last: std::time::Instant,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let k = self.inner.read(b)?;
        self.n.set(self.n.get() + k as u64);
        if self.last.elapsed() >= std::time::Duration::from_millis(500) {
            self.last = std::time::Instant::now();
            let done = self.base + self.n.get();
            (self.p)(done, self.total.max(done));
        }
        Ok(k)
    }
}

pub struct BtrfsSource {
    client: Arc<Client>,
    pub dev: RestoreDevice,
    pub listing: RestoreBtrfsListing,
    /// What the legacy reader put together (no manifest in the snapshots).
    pub legacy: Option<Legacy>,
    /// The manifest of the chosen version.
    manifest: Option<Manifest>,
    pub selection: Option<Selection>,
    /// ESP size of the guided layout (no partition table dump), MiB.
    pub esp_mib: u64,
}

/// `<path>` set aside as `<path>_rynas_old_<stamp>` (`_2`, `_3`… when taken), never
/// replacing anything.
fn set_aside(target: &Path, stamp: &str) -> Result<PathBuf, String> {
    let (Some(parent), Some(name)) = (target.parent(), target.file_name().and_then(|n| n.to_str())) else {
        return Err(format!("{}: no folder to rename in", target.display()));
    };
    let dir = crate::ioctl::open_dir(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    for k in 1..100 {
        let new = if k == 1 { format!("{name}_rynas_old_{stamp}") } else { format!("{name}_rynas_old_{stamp}_{k}") };
        match crate::ioctl::rename_noreplace(&dir, name, &new) {
            Ok(()) => return Ok(parent.join(new)),
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => continue,
            Err(e) => return Err(format!("renaming {}: {e}", target.display())),
        }
    }
    Err(format!("{}: too many earlier copies set aside", target.display()))
}

/// Removes what a run received into `dir` (the subvolume of the stream, if any), then the
/// folder itself.
fn discard(dir: &Path, log: &mut Log) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if btrfs::subvol_info(&e.path()).is_some()
                && let Err(err) = btrfs::delete(&e.path())
            {
                log.line(format!("warning: {err}"));
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

impl BtrfsSource {
    /// The PC's snapshots on the NAS, with a restore session's client.
    pub fn new(client: Arc<Client>, dev: RestoreDevice) -> Result<BtrfsSource, String> {
        let listing: RestoreBtrfsListing = client.get_json(&format!("{RESTORE_BTRFS}/{}/snapshots", dev.device))?;
        Ok(BtrfsSource { client, dev, listing, legacy: None, manifest: None, selection: None, esp_mib: 1024 })
    }

    pub fn run(&self, id: &str) -> Option<&PcRun> {
        self.listing.runs.iter().find(|r| r.timestamp == id)
    }

    /// The subvolume names of the PC's snapshots (complete ones).
    fn groups(&self) -> Vec<String> {
        let mut g: Vec<String> = self.listing.snapshots.iter().filter(|s| complete(s)).map(|s| s.group.clone()).collect();
        g.sort();
        g.dedup();
        g
    }

    /// Chooses what to restore of version `v` (see [`select`]); after [`RestoreSource::manifest`].
    pub fn choose(&mut self, v: &Version, mode: Mode, names: &[String], overrides: &[(String, String)], pool_has: &dyn Fn(&str) -> bool) -> Result<&Selection, String> {
        let m = self.manifest.as_ref().expect("the version's manifest is read before choosing");
        let run = self.run(&v.id).ok_or_else(|| format!("{}: no such run", v.id))?;
        let mut s = select(m, &self.listing.snapshots, run, mode, names, overrides, pool_has)?;
        if let Some(l) = &self.legacy {
            s.notes.splice(0..0, l.notes.iter().cloned());
        }
        self.selection = Some(s);
        Ok(self.selection.as_ref().expect("set above"))
    }

    /// "Add subvolumes" up to the plan: the filesystem on `device` looked at (read-only,
    /// [`exec::probe_pool`]), checked to be the PC's ([`exec::check_same_pool`], unless
    /// `other_ok`), the subvolumes chosen against what it holds. The restored paths.
    pub fn prepare_add(&mut self, v: &Version, device: &str, names: &[String], overrides: &[(String, String)], other_ok: bool) -> Result<Vec<String>, String> {
        let m = self.manifest.as_ref().expect("the version's manifest is read before choosing");
        let paths: Vec<String> = m.btrbk.as_ref().map(|b| b.subvolumes.iter().map(|s| s.path.clone()).collect()).unwrap_or_default();
        let probe = exec::probe_pool(device, &paths)?;
        exec::check_same_pool(m, device, probe.uuid.as_deref(), other_ok)?;
        let has = |p: &str| probe.present.iter().any(|x| x.trim_matches('/') == p.trim_matches('/'));
        let sel = self.choose(v, Mode::Add, names, overrides, &has)?;
        Ok(sel.chosen.iter().map(|c| c.path.clone()).collect())
    }

    /// The manifest the plan is built from for a target with `sector`-byte sectors: the
    /// root filesystem's used bytes are the chosen snapshots' referenced bytes when known (the
    /// "fits" check: they plus 10 % must fit the new btrfs partition; a smaller disk is fine),
    /// and a legacy guided layout fitted to the sector size.
    pub fn planned_manifest(&self, m: &Manifest, sector: u32) -> Manifest {
        let mut m = m.clone();
        if let Some(b) = self.selection.as_ref().and_then(Selection::referenced).filter(|b| *b > 0)
            && let Some(f) = m.filesystems.iter_mut().find(|f| f.mounts.iter().any(|x| x.path == "/"))
        {
            f.used_bytes = b;
        }
        legacy::refit_guided(&mut m, sector, self.esp_mib);
        m
    }

    /// Receives one snapshot into `dir` (an empty folder of the pool) and checks it: the
    /// received subvolume and the bytes of the stream.
    fn receive(&self, c: &Choice, dir: &Path, base: u64, total: u64, p: Progress) -> Result<(PathBuf, u64), String> {
        let path = format!("{RESTORE_BTRFS}/{}/send?snapshot={}", self.dev.device, c.snapshot);
        let body = match self.client.get_stream(&path, &[("Accept", "application/zstd")], std::time::Duration::from_secs(600)) {
            Ok(Ok(b)) => b,
            Ok(Err(r)) => return Err(format!("{}: {}", c.snapshot, r.error_text())),
            Err(e) => return Err(format!("{}: {e} (check the network and start the restore again)", self.client.ep.url())),
        };
        let mut dec = zstd::stream::read::Decoder::new(body).map_err(|e| format!("zstd: {e}"))?;
        dec.window_log_max(AGENT_ZSTD_WINDOW_LOG_MAX).map_err(|e| format!("zstd: {e}"))?;
        let n = std::cell::Cell::new(0u64);
        let counted = Counting { inner: dec, n: &n, base, total, p, last: std::time::Instant::now() };
        // the NAS is ours and pinned, but the stream is still checked like the NAS checks
        // the PC's: exactly this snapshot, a v1 stream, paths inside it
        let mut guard = Guard::new(counted, Expect { name: c.snapshot.clone(), parent_uuid: None });
        let mut child = Command::new("btrfs")
            .args(["receive", "-e", "--chroot"])
            .arg(format!("{}/", dir.display()))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("btrfs receive: {e} (install btrfs-progs)"))?;
        let mut errp = child.stderr.take();
        let errs = std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(e) = errp.as_mut() {
                let _ = e.take(16 << 10).read_to_string(&mut s);
            }
            s
        });
        let mut stdin = child.stdin.take().ok_or("btrfs receive: no input")?;
        let copied = std::io::copy(&mut guard, &mut stdin);
        drop(stdin);
        if copied.as_ref().is_err_and(|e| e.kind() != std::io::ErrorKind::BrokenPipe) {
            let _ = child.kill();
        }
        let status = child.wait().map_err(|e| e.to_string())?;
        let text = errs.join().unwrap_or_default();
        if let Err(e) = copied
            && e.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(format!("{}: the stream from the NAS failed: {e} (start the restore again; the NAS log says more)", c.snapshot));
        }
        if !status.success() {
            let last = text.lines().rfind(|l| !l.starts_with("At ") && !l.starts_with("Chroot to ")).unwrap_or("").trim().to_string();
            return Err(format!("{}: btrfs receive: {} (check the free space of the target disk, then start again)", c.snapshot, if last.is_empty() { status.to_string() } else { last }));
        }
        if !guard.seen.ended {
            return Err(format!("{}: the stream did not end (the connection broke: start the restore again)", c.snapshot));
        }
        let bytes = n.get();
        // exactly the announced subvolume, read-only, with the stream's received UUID and data
        let names: Vec<String> = std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        if names != [c.snapshot.clone()] {
            return Err(format!("{}: the receive left {:?} instead of the snapshot alone: the copy on the NAS is not usable, choose another snapshot of {}", c.snapshot, names, c.name));
        }
        let got = dir.join(&c.snapshot);
        let i = btrfs::subvol_info(&got).ok_or_else(|| format!("{}: not a subvolume after the receive", got.display()))?;
        if !i.readonly || i.received_uuid.is_none() || i.received_uuid != guard.seen.uuid {
            return Err(format!("{}: the received snapshot is not read-only with the stream's received UUID: choose another snapshot of {}", c.snapshot, c.name));
        }
        if i.generation == 0 {
            return Err(format!("{}: the received snapshot is empty: the copy on the NAS is not usable, choose another snapshot of {}", c.snapshot, c.name));
        }
        Ok((got, bytes))
    }
}

impl RestoreSource for BtrfsSource {
    fn kind(&self) -> &'static str {
        "btrfs snapshots"
    }

    fn device(&self) -> &str {
        &self.dev.device
    }

    fn versions(&mut self) -> Result<Vec<Version>, String> {
        let snaps = &self.listing.snapshots;
        let mut v: Vec<Version> = self
            .listing
            .runs
            .iter()
            .filter(|r| r.snapshots.iter().any(|n| snaps.iter().any(|s| &s.name == n && complete(s))))
            .map(|r| Version {
                id: r.timestamp.clone(),
                time_ms: r.created_ms.unwrap_or(0),
                hostname: self.dev.hostname.clone(),
                bytes: r.referenced_bytes.unwrap_or(0),
                files: 0,
            })
            .collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.time_ms));
        Ok(v)
    }

    fn manifest(&mut self, v: &Version) -> Result<Manifest, String> {
        let run = self.run(&v.id).cloned().ok_or_else(|| format!("{}: no such run", v.id))?;
        // the root subvolume's snapshot first (it holds the manifest)
        let not_root = |n: &str| !matches!(n.rsplit_once('.').map(|x| x.0).unwrap_or(n), "@" | "root" | "rootfs" | "@root");
        let mut names: Vec<String> = run.snapshots.iter().filter(|n| self.listing.snapshots.iter().any(|s| &&s.name == n && complete(s))).cloned().collect();
        names.sort_by_key(|n| not_root(n));
        // the agent's manifest, in the root snapshot of the run
        for n in &names {
            let r = self
                .client
                .request("GET", &format!("{RESTORE_BTRFS}/{}/manifest?snapshot={n}", self.dev.device), &[("Accept", "application/json")], &[])
                .map_err(|e| format!("{}: {e}", self.client.ep.url()))?;
            if r.status == 404 {
                continue;
            }
            if !r.ok() {
                return Err(format!("{n}: {}", r.error_text()));
            }
            let m = super::source::parse_manifest(&r.body, &format!("the manifest of {n}"))?;
            if m.btrbk.as_ref().is_none_or(|b| b.subvolumes.is_empty()) {
                return Err(format!("the manifest of {n} has no btrfs block (it is a file backup's)"));
            }
            self.legacy = None;
            self.manifest = Some(m.clone());
            return Ok(m);
        }
        // an older snapshot without manifest: what its own files say
        let groups = self.groups();
        for n in &names {
            let l: RestoreLegacy = self.client.get_json(&format!("{RESTORE_BTRFS}/{}/legacy?snapshot={n}", self.dev.device))?;
            if l.fstab.is_none() {
                continue;
            }
            let lg = legacy::synthesize(&self.dev.device, &l, &groups, run.referenced_bytes.unwrap_or(0), self.esp_mib)?;
            super::source::check_paths(&lg.manifest)?;
            let m = lg.manifest.clone();
            self.legacy = Some(lg);
            self.manifest = Some(m.clone());
            return Ok(m);
        }
        Err(format!("run {}: no snapshot holds a layout manifest or an /etc/fstab to restore from", v.id))
    }

    fn fill(&self) -> Fill {
        Fill::BtrfsPool
    }

    fn fill_pool(&mut self, _v: &Version, pools: &[PoolTarget], p: Progress, log: &mut Log) -> Result<(), String> {
        let sel = self.selection.clone().ok_or("no subvolumes chosen")?;
        let m = self.manifest.clone().ok_or("no manifest")?;
        let first = sel.chosen.first().ok_or("no subvolumes chosen")?;
        // the root's filesystem (the only one the selection takes subvolumes of)
        let root_uuid = m.root().map(|f| f.uuid.clone()).unwrap_or_default();
        let pool = pools
            .iter()
            .find(|x| !root_uuid.is_empty() && x.uuid.eq_ignore_ascii_case(&root_uuid))
            .or_else(|| pools.iter().find(|x| x.subvolumes.iter().any(|s| s.path.trim_matches('/') == first.path)))
            .or(pools.first())
            .ok_or("no btrfs filesystem to fill")?;
        let top = pool.top_level.clone();
        let stamp = crate::send::local_timestamp((crate::state::now_ms() / 1000) as i64);
        // one folder per run: what an earlier run left is said, never in the way
        let staging = in_pool(&top, STAGING)?;
        if let Ok(rd) = std::fs::read_dir(&staging) {
            for e in rd.flatten() {
                log.line(format!("note: {STAGING}/{} was left by an earlier restore that stopped: delete it when that one is not needed any more", e.file_name().to_string_lossy()));
            }
        }
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
        }
        let run_dir = staging.join(&stamp);
        std::fs::create_dir(&run_dir).map_err(|e| format!("{}: {e}", run_dir.display()))?;
        let total: u64 = sel.chosen.iter().filter_map(|c| c.referenced).sum();
        let mut done = 0u64;
        let mut received: Vec<(PathBuf, &Choice)> = Vec::new();
        let r = (|| -> Result<(), String> {
            for (i, c) in sel.chosen.iter().enumerate() {
                log.line(format!("receiving {} ({}) from the NAS", c.snapshot, c.where_text()));
                let dir = run_dir.join(i.to_string());
                std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                received.push((dir.clone(), c));
                let (got, bytes) = self.receive(c, &dir, done, total, p)?;
                done += bytes;
                // the writable subvolume at its own path
                let target = in_pool(&top, &c.path)?;
                if std::fs::symlink_metadata(&target).is_ok() {
                    if btrfs::is_empty_plain_dir(&target) {
                        // the place a nested subvolume leaves in its parent's snapshot
                        std::fs::remove_dir(&target).map_err(|e| format!("{}: {e}", target.display()))?;
                    } else {
                        let old = set_aside(&target, &stamp)?;
                        log.line(format!("{} was there: renamed {}, not deleted", c.path, old.display()));
                    }
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
                }
                btrfs::snapshot_rw(&got, &target)?;
                log.line(format!("{}: writable from {}", c.path, c.snapshot));
            }
            Ok(())
        })();
        eprintln!();
        if let Err(e) = r {
            // the received copies of this run go (the writable subvolumes made stay)
            for (dir, _) in &received {
                discard(dir, log);
            }
            let _ = std::fs::remove_dir(&run_dir);
            let _ = std::fs::remove_dir(&staging);
            return Err(e);
        }
        // the received snapshots into the PC's snapshot folder: the parents of the next sends
        let sd = in_pool(&top, &snapshot_dir_in_pool(&m)?)?;
        std::fs::create_dir_all(&sd).map_err(|e| format!("{}: {e}", sd.display()))?;
        for (dir, c) in &received {
            let to = sd.join(&c.snapshot);
            if std::fs::symlink_metadata(&to).is_ok() {
                if btrfs::is_empty_plain_dir(&to) {
                    // the place the snapshot (nested in the root's subvolume on the PC) left
                    // in the received root
                    std::fs::remove_dir(&to).map_err(|e| format!("{}: {e}", to.display()))?;
                } else {
                    // the same snapshot already there (adding to the PC's own filesystem): a
                    // copy received earlier (same received UUID) or the PC's own original
                    // (its UUID is what the NAS copy was received from); that one stays the
                    // parent, the copy just received goes
                    let there = btrfs::subvol_info(&to);
                    let got = btrfs::subvol_info(&dir.join(&c.snapshot));
                    let same = matches!((&there, &got), (Some(a), Some(b)) if a.readonly && b.received_uuid.is_some() && a.received_uuid.as_ref().or(a.uuid.as_ref()) == b.received_uuid.as_ref());
                    if same {
                        discard(dir, log);
                        log.line(format!("{} is already in {} (the same snapshot): it stays the parent, the copy just received is removed", c.snapshot, sd.display()));
                    } else {
                        log.line(format!("{} is already in {} and is another snapshot: the received copy stays in {}", c.snapshot, sd.display(), dir.display()));
                    }
                    continue;
                }
            }
            btrfs::move_readonly(dir, &c.snapshot, &sd, &c.snapshot)?;
            let i = btrfs::subvol_info(&to).ok_or_else(|| format!("{}: lost after the move", to.display()))?;
            if !i.readonly || i.received_uuid.is_none() {
                return Err(format!("{}: not read-only with its received UUID after the move", to.display()));
            }
            if i.received_uuid.as_deref() != Some(c.received_uuid.as_str()) {
                log.line(format!("{}: its received UUID differs from the NAS copy's: the next backup of it is a full one", c.snapshot));
            }
            log.line(format!("{} kept in {} (parent of the next incremental backup)", c.snapshot, sd.display()));
            let _ = std::fs::remove_dir(dir);
        }
        let _ = std::fs::remove_dir(&run_dir);
        let _ = std::fs::remove_dir(&staging);
        Ok(())
    }

    fn report(&mut self, v: &Version, ok: bool, message: &str, log: &str) {
        let r = RestoreReport { device: self.dev.device.clone(), version: format!("btrfs {}", v.id), ok, message: message.to_string(), log: log.to_string() };
        if let Err(e) = self.client.post_json::<_, serde_json::Value>("/api/v1/pcbackup/agent/restore/report", &r) {
            eprintln!("report to the NAS: {e}");
        }
    }

    fn skipped_subvolumes(&self) -> Vec<String> {
        self.selection.as_ref().map(|s| s.skipped.clone()).unwrap_or_default()
    }

    fn boot_copies(&self, m: &Manifest) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = m
            .btrbk
            .as_ref()
            .map(|b| b.boot_copies.iter().filter(|c| c.files > 0).map(|c| (c.dir.clone(), c.mount.clone())).collect())
            .unwrap_or_default();
        if let Some(b) = self.legacy.as_ref().and_then(|l| l.boot.clone()) {
            v.push(b);
        }
        v
    }

    fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(s) = &self.selection {
            out.push(format!("btrfs: {}", s.mode.label()));
            for c in &s.chosen {
                let size = c.referenced.map(|b| format!(", {} MiB", b >> 20)).unwrap_or_default();
                out.push(format!("  receive {} → writable {} ({}){size}", c.snapshot, c.path, c.where_text()));
            }
            for k in &s.skipped {
                out.push(format!("  not restored: {k} (its fstab line is commented: {})", super::plan::RESTORE_MARK.trim()));
            }
            if let Some(m) = &self.manifest
                && let Ok(sd) = snapshot_dir_in_pool(m)
            {
                out.push(format!("  received snapshots kept in {sd} (parents of the next incremental backups)"));
            }
            for n in &s.notes {
                out.push(format!("note: {n}"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::pcbackup::SnapshotSize;

    fn manifest() -> Manifest {
        let sub = |n: &str, p: &str, m: &str| BtrbkSubvolume { name: n.into(), path: p.into(), mount: m.into(), ..Default::default() };
        Manifest {
            format: 1,
            filesystems: vec![Filesystem {
                fstype: "btrfs".into(),
                mounts: vec![MountEntry { path: "/".into(), subvol: Some("@".into()), ..Default::default() }, MountEntry { path: "/home".into(), subvol: Some("@home".into()), ..Default::default() }],
                ..Default::default()
            }],
            btrbk: Some(BtrbkLayout {
                snapshot_dir: "/.btrbk_snapshots".into(),
                subvolumes: vec![sub("@", "@", "/"), sub("@home", "@home", "/home"), sub("@games", "@games", "/home/me/games"), sub("@var", "@var", "/var"), sub("@data", "@data", "")],
                agent: true,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn snap(g: &str, ts: &str, ms: u64, size: Option<u64>) -> ReceivedSnapshot {
        ReceivedSnapshot {
            name: format!("{g}.{ts}"),
            group: g.into(),
            created_ms: Some(ms),
            readonly: true,
            received_uuid: Some(format!("u-{g}-{ts}")),
            size: size.map(|b| SnapshotSize { referenced_bytes: b, ..Default::default() }),
            ..Default::default()
        }
    }

    fn data() -> (Vec<ReceivedSnapshot>, PcRun) {
        let mut s = Vec::new();
        for g in ["@", "@home", "@games", "@var", "@data"] {
            s.push(snap(g, "20261110T0300", 2000, Some(1 << 30)));
            s.push(snap(g, "20261109T0300", 1000, Some(1 << 30)));
        }
        // the newest run lacks @games
        s.retain(|x| x.name != "@games.20261110T0300");
        let run = PcRun {
            timestamp: "20261110T0300".into(),
            created_ms: Some(2000),
            snapshots: s.iter().filter(|x| x.name.ends_with("20261110T0300")).map(|x| x.name.clone()).collect(),
            ..Default::default()
        };
        (s, run)
    }

    fn names(s: &Selection) -> Vec<&str> {
        s.chosen.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn modes_and_snapshots() {
        let m = manifest();
        let (snaps, run) = data();
        let none = |_: &str| false;
        let w = select(&m, &snaps, &run, Mode::Whole, &[], &[], &none).unwrap();
        assert_eq!(names(&w), vec!["@", "@data", "@games", "@home", "@var"]);
        assert!(w.skipped.is_empty());
        // @games is not in the run: the newest complete one before it, said so
        let g = w.chosen.iter().find(|c| c.name == "@games").unwrap();
        assert_eq!(g.snapshot, "@games.20261109T0300");
        assert!(w.notes.iter().any(|n| n.contains("@games")));
        assert_eq!(w.referenced(), Some(5 << 30));
        let s = select(&m, &snaps, &run, Mode::System, &[], &[], &none).unwrap();
        assert_eq!(names(&s), vec!["@", "@var"]);
        assert_eq!(s.skipped, vec!["@home", "@games", "@data"]);
        // an override
        let o = select(&m, &snaps, &run, Mode::System, &[], &[("@".into(), "@.20261109T0300".into())], &none).unwrap();
        assert_eq!(o.chosen[0].snapshot, "@.20261109T0300");
        assert!(select(&m, &snaps, &run, Mode::System, &[], &[("@".into(), "@home.20261109T0300".into())], &none).is_err());
    }

    #[test]
    fn nested_subvolumes_without_a_mount() {
        let mut m = manifest();
        let b = m.btrbk.as_mut().unwrap();
        let nested = |n: &str, p: &str, parent: &str, inside: &str| BtrbkSubvolume { name: n.into(), path: p.into(), nested_in: parent.into(), inside: inside.into(), ..Default::default() };
        b.subvolumes.push(nested("saves", "@home/saves", "@home", "saves"));
        b.subvolumes.push(nested("deep", "@home/saves/deep", "saves", "deep"));
        b.subvolumes.push(nested("machines", "@/var/lib/machines", "@", "var/lib/machines"));
        let (mut snaps, mut run) = data();
        for g in ["saves", "deep", "machines"] {
            snaps.push(snap(g, "20261110T0300", 2000, Some(1 << 20)));
            run.snapshots.push(format!("{g}.20261110T0300"));
        }
        let none = |_: &str| false;
        // whole: parents first, the nested ones after them
        let w = select(&m, &snaps, &run, Mode::Whole, &[], &[], &none).unwrap();
        let order: Vec<&str> = w.chosen.iter().map(|c| c.path.as_str()).collect();
        let pos = |p: &str| order.iter().position(|x| *x == p).unwrap();
        assert!(pos("@home") < pos("@home/saves") && pos("@home/saves") < pos("@home/saves/deep") && pos("@") < pos("@/var/lib/machines"), "{order:?}");
        // system: a nested one at a system place goes with /, one in /home does not
        let s = select(&m, &snaps, &run, Mode::System, &[], &[], &none).unwrap();
        assert!(names(&s).contains(&"machines") && !names(&s).contains(&"saves"));
        assert!(s.skipped.contains(&"@home/saves".to_string()));
        // custom: a nested one needs its parent, at every level
        let e = select(&m, &snaps, &run, Mode::Custom, &["@".into(), "saves".into()], &[], &none).unwrap_err();
        assert!(e.contains("nested inside @home"), "{e}");
        let e = select(&m, &snaps, &run, Mode::Custom, &["@".into(), "@home".into(), "deep".into()], &[], &none).unwrap_err();
        assert!(e.contains("nested inside saves"), "{e}");
        assert!(select(&m, &snaps, &run, Mode::Custom, &["@".into(), "@home".into(), "saves".into(), "deep".into()], &[], &none).is_ok());
        // add: the parent already in the pool is enough
        let pool = |p: &str| p == "@" || p == "@home";
        let a = select(&m, &snaps, &run, Mode::Add, &["saves".into()], &[], &pool).unwrap();
        assert_eq!(names(&a), vec!["saves"]);
        assert!(select(&m, &snaps, &run, Mode::Add, &["saves".into()], &[], &|p: &str| p == "@").is_err());
        // a parent the manifest does not list is refused
        m.btrbk.as_mut().unwrap().subvolumes.push(nested("lost", "@x/lost", "@x", "lost"));
        assert!(select(&m, &snaps, &run, Mode::Whole, &[], &[], &none).unwrap_err().contains("lost"));
    }

    #[test]
    fn nesting_rules_from_mount_points() {
        let m = manifest();
        let (snaps, run) = data();
        let none = |_: &str| false;
        let n = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // formatting: the root is needed, and @games needs @home
        assert!(select(&m, &snaps, &run, Mode::Custom, &n(&["@home"]), &[], &none).unwrap_err().contains("root"));
        assert!(select(&m, &snaps, &run, Mode::Custom, &n(&["@", "@games"]), &[], &none).unwrap_err().contains("@home"));
        assert!(select(&m, &snaps, &run, Mode::Custom, &n(&["@", "@home", "@games"]), &[], &none).is_ok());
        assert!(select(&m, &snaps, &run, Mode::Custom, &[], &[], &none).unwrap_err().contains("at least one"));
        assert!(select(&m, &snaps, &run, Mode::Custom, &n(&["nope"]), &[], &none).is_err());
        // adding: the parents may already be in the pool
        assert!(select(&m, &snaps, &run, Mode::Add, &n(&["@games"]), &[], &none).unwrap_err().contains("root"));
        let pool = |p: &str| p == "@";
        assert!(select(&m, &snaps, &run, Mode::Add, &n(&["@games"]), &[], &pool).unwrap_err().contains("@home"));
        let pool = |p: &str| p == "@" || p == "@home";
        let a = select(&m, &snaps, &run, Mode::Add, &n(&["@games"]), &[], &pool).unwrap();
        assert_eq!(names(&a), vec!["@games"]);
        assert!(a.skipped.is_empty(), "adding comments nothing out");
        // an unmounted subvolume has no parent
        assert!(select(&m, &snaps, &run, Mode::Add, &n(&["@data"]), &[], &pool).is_ok());
    }

    #[test]
    fn snapshot_folder_in_the_pool() {
        let mut m = manifest();
        assert_eq!(snapshot_dir_in_pool(&m).unwrap(), "@/.btrbk_snapshots");
        m.btrbk.as_mut().unwrap().snapshot_dir = "/mnt/pool/btrbk_snapshots".into();
        m.filesystems[0].mounts.push(MountEntry { path: "/mnt/pool".into(), subvol: None, ..Default::default() });
        assert_eq!(snapshot_dir_in_pool(&m).unwrap(), "btrbk_snapshots");
        m.btrbk.as_mut().unwrap().snapshot_dir = "/home/.snaps".into();
        assert_eq!(snapshot_dir_in_pool(&m).unwrap(), "@home/.snaps");
        m.btrbk.as_mut().unwrap().snapshot_dir = String::new();
        assert_eq!(snapshot_dir_in_pool(&m).unwrap(), "btrbk_snapshots");
    }

    #[test]
    fn mount_point_rules() {
        assert!(mounted_inside("/home", "/home/me/games"));
        assert!(mounted_inside("/home/", "/home/me"));
        assert!(!mounted_inside("/home", "/homework"));
        assert!(!mounted_inside("/home", "/home/"));
        assert!(!mounted_inside("", "/home"));
        assert!(mounted_inside("/", "/home"));
        assert!(system_mount("/var/lib/machines"));
        assert!(!system_mount("/variable"));
        assert!(!system_mount("/home"));
        for m in Mode::ALL {
            assert_eq!(Mode::parse(m.cli()), Some(m));
        }
    }

    #[test]
    fn snapshot_fallbacks() {
        let (mut snaps, run) = data();
        // the run's own, then the newest not newer than the run …
        assert_eq!(snapshot_for(&snaps, &run, "@").unwrap().name, "@.20261110T0300");
        assert_eq!(snapshot_for(&snaps, &run, "@games").unwrap().name, "@games.20261109T0300");
        // … then the newest complete one at all (only newer ones are left)
        snaps.retain(|s| s.group != "@games");
        snaps.push(snap("@games", "20261111T0300", 3000, None));
        assert_eq!(snapshot_for(&snaps, &run, "@games").unwrap().name, "@games.20261111T0300");
        // an incomplete one never
        snaps.iter_mut().for_each(|s| if s.group == "@games" { s.readonly = false });
        assert!(snapshot_for(&snaps, &run, "@games").is_none());
    }

    #[test]
    fn other_filesystems_and_unused_overrides() {
        let mut m = manifest();
        m.filesystems[0].uuid = "aaaa".into();
        m.btrbk.as_mut().unwrap().subvolumes.iter_mut().find(|s| s.name == "@data").unwrap().fs_uuid = "bbbb".into();
        let (snaps, run) = data();
        let none = |_: &str| false;
        let w = select(&m, &snaps, &run, Mode::Whole, &[], &[("@home".into(), "@home.20261109T0300".into())], &none).unwrap();
        assert!(!names(&w).contains(&"@data"));
        assert!(!w.skipped.contains(&"@data".to_string()), "its fstab line stays: that filesystem is not touched");
        assert!(w.notes.iter().any(|n| n.starts_with("@data: on another btrfs filesystem")));
        assert!(select(&m, &snaps, &run, Mode::Custom, &["@".into(), "@data".into()], &[], &none).unwrap_err().contains("another btrfs filesystem"));
        // an override of a subvolume left out is said
        let s = select(&m, &snaps, &run, Mode::System, &[], &[("@home".into(), "@home.20261109T0300".into())], &none).unwrap();
        assert!(s.notes.iter().any(|n| n.starts_with("@home: the snapshot")), "{:?}", s.notes);
        // a path out of the top level is refused
        m.btrbk.as_mut().unwrap().subvolumes[0].path = "../x".into();
        assert!(select(&m, &snaps, &run, Mode::Whole, &[], &[], &none).is_err());
        m.btrbk.as_mut().unwrap().subvolumes[0].path = "@".into();
        m.btrbk.as_mut().unwrap().snapshot_dir = "/../../etc".into();
        assert!(snapshot_dir_in_pool(&m).is_err());
    }
}
