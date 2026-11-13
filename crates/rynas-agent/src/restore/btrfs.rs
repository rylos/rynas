//! The btrfs method as a restore source (`docs/research/pc-backup.md` §11.4): the PC's
//! snapshots received by the NAS (sent by `rynas-agent` itself, or by btrbk over SSH),
//! streamed back over the pinned HTTPS connection and **received into the new btrfs pool**.
//!
//! - Versions are runs: the snapshots with one timestamp in their names; each chosen
//!   subvolume can take another snapshot instead (`--subvol NAME=SNAPSHOT`, or the TUI).
//! - Modes over the manifest's subvolumes: whole PC, system only, a custom list, or *add*
//!   (into an existing pool, nothing formatted). The rules come from the mount points: a
//!   subvolume mounted inside another needs that one too (restored with it, or already in
//!   the pool when adding); formatting needs the root subvolume; at least one subvolume.
//! - Each stream goes through the NAS' own stream guard ([`crate::send_stream`]: exactly the
//!   announced snapshot, v1, paths inside the subvolume, clones only from itself) into
//!   `btrfs receive -e --chroot` in an empty staging folder of the pool, which must then hold
//!   exactly that read-only subvolume with the stream's received UUID and a generation.
//! - Then a writable snapshot at the subvolume's own path (an existing one is renamed
//!   `<path>_rynas_old_<time>`, never deleted), and the received snapshot moves into the PC's
//!   snapshot folder with its btrbk name: it stays the parent of the next incremental send
//!   (B2 picks it by its received UUID).
//! - A PC without manifest (older btrbk snapshots): [`super::legacy`].

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rynas_api_types::pcagent::*;
use rynas_api_types::pcbackup::{PcRun, ReceivedSnapshot};

use super::legacy::{self, Legacy};
use super::source::{Fill, PoolTarget, Progress, RestoreSource, Version};
use crate::btrfs;
use crate::http::Client;
use crate::send_stream::{Expect, Guard};

/// Where the streams land in the pool before they are checked (a plain folder at the top
/// level, removed at the end).
pub const STAGING: &str = ".rynas-restore-incoming";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Format; every subvolume of the manifest.
    Whole,
    /// Format; the root subvolume and the ones mounted at system paths.
    System,
    /// Format; the subvolumes named.
    Custom,
    /// No format: the subvolumes named, into the target's existing pool.
    Add,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        Some(match s {
            "whole" => Mode::Whole,
            "system" => Mode::System,
            "custom" => Mode::Custom,
            "add" => Mode::Add,
            _ => return None,
        })
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
    /// Path from the top level.
    pub path: String,
    pub mount: String,
    pub snapshot: String,
    pub received_uuid: String,
    pub referenced: Option<u64>,
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
    m == "/" || SYSTEM_MOUNTS.iter().any(|s| m == *s || m.starts_with(&format!("{s}/")))
}

/// `parent` is a proper mount-point parent of `child`.
fn mounted_inside(parent: &str, child: &str) -> bool {
    if parent.is_empty() || child.is_empty() || parent == child {
        return false;
    }
    parent == "/" || child.starts_with(&format!("{}/", parent.trim_end_matches('/')))
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
/// target pool (adding).
pub fn select(
    m: &Manifest,
    snaps: &[ReceivedSnapshot],
    run: &PcRun,
    mode: Mode,
    names: &[String],
    overrides: &[(String, String)],
    pool_has: &dyn Fn(&str) -> bool,
) -> Result<Selection, String> {
    let layout = m.btrbk.as_ref().map(|b| b.subvolumes.clone()).unwrap_or_default();
    if layout.is_empty() {
        return Err("the manifest lists no btrfs subvolumes of the PC".into());
    }
    let root = layout.iter().find(|s| s.mount == "/");
    let wanted: Vec<&BtrbkSubvolume> = match mode {
        Mode::Whole => layout.iter().collect(),
        Mode::System => layout.iter().filter(|s| system_mount(&s.mount)).collect(),
        Mode::Custom | Mode::Add => {
            let mut v = Vec::new();
            for n in names {
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
        return Err(format!("adding needs the PC's pool with its root subvolume {} ({}); it is not in the target pool", r.name, r.path));
    }
    for c in &wanted {
        let parent = layout.iter().filter(|p| p.name != c.name && mounted_inside(&p.mount, &c.mount)).max_by_key(|p| p.mount.len());
        if let Some(p) = parent
            && !wanted.contains(&p)
            && (mode.formats() || !pool_has(&p.path))
        {
            return Err(format!("{} is mounted inside {} ({} in {}): restore {} too", c.name, p.name, c.mount, p.mount, p.name));
        }
    }
    let mut chosen = Vec::new();
    let mut notes = Vec::new();
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
            path: c.path.trim_start_matches('/').to_string(),
            mount: c.mount.clone(),
            snapshot: snap.name.clone(),
            received_uuid: snap.received_uuid.clone().unwrap_or_default(),
            referenced: snap.size.map(|z| z.referenced_bytes),
        });
    }
    chosen.sort_by_key(|c| (c.path.matches('/').count(), c.path.clone()));
    let skipped = if mode.formats() {
        layout.iter().filter(|s| !wanted.contains(s)).map(|s| s.path.trim_start_matches('/').to_string()).collect()
    } else {
        Vec::new()
    };
    Ok(Selection { mode, chosen, skipped, notes })
}

/// The PC's snapshot folder as a path from the top level of the pool: its `snapshot_dir`
/// (a path on the PC) below the mount whose subvolume holds it; `btrbk_snapshots` at the
/// top level when the manifest has none.
pub fn snapshot_dir_in_pool(m: &Manifest) -> String {
    let sd = m.btrbk.as_ref().map(|b| b.snapshot_dir.trim().to_string()).unwrap_or_default();
    if sd.is_empty() {
        return "btrbk_snapshots".into();
    }
    if !sd.starts_with('/') {
        return sd.trim_matches('/').to_string();
    }
    let mounts: Vec<&MountEntry> = m.root().map(|f| f.mounts.iter().collect()).unwrap_or_default();
    let best = mounts
        .into_iter()
        .filter(|me| me.path == "/" || sd == me.path || sd.starts_with(&format!("{}/", me.path.trim_end_matches('/'))))
        .max_by_key(|me| me.path.len());
    match best {
        Some(me) => {
            let rest = sd[me.path.trim_end_matches('/').len()..].trim_matches('/');
            let base = me.subvol.as_deref().unwrap_or("").trim_matches('/');
            [base, rest].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("/")
        }
        None => sd.trim_matches('/').to_string(),
    }
}

/// Bytes read from the decoded stream (progress).
struct Counting<R> {
    inner: R,
    n: Arc<AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let k = self.inner.read(b)?;
        self.n.fetch_add(k as u64, Ordering::Relaxed);
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
    messages: Vec<String>,
}

fn say(v: &mut Vec<String>, s: String) {
    eprintln!("{s}");
    v.push(s);
}

impl BtrfsSource {
    /// The PC's snapshots on the NAS, with a restore session's client.
    pub fn new(client: Arc<Client>, dev: RestoreDevice) -> Result<BtrfsSource, String> {
        let listing: RestoreBtrfsListing = client.get_json(&format!("{RESTORE_BTRFS}/{}/snapshots", dev.device))?;
        Ok(BtrfsSource { client, dev, listing, legacy: None, manifest: None, selection: None, esp_mib: 1024, messages: Vec::new() })
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

    /// Chooses what to restore of version `v` (see [`select`]).
    pub fn choose(&mut self, v: &Version, mode: Mode, names: &[String], overrides: &[(String, String)], pool_has: &dyn Fn(&str) -> bool) -> Result<&Selection, String> {
        let m = self.manifest.as_ref().ok_or("read the version's manifest first")?;
        let run = self.run(&v.id).ok_or_else(|| format!("{}: no such run", v.id))?;
        let mut s = select(m, &self.listing.snapshots, run, mode, names, overrides, pool_has)?;
        if let Some(l) = &self.legacy {
            s.notes.splice(0..0, l.notes.iter().cloned());
        }
        self.selection = Some(s);
        Ok(self.selection.as_ref().expect("set above"))
    }

    /// The manifest the plan is built from: the root filesystem's used bytes are the chosen
    /// snapshots' referenced bytes when known (the "fits" check: they plus 10 % must fit the
    /// new btrfs partition; a smaller disk is fine).
    pub fn planned_manifest(&self, m: &Manifest) -> Manifest {
        let mut m = m.clone();
        if let Some(b) = self.selection.as_ref().and_then(Selection::referenced).filter(|b| *b > 0)
            && let Some(f) = m.filesystems.iter_mut().find(|f| f.mounts.iter().any(|x| x.path == "/"))
        {
            f.used_bytes = b;
        }
        m
    }

    /// Receives one snapshot into `dir` (an empty folder of the pool) and checks it.
    fn receive(&self, c: &Choice, dir: &Path, done: &Arc<AtomicU64>) -> Result<PathBuf, String> {
        let path = format!("{RESTORE_BTRFS}/{}/send?snapshot={}", self.dev.device, c.snapshot);
        let body = match self.client.get_stream(&path, &[("Accept", "application/zstd")], std::time::Duration::from_secs(600)) {
            Ok(Ok(b)) => b,
            Ok(Err(r)) => return Err(format!("{}: {}", c.snapshot, r.error_text())),
            Err(e) => return Err(format!("{}: {e}", self.client.ep.url())),
        };
        let mut dec = zstd::stream::read::Decoder::new(body).map_err(|e| format!("zstd: {e}"))?;
        dec.window_log_max(AGENT_ZSTD_WINDOW_LOG_MAX).map_err(|e| format!("zstd: {e}"))?;
        let counted = Counting { inner: dec, n: done.clone() };
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
            return Err(format!("{}: the stream from the NAS failed: {e}", c.snapshot));
        }
        if !status.success() {
            let last = text.lines().rfind(|l| !l.starts_with("At ") && !l.starts_with("Chroot to ")).unwrap_or("").trim().to_string();
            return Err(format!("{}: btrfs receive: {}", c.snapshot, if last.is_empty() { status.to_string() } else { last }));
        }
        if !guard.seen.ended {
            return Err(format!("{}: the stream did not end", c.snapshot));
        }
        // exactly the announced subvolume, read-only, with the stream's received UUID and data
        let names: Vec<String> = std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        if names != [c.snapshot.clone()] {
            return Err(format!("{}: the receive left {:?}, not just the snapshot", c.snapshot, names));
        }
        let got = dir.join(&c.snapshot);
        let i = btrfs::subvol_info(&got).ok_or_else(|| format!("{}: not a subvolume after the receive", got.display()))?;
        if !i.readonly || i.received_uuid.is_none() || i.received_uuid != guard.seen.uuid {
            return Err(format!("{}: the received snapshot is not read-only with the stream's received UUID", c.snapshot));
        }
        if i.generation == 0 {
            return Err(format!("{}: the received snapshot is empty (generation 0)", c.snapshot));
        }
        Ok(got)
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
        let rootish = |n: &str| !matches!(n.rsplit_once('.').map(|x| x.0).unwrap_or(n), "@" | "root" | "rootfs" | "@root");
        let mut names: Vec<String> = run.snapshots.iter().filter(|n| self.listing.snapshots.iter().any(|s| &&s.name == n && complete(s))).cloned().collect();
        names.sort_by_key(|n| rootish(n));
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
            if r.body.len() > MAX_MANIFEST {
                return Err("the manifest is too large".into());
            }
            let m: Manifest = serde_json::from_slice(&r.body).map_err(|e| format!("the manifest of {n} does not parse: {e}"))?;
            if m.format != MANIFEST_FORMAT {
                return Err(format!("the manifest of {n} has format {}, this agent reads {MANIFEST_FORMAT}", m.format));
            }
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

    fn fill_pool(&mut self, _v: &Version, pools: &[PoolTarget], p: Progress) -> Result<(), String> {
        let mut msgs = Vec::new();
        let r = self.fill_inner(pools, p, &mut msgs);
        self.messages.extend(msgs);
        r
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
            let mode = match s.mode {
                Mode::Whole => "whole PC",
                Mode::System => "system only",
                Mode::Custom => "chosen subvolumes",
                Mode::Add => "add subvolumes (nothing formatted)",
            };
            out.push(format!("btrfs: {mode}"));
            for c in &s.chosen {
                let size = c.referenced.map(|b| format!(", {} MiB", b >> 20)).unwrap_or_default();
                out.push(format!("  receive {} → writable {} ({}){size}", c.snapshot, c.path, if c.mount.is_empty() { "not mounted" } else { c.mount.as_str() }));
            }
            for k in &s.skipped {
                out.push(format!("  not restored: {k} (its fstab line is commented: {})", super::plan::RESTORE_MARK.trim()));
            }
            if let Some(m) = &self.manifest {
                out.push(format!("  received snapshots kept in {} (parents of the next incremental backups)", snapshot_dir_in_pool(m)));
            }
            for n in &s.notes {
                out.push(format!("note: {n}"));
            }
        }
        out
    }

    fn drain_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.messages)
    }
}

impl BtrfsSource {
    fn fill_inner(&self, pools: &[PoolTarget], p: Progress, msgs: &mut Vec<String>) -> Result<(), String> {
        use std::os::unix::fs::DirBuilderExt;
        let sel = self.selection.clone().ok_or("no subvolumes chosen")?;
        let m = self.manifest.clone().ok_or("no manifest")?;
        let first = sel.chosen.first().ok_or("no subvolumes chosen")?;
        let pool = pools.iter().find(|x| x.subvolumes.iter().any(|s| s.path.trim_start_matches('/') == first.path)).or(pools.first()).ok_or("no btrfs filesystem to fill")?;
        let top = pool.top_level.clone();
        let staging = top.join(STAGING);
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
        let total: u64 = sel.chosen.iter().filter_map(|c| c.referenced).sum();
        let done = Arc::new(AtomicU64::new(0));
        let ts = crate::state::now_ms() / 1000;
        let mut received: Vec<(PathBuf, &Choice)> = Vec::new();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = std::thread::scope(|sc| -> Result<(), String> {
            let (d2, s2) = (done.clone(), stop.clone());
            sc.spawn(move || {
                while !s2.load(Ordering::SeqCst) {
                    p(d2.load(Ordering::Relaxed), total.max(d2.load(Ordering::Relaxed)));
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            });
            let r = (|| {
                for (i, c) in sel.chosen.iter().enumerate() {
                    say(msgs, format!("receiving {} ({}) from the NAS", c.snapshot, c.mount));
                    let dir = staging.join(i.to_string());
                    std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                    let got = self.receive(c, &dir, &done)?;
                    // the writable subvolume at its own path
                    let target = top.join(&c.path);
                    if let Ok(meta) = std::fs::symlink_metadata(&target) {
                        let empty_dir = meta.is_dir() && btrfs::subvol_info(&target).is_none() && std::fs::read_dir(&target).map(|mut r| r.next().is_none()).unwrap_or(false);
                        if empty_dir {
                            // the place a nested subvolume leaves in its parent's snapshot
                            std::fs::remove_dir(&target).map_err(|e| format!("{}: {e}", target.display()))?;
                        } else {
                            let old = PathBuf::from(format!("{}_rynas_old_{ts}", target.display()));
                            std::fs::rename(&target, &old).map_err(|e| format!("renaming {}: {e}", target.display()))?;
                            say(msgs, format!("{} was there: renamed {}, not deleted", c.path, old.display()));
                        }
                    }
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
                    }
                    btrfs::snapshot_rw(&got, &target)?;
                    say(msgs, format!("{}: writable from {}", c.path, c.snapshot));
                    received.push((got, c));
                }
                Ok(())
            })();
            stop.store(true, Ordering::SeqCst);
            r
        });
        eprintln!();
        result?;
        // the received snapshots into the PC's snapshot folder: the parents of the next sends
        let sd = top.join(snapshot_dir_in_pool(&m));
        std::fs::create_dir_all(&sd).map_err(|e| format!("{}: {e}", sd.display()))?;
        for (got, c) in &received {
            let to = sd.join(&c.snapshot);
            if to.exists() {
                say(msgs, format!("{} is already in {}: the received copy stays in {}", c.snapshot, sd.display(), got.display()));
                continue;
            }
            btrfs::move_readonly(got, &to)?;
            let i = btrfs::subvol_info(&to).ok_or_else(|| format!("{}: lost after the move", to.display()))?;
            if !i.readonly || i.received_uuid.is_none() {
                return Err(format!("{}: not read-only with its received UUID after the move", to.display()));
            }
            if i.received_uuid.as_deref() != Some(c.received_uuid.as_str()) {
                say(msgs, format!("{}: its received UUID differs from the NAS copy's: the next backup of it is a full one", c.snapshot));
            }
            say(msgs, format!("{} kept in {} (parent of the next incremental backup)", c.snapshot, sd.display()));
            if let Some(dir) = got.parent() {
                let _ = std::fs::remove_dir(dir);
            }
        }
        let _ = std::fs::remove_dir(&staging);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::pcbackup::SnapshotSize;

    fn manifest() -> Manifest {
        let sub = |n: &str, p: &str, m: &str| BtrbkSubvolume { name: n.into(), path: p.into(), mount: m.into() };
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
        assert_eq!(snapshot_dir_in_pool(&m), "@/.btrbk_snapshots");
        m.btrbk.as_mut().unwrap().snapshot_dir = "/mnt/pool/btrbk_snapshots".into();
        m.filesystems[0].mounts.push(MountEntry { path: "/mnt/pool".into(), subvol: None, ..Default::default() });
        assert_eq!(snapshot_dir_in_pool(&m), "btrbk_snapshots");
        m.btrbk.as_mut().unwrap().snapshot_dir = "/home/.snaps".into();
        assert_eq!(snapshot_dir_in_pool(&m), "@home/.snaps");
        m.btrbk.as_mut().unwrap().snapshot_dir = String::new();
        assert_eq!(snapshot_dir_in_pool(&m), "btrbk_snapshots");
    }
}
