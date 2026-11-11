//! Folders of the rynas volumes: which top-level folders no share uses ("Unshared
//! folders"), what deleting a folder removes (the plan shown before), and the deletion
//! itself (snapshot folder unmounted, snapshots, then the folder with its recycle bin).
//!
//! Only folders directly on a rynas volume (`/<volume>/<name>`) can be deleted: a share may
//! point at any directory (`/home/alice`, `/srv`), and rynas never deletes those.
//! The deletion runs in a task: at the end of the apply of `shares` for a share deleted
//! with its folder ([`rynas_api_types::SharesConfig::remove_folders`]), or as task
//! `shares.folder-delete` for a folder without a share. Commands go through
//! [`rynas_config::System::run`], so `--dry-system` and the tests only record them.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rynas_api_types::{
    FolderDeletePlan, FolderUsage, FolderUsageKind, SnapshotSource, UnsharedFolder,
};
use rynas_config::System;
use rynas_tasks::Cmd;

use crate::host::ShareHost;
use crate::recycle::RECYCLE_DIR;
use crate::removed::RemovedShare;
use crate::snapview;

/// Time spent measuring one folder for a plan.
pub const PLAN_SIZE_BUDGET: Duration = Duration::from_millis(1500);
/// Time spent measuring one folder of the list, and all of them together.
pub const LIST_SIZE_BUDGET: Duration = Duration::from_millis(500);
pub const LIST_TOTAL_BUDGET: Duration = Duration::from_secs(3);
/// How long a measured size of the list is served without measuring again (the cache is
/// also cleared by every apply of `shares` and every folder deletion).
pub const SIZE_TTL_MS: u64 = 3600 * 1000;
/// Most sizes kept (the oldest goes first): one per top-level folder of the volumes.
pub const SIZE_CACHE_MAX: usize = 1024;
/// Deleting a big folder takes long: the task log shows the command meanwhile.
const DELETE_TIMEOUT: Duration = Duration::from_secs(6 * 3600);

/// Names rynas and the system keep at the top of a volume: hidden (`.snapshots`,
/// `.receive`), internal (`@rynas-cache`, `@eaDir`), recycle bins and snapshot folders
/// (`#recycle`, `#snapshot`), `lost+found`.
pub fn is_system_name(n: &str) -> bool {
    n.starts_with('.') || n.starts_with('@') || n.starts_with('#') || n == "lost+found"
}

/// `(volume, name)` when `path` is a folder directly on one of `volumes` (label, mount
/// point) with a name rynas may delete.
pub fn on_volume(volumes: &[(String, String)], path: &str) -> Option<(String, String)> {
    let (parent, name) = path.trim_end_matches('/').rsplit_once('/')?;
    if name.is_empty() || is_system_name(name) || name == ".." {
        return None;
    }
    volumes
        .iter()
        .find(|(_, m)| m.trim_end_matches('/') == parent && !parent.is_empty())
        .map(|(l, _)| (l.clone(), name.to_string()))
}

/// `a` is `b` or inside it.
pub fn is_within(a: &str, b: &str) -> bool {
    let b = b.trim_end_matches('/');
    a == b || a.starts_with(&format!("{b}/"))
}

fn count_snapshots(host: &dyn ShareHost, sources: &[SnapshotSource]) -> u32 {
    sources
        .iter()
        .map(|s| host.snapshot_names(&s.dir, &s.prefix).len() as u32)
        .sum()
}

/// A size as measured: bytes, partial (the time budget ran out), when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedSize {
    pub bytes: u64,
    pub partial: bool,
    pub at_ms: u64,
    /// Read from the btrfs quota group (no walk of the files).
    pub exact: bool,
}

/// Sizes of the unshared folders, so that opening the page does not walk every file of
/// every folder each time (and never spins up a sleeping disk to do it). Capped at
/// [`SIZE_CACHE_MAX`] entries, the oldest evicted first.
#[derive(Debug, Default)]
pub struct SizeCache {
    m: Mutex<HashMap<String, CachedSize>>,
}

impl SizeCache {
    pub fn get(&self, path: &str) -> Option<CachedSize> {
        self.m.lock().unwrap().get(path).copied()
    }

    pub fn put(&self, path: &str, s: CachedSize) {
        let mut m = self.m.lock().unwrap();
        if m.len() >= SIZE_CACHE_MAX && !m.contains_key(path) {
            let oldest = m.iter().min_by_key(|(_, v)| v.at_ms).map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                m.remove(&k);
            }
        }
        m.insert(path.to_string(), s);
    }

    /// Forgets every size (after a change of the shares or a folder deletion).
    pub fn clear(&self) {
        self.m.lock().unwrap().clear();
    }

    pub fn len(&self) -> usize {
        self.m.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Measure the sizes the cache does not have fresh ([`unshared`]).
pub struct Measure<'a> {
    pub now_ms: u64,
    /// A disk of the volume mounted there sleeps ([`crate::SubvolumeCreator::asleep`]).
    pub asleep: &'a dyn Fn(&str) -> bool,
}

fn measure_one(host: &dyn ShareHost, path: &str, subvolume: bool, now_ms: u64) -> Option<CachedSize> {
    measure_within(host, path, subvolume, now_ms, LIST_SIZE_BUDGET)
}

/// [`measure_one`] with the time budget of the walk.
fn measure_within(
    host: &dyn ShareHost,
    path: &str,
    subvolume: bool,
    now_ms: u64,
    budget: Duration,
) -> Option<CachedSize> {
    if subvolume && let Some(bytes) = host.qgroup_size(path) {
        return Some(CachedSize {
            bytes,
            partial: false,
            at_ms: now_ms,
            exact: true,
        });
    }
    host.tree_size(path, budget)
        .map(|(bytes, partial)| CachedSize {
            bytes,
            partial,
            at_ms: now_ms,
            exact: false,
        })
}

/// The top-level folders of the volumes that no share uses. `share_paths`: paths of the
/// shares (applied and staged); `snapshots_of`: the snapshot directories of a path.
///
/// Sizes come from `sizes`. Without `measure` nothing is measured (the list is quick: the
/// last known sizes, any age). With it, sizes older than [`SIZE_TTL_MS`] or unknown are
/// measured and cached, within [`LIST_TOTAL_BUDGET`], except on a volume whose disks
/// sleep: there the last known size stays, marked `size_standby`.
pub fn unshared(
    host: &dyn ShareHost,
    volumes: &[(String, String)],
    share_paths: &[String],
    removed: &[RemovedShare],
    snapshots_of: &dyn Fn(&str) -> Vec<SnapshotSource>,
    sizes: &SizeCache,
    measure: Option<&Measure>,
) -> Vec<UnsharedFolder> {
    let start = Instant::now();
    let mut out = Vec::new();
    for (label, mnt) in volumes {
        let mut asleep: Option<bool> = None;
        for e in host.list_dir(mnt) {
            if !e.is_dir || is_system_name(&e.name) {
                continue;
            }
            let path = format!("{}/{}", mnt.trim_end_matches('/'), e.name);
            // used: a share on it, or on something inside it
            if share_paths.iter().any(|p| is_within(p, &path)) {
                continue;
            }
            let mut size = sizes.get(&path);
            let mut standby = false;
            if let Some(m) = measure
                && size.is_none_or(|s| m.now_ms.saturating_sub(s.at_ms) >= SIZE_TTL_MS)
            {
                // the power check only when a size is due, once per volume
                if *asleep.get_or_insert_with(|| (m.asleep)(mnt)) {
                    standby = true;
                } else if start.elapsed() < LIST_TOTAL_BUDGET
                    && let Some(s) = measure_one(host, &path, e.subvolume, m.now_ms)
                {
                    sizes.put(&path, s);
                    size = Some(s);
                }
            }
            let rec = removed.iter().rev().find(|r| r.share.path == path);
            out.push(UnsharedFolder {
                volume: label.clone(),
                name: e.name.clone(),
                subvolume: e.subvolume,
                size_bytes: size.map(|s| s.bytes),
                size_partial: size.is_some_and(|s| s.partial),
                size_at_ms: size.map(|s| s.at_ms),
                size_standby: standby,
                snapshots: count_snapshots(host, &snapshots_of(&path)),
                removed_at_ms: rec.map(|r| r.removed_at_ms),
                previous: rec.map(|r| r.share.clone()),
                path,
            });
        }
    }
    out.sort_by(|a, b| (&a.volume, &a.name).cmp(&(&b.volume, &b.name)));
    out
}

/// One slice of the background walk of a share's folder: then the folder goes to the back of
/// the queue, so that one huge share never starves the others, and its walk goes on where
/// it stopped at its next turn ([`crate::host::TreeWalk`]). Until the walk is done, a folder
/// without an earlier complete size shows the bytes counted so far as "at least"
/// ([`CachedSize::partial`]).
pub const SHARE_WALK_SLICE: Duration = Duration::from_secs(60);
/// A size read from a btrfs quota group is served this long (reading it is cheap).
pub const EXACT_TTL_MS: u64 = 60 * 1000;

/// A share whose size is asked ([`ShareSizeMeter::report`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareFolder {
    pub name: String,
    pub path: String,
    /// The directory measured: `path`, or the ciphertext of an encrypted folder (its size
    /// does not depend on the lock and is read without FUSE).
    pub dir: String,
    /// `dir` is a btrfs subvolume (its quota group may give the size).
    pub subvolume: bool,
}

/// "A disk under this path sleeps", for the background thread.
pub type AsleepFn = std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Sizes of the shares' folders for the shares list. The list never waits: it gets the
/// sizes known now, and the unknown or old ones are measured one at a time by a background
/// thread at idle IO priority (quota group of a subvolume when quotas are on, else a walk in
/// slices of [`SHARE_WALK_SLICE`] that resumes until it is done). A folder on a sleeping
/// disk is never read. Unlike the sizes of the unshared folders, these survive an apply of
/// `shares` (they only age).
#[derive(Debug, Default)]
pub struct ShareSizeMeter {
    cache: SizeCache,
    queue: Mutex<SizeQueue>,
    /// Walks begun and not done yet, by folder.
    walks: Mutex<HashMap<String, crate::host::TreeWalk>>,
}

#[derive(Debug, Default)]
struct SizeQueue {
    /// Paths to measure, in order, with their subvolume flag.
    waiting: std::collections::VecDeque<(String, bool)>,
    /// Being measured now.
    current: Option<String>,
    /// A thread runs the queue.
    running: bool,
}

impl SizeQueue {
    fn has(&self, path: &str) -> bool {
        self.current.as_deref() == Some(path) || self.waiting.iter().any(|(p, _)| p == path)
    }
}

/// A cached size must be measured again (unknown, or older than its time to live).
fn size_due(s: Option<CachedSize>, now_ms: u64) -> bool {
    s.is_none_or(|s| {
        let ttl = if s.exact { EXACT_TTL_MS } else { SIZE_TTL_MS };
        now_ms.saturating_sub(s.at_ms) >= ttl
    })
}

/// The volume (label) holding `path`: the one with the longest folder containing it.
fn volume_of(volumes: &[(String, String)], path: &str) -> Option<String> {
    volumes
        .iter()
        .filter(|(_, m)| is_within(path, m))
        .max_by_key(|(_, m)| m.len())
        .map(|(l, _)| l.clone())
}

/// Clears `running` when the thread ends, even by a panic, so that a later list starts a
/// new one.
struct RunGuard<'a>(&'a Mutex<SizeQueue>);

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        let mut q = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        q.running = false;
        q.current = None;
    }
}

impl ShareSizeMeter {
    /// The sizes of `shares` as known now. Those due ([`size_due`]) are queued, except on a
    /// sleeping disk (`asleep(path)`, then `standby`). The `bool` is true when the caller
    /// must start the thread ([`ShareSizeMeter::start`]): there is work and none runs.
    pub fn report(
        &self,
        volumes: &[(String, String)],
        shares: &[ShareFolder],
        now_ms: u64,
        asleep: &dyn Fn(&str) -> bool,
    ) -> (rynas_api_types::ShareSizes, bool) {
        let mut q = self.queue.lock().unwrap();
        let mut out = Vec::with_capacity(shares.len());
        for f in shares {
            let size = self.cache.get(&f.dir);
            let mut measuring = q.has(&f.dir);
            let mut standby = false;
            if !measuring && size_due(size, now_ms) {
                if asleep(&f.dir) {
                    standby = true;
                } else {
                    q.waiting.push_back((f.dir.clone(), f.subvolume));
                    measuring = true;
                }
            }
            out.push(rynas_api_types::ShareSize {
                name: f.name.clone(),
                path: f.path.clone(),
                volume: volume_of(volumes, &f.path),
                bytes: size.map(|s| s.bytes),
                partial: size.is_some_and(|s| s.partial),
                exact: size.is_some_and(|s| s.exact),
                at_ms: size.map(|s| s.at_ms),
                standby,
                measuring,
            });
        }
        let start = !q.running && !q.waiting.is_empty();
        if start {
            q.running = true;
        }
        let measuring = out.iter().any(|s| s.measuring);
        (rynas_api_types::ShareSizes { shares: out, measuring }, start)
    }

    /// Measures the queued folders until none is left: the body of the background thread
    /// (the tests call it directly). A folder whose disk fell asleep meanwhile is skipped
    /// (its walk waits for the next time it is queued). Walks go in slices
    /// ([`ShareSizeMeter::run_sliced`] with [`SHARE_WALK_SLICE`]).
    pub fn run(&self, host: &dyn ShareHost, asleep: &dyn Fn(&str) -> bool, now_ms: &dyn Fn() -> u64) {
        self.run_sliced(host, asleep, now_ms, SHARE_WALK_SLICE)
    }

    /// [`ShareSizeMeter::run`] with slices of `slice`.
    pub fn run_sliced(&self, host: &dyn ShareHost, asleep: &dyn Fn(&str) -> bool, now_ms: &dyn Fn() -> u64, slice: Duration) {
        let _guard = RunGuard(&self.queue);
        loop {
            let (path, subvolume) = {
                let mut q = self.queue.lock().unwrap();
                let Some(job) = q.waiting.pop_front() else {
                    return;
                };
                q.current = Some(job.0.clone());
                job
            };
            let more = !asleep(&path) && self.measure_slice(host, &path, subvolume, now_ms(), slice);
            let mut q = self.queue.lock().unwrap();
            q.current = None;
            if more {
                // the rest of the walk at its next turn, behind the others
                q.waiting.push_back((path, subvolume));
            }
        }
    }

    /// One turn of `path`: its quota group, or one slice of its walk. `true` when the walk
    /// goes on at a later turn.
    fn measure_slice(&self, host: &dyn ShareHost, path: &str, subvolume: bool, now: u64, slice: Duration) -> bool {
        if subvolume && let Some(bytes) = host.qgroup_size(path) {
            self.walks.lock().unwrap().remove(path);
            self.cache.put(path, CachedSize { bytes, partial: false, at_ms: now, exact: true });
            return false;
        }
        let taken = self.walks.lock().unwrap().remove(path);
        // a walk begun longer ago than a size lives counts a tree that has changed since
        let mut walk = taken
            .filter(|w| now.saturating_sub(w.started_ms) < SIZE_TTL_MS)
            .unwrap_or_else(|| crate::host::TreeWalk::new(path, now));
        match host.tree_walk(&mut walk, slice) {
            None => false,
            Some(true) => {
                self.cache.put(path, CachedSize { bytes: walk.total, partial: walk.incomplete, at_ms: now, exact: false });
                false
            }
            Some(false) => {
                // the bytes so far, "at least", unless an earlier complete size is known: that
                // one stays until this walk is done
                if self.cache.get(path).is_none_or(|s| s.partial) {
                    self.cache.put(path, CachedSize { bytes: walk.total, partial: true, at_ms: now, exact: false });
                }
                self.walks.lock().unwrap().insert(path.to_string(), walk);
                true
            }
        }
    }

    /// Runs the queue in a thread named `share-sizes`, at idle IO priority and lowest CPU
    /// priority, after [`ShareSizeMeter::report`] asked for it.
    pub fn start(self: &std::sync::Arc<Self>, host: std::sync::Arc<dyn ShareHost>, asleep: AsleepFn) {
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name("share-sizes".into())
            .spawn(move || {
                background_priority();
                me.run(host.as_ref(), asleep.as_ref(), &rynas_tasks::now_ms);
            });
        if let Err(e) = spawned {
            tracing::warn!("share sizes: cannot start the thread: {e}");
            let mut q = self.queue.lock().unwrap();
            q.running = false;
            q.waiting.clear();
        }
    }

    /// Nothing queued or being measured.
    pub fn idle(&self) -> bool {
        let q = self.queue.lock().unwrap();
        !q.running && q.waiting.is_empty()
    }
}

/// The calling thread reads the disk only when nobody else does (IO class idle) and gets
/// the CPU last (nice 19): a walk of millions of files never slows the NAS down.
fn background_priority() {
    // SAFETY: plain syscalls on the calling thread, no memory passed
    unsafe {
        // ioprio_set(IOPRIO_WHO_PROCESS, 0 = this thread, IOPRIO_CLASS_IDLE << 13)
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
        // per thread on Linux: the thread id as the "process"
        libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 19);
    }
}

/// What uses a folder besides the share being deleted, collected by the caller.
#[derive(Debug, Default)]
pub struct Usages {
    /// The share deleted with the folder.
    pub share: Option<String>,
    /// That share has an NFS export.
    pub own_nfs: bool,
    /// Other shares on the folder or inside it.
    pub shares: Vec<String>,
    /// Snapshot jobs removed together with the share.
    pub jobs_removed: Vec<String>,
    /// Snapshot jobs that would stay and fail.
    pub jobs_kept: Vec<String>,
    /// Scheduled tasks naming the folder or the share.
    pub tasks: Vec<String>,
    /// Containers mounting the folder.
    pub containers: Vec<String>,
}

/// The plan of deleting `path` (no side effect).
pub fn plan(
    host: &dyn ShareHost,
    volumes: &[(String, String)],
    path: &str,
    snapshots: &[SnapshotSource],
    u: Usages,
) -> FolderDeletePlan {
    let info = host.probe(path);
    let size = host.tree_size(path, PLAN_SIZE_BUDGET);
    let mut usages = Vec::new();
    let mut blockers = Vec::new();
    let mut add = |kind, name: &str, blocking: bool| {
        let x = FolderUsage {
            kind,
            name: name.to_string(),
        };
        if blocking {
            blockers.push(x.clone());
        }
        usages.push(x);
    };
    if on_volume(volumes, path).is_none() {
        add(FolderUsageKind::NotOnVolume, path, true);
    }
    if u.own_nfs {
        add(
            FolderUsageKind::NfsExport,
            u.share.as_deref().unwrap_or(""),
            false,
        );
    }
    for s in &u.shares {
        add(FolderUsageKind::Share, s, true);
    }
    for j in &u.jobs_removed {
        add(FolderUsageKind::SnapshotJob, j, false);
    }
    for j in &u.jobs_kept {
        add(FolderUsageKind::SnapshotJob, j, true);
    }
    for t in &u.tasks {
        add(FolderUsageKind::ScheduledTask, t, false);
    }
    for c in &u.containers {
        add(FolderUsageKind::Container, c, false);
    }
    let recycle_bin = host
        .list_dir(path)
        .iter()
        .any(|e| e.is_dir && e.name == RECYCLE_DIR);
    FolderDeletePlan {
        path: path.to_string(),
        share: u.share,
        subvolume: info.subvolume,
        size_bytes: size.map(|s| s.0).unwrap_or(0),
        size_partial: size.is_none_or(|s| s.1),
        snapshots: count_snapshots(host, snapshots),
        recycle_bin,
        // a symlink would make `btrfs subvolume delete` act on its target
        allowed: blockers.is_empty() && info.exists && info.is_dir && !info.symlink,
        usages,
        blockers,
    }
}

fn run(system: &dyn System, log: &dyn Fn(&str), cmd: Cmd) -> Result<(), String> {
    log(&format!("$ {}", cmd.display()));
    system.run(&cmd.env_clear().timeout(DELETE_TIMEOUT)).map(|_| ())
}

/// Deletes `path` for good: its snapshot folder (unmounted), its snapshots, then the folder
/// with everything in it (recycle bin included). `volumes` is checked again here: only a
/// folder directly on a rynas volume goes.
pub fn remove_folder(
    system: &dyn System,
    host: &dyn ShareHost,
    crypt: &dyn crate::crypt::CryptHost,
    volumes: &[(String, String)],
    path: &str,
    snapshots: &[SnapshotSource],
    log: &dyn Fn(&str),
) -> Result<(), String> {
    if on_volume(volumes, path).is_none() {
        return Err(format!(
            "{path} is not a folder directly on a rynas volume: not deleted"
        ));
    }
    // an encrypted folder: locked (refused while busy or still served elsewhere), then the
    // snapshots of its ciphertext (the caller lists those of the jobs on the ciphertext
    // directory too), then its mount point, ciphertext (nested subvolumes included) and keys
    if crypt.state(path) != rynas_api_types::CryptState::Missing {
        log(&format!("{path} is an encrypted folder: locking it and deleting its ciphertext"));
        crypt.lock(path, log).map_err(|e| format!("{path}: {e}"))?;
        delete_snapshots(system, host, snapshots, log)?;
        crypt.remove(path, log).map_err(|e| format!("{path}: {e}"))?;
        log(&format!("{path} deleted"));
        return Ok(());
    }
    let info = host.probe(path);
    if info.symlink {
        // `btrfs subvolume delete` resolves the path: it would delete the link's target
        return Err(format!("{path} is a symbolic link: not deleted"));
    }
    if !info.exists {
        log(&format!("{path} does not exist any more"));
    }
    // the snapshot folder first: a mounted folder cannot be deleted
    let unit = snapview::unit_name(path);
    if system.unit_active(&unit).unwrap_or(false) {
        log(&format!("stopping {unit}"));
        system.stop_unit(&unit)?;
    }
    delete_snapshots(system, host, snapshots, log)?;
    if info.exists {
        if info.subvolume {
            run(
                system,
                log,
                Cmd::new("btrfs").args(["subvolume", "delete", "--recursive", path]),
            )?;
        } else {
            run(
                system,
                log,
                Cmd::new("rm").args([
                    "-rf",
                    "--one-file-system",
                    "--preserve-root=all",
                    "--",
                    path,
                ]),
            )?;
        }
        log(&format!("{path} deleted"));
    }
    Ok(())
}

/// Deletes the snapshots of `snapshots` (each a read-only subvolume), then their folders
/// when empty.
fn delete_snapshots(
    system: &dyn System,
    host: &dyn ShareHost,
    snapshots: &[SnapshotSource],
    log: &dyn Fn(&str),
) -> Result<(), String> {
    for s in snapshots {
        let names = host.snapshot_names(&s.dir, &s.prefix);
        if names.is_empty() {
            continue;
        }
        log(&format!(
            "deleting {} snapshots in {}",
            names.len(),
            s.dir
        ));
        for n in names {
            let p = format!("{}/{n}", s.dir.trim_end_matches('/'));
            run(
                system,
                log,
                Cmd::new("btrfs").args(["subvolume", "delete", &p]),
            )?;
        }
        host.remove_empty_dir(&s.dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::MemoryShares;
    use rynas_config::MemorySystem;

    fn vols() -> Vec<(String, String)> {
        vec![("vol1".into(), "/vol1".into())]
    }

    fn host() -> MemoryShares {
        let h = MemoryShares::new();
        h.add_dir("/vol1", Some("00000000-0000-4000-8000-000000000001"));
        h.add_dir("/vol1/photos", Some("00000000-0000-4000-8000-000000000002"));
        h.add_dir("/vol1/photos/#recycle", None);
        h.add_dir("/vol1/old", None);
        h.add_dir("/vol1/.snapshots", Some("00000000-0000-4000-8000-000000000003"));
        h.add_dir("/vol1/@rynas-cache", None);
        h.add_dir("/vol1/data", None);
        h.add_dir("/vol1/data/inner", None);
        h.state().sizes.insert("/vol1/photos".into(), 5000);
        h.state().snapshots.insert(
            "/vol1/.snapshots/photos".into(),
            vec![
                "photos.20261104T1200".into(),
                "photos.20261105T1200".into(),
                "stray".into(),
            ],
        );
        h
    }

    fn src() -> Vec<SnapshotSource> {
        vec![SnapshotSource {
            dir: "/vol1/.snapshots/photos".into(),
            prefix: "photos".into(),
        }]
    }

    #[test]
    fn only_folders_directly_on_a_volume_can_go() {
        let v = vols();
        assert_eq!(
            on_volume(&v, "/vol1/photos"),
            Some(("vol1".into(), "photos".into()))
        );
        for p in ["/vol1", "/vol1/a/b", "/srv/photos", "/vol1/.snapshots", "/vol1/#recycle", "/"] {
            assert_eq!(on_volume(&v, p), None, "{p}");
        }
        assert!(is_within("/vol1/data/x", "/vol1/data"));
        assert!(!is_within("/vol1/database", "/vol1/data"));
    }

    #[test]
    fn unshared_folders_leave_out_shares_and_system_names() {
        let h = host();
        let removed = vec![RemovedShare {
            share: rynas_api_types::Share {
                name: "Photos".into(),
                path: "/vol1/photos".into(),
                ..Default::default()
            },
            removed_at_ms: 42,
        }];
        let cache = SizeCache::default();
        let awake = |_: &str| false;
        let m = Measure {
            now_ms: 1000,
            asleep: &awake,
        };
        let l = unshared(
            &h,
            &vols(),
            &["/vol1/data/inner".into()],
            &removed,
            &|p| if p == "/vol1/photos" { src() } else { Vec::new() },
            &cache,
            Some(&m),
        );
        let names: Vec<&str> = l.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["old", "photos"], "data holds a share, system names stay out");
        let p = &l[1];
        assert_eq!(p.size_bytes, Some(5000));
        assert_eq!(p.snapshots, 2);
        assert_eq!(p.removed_at_ms, Some(42));
        assert_eq!(p.previous.as_ref().unwrap().name, "Photos");
        assert!(p.subvolume);
    }

    #[test]
    fn sizes_are_cached_invalidated_and_never_read_from_sleeping_disks() {
        let h = host();
        let cache = SizeCache::default();
        let none = |_: &str| Vec::new();
        let list = |m: Option<&Measure>| unshared(&h, &vols(), &[], &[], &none, &cache, m);
        let walks = || std::mem::take(&mut h.state().size_log);
        fn awake(_: &str) -> bool {
            false
        }
        fn asleep(m: &str) -> bool {
            m == "/vol1"
        }
        let at = |now_ms, asleep: &'static dyn Fn(&str) -> bool| Measure { now_ms, asleep };
        let size_of = |l: &[UnsharedFolder], n: &str| {
            let f = l.iter().find(|f| f.name == n).unwrap();
            (f.size_bytes, f.size_at_ms, f.size_standby)
        };
        // the quick list measures nothing
        let l = list(None);
        assert_eq!(size_of(&l, "photos"), (None, None, false));
        assert!(walks().is_empty());
        // disks asleep: nothing is read, sizes unknown and marked
        let l = list(Some(&at(1000, &asleep)));
        assert_eq!(size_of(&l, "photos"), (None, None, true));
        assert!(walks().is_empty());
        // awake: measured once (quota group of a subvolume when quotas are on), then cached
        h.state().qgroups.insert("/vol1/photos".into(), 7000);
        let l = list(Some(&at(1000, &awake)));
        assert_eq!(size_of(&l, "photos"), (Some(7000), Some(1000), false));
        assert_eq!(size_of(&l, "old").0, Some(0));
        let w = walks();
        assert!(w.contains(&"qgroup /vol1/photos".to_string()), "{w:?}");
        assert!(!w.contains(&"walk /vol1/photos".to_string()), "{w:?}");
        assert!(w.contains(&"walk /vol1/old".to_string()), "{w:?}");
        let l = list(Some(&at(1000 + SIZE_TTL_MS - 1, &awake)));
        assert_eq!(size_of(&l, "photos").0, Some(7000));
        assert!(walks().is_empty());
        assert_eq!(size_of(&list(None), "photos"), (Some(7000), Some(1000), false));
        // expired while the disks sleep: the last size with its age, nothing read
        let late = 1000 + SIZE_TTL_MS;
        let l = list(Some(&at(late, &asleep)));
        assert_eq!(size_of(&l, "photos"), (Some(7000), Some(1000), true));
        assert!(walks().is_empty());
        // expired and awake: measured again
        h.state().qgroups.insert("/vol1/photos".into(), 8000);
        let l = list(Some(&at(late, &awake)));
        assert_eq!(size_of(&l, "photos"), (Some(8000), Some(late), false));
        assert!(!walks().is_empty());
        // cleared (apply of shares, folder deletion): measured on the next sized list
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(size_of(&list(None), "photos").0, None);
        list(Some(&at(late, &awake)));
        assert!(walks().contains(&"qgroup /vol1/photos".to_string()));
    }

    #[test]
    fn the_size_cache_is_capped() {
        let c = SizeCache::default();
        for i in 0..SIZE_CACHE_MAX as u64 + 5 {
            c.put(&format!("/v/{i}"), CachedSize { bytes: i, partial: false, at_ms: i, exact: false });
        }
        assert_eq!(c.len(), SIZE_CACHE_MAX);
        assert!(c.get("/v/0").is_none(), "the oldest went first");
        assert!(c.get(&format!("/v/{}", SIZE_CACHE_MAX + 4)).is_some());
    }

    fn folder(name: &str, path: &str, subvolume: bool) -> ShareFolder {
        ShareFolder {
            name: name.into(),
            path: path.into(),
            dir: path.into(),
            subvolume,
        }
    }

    #[test]
    fn share_sizes_answer_at_once_and_measure_in_the_background() {
        let h = host();
        h.state().sizes.insert("/vol1/old".into(), 700);
        let m = ShareSizeMeter::default();
        let list = [folder("photos", "/vol1/photos", true), folder("old", "/vol1/old", false)];
        let awake = |_: &str| false;
        // first answer: nothing known, both queued, nothing read yet
        let (r, start) = m.report(&vols(), &list, 1_000, &awake);
        assert!(start && r.measuring);
        assert!(r.shares.iter().all(|s| s.bytes.is_none() && s.measuring));
        assert_eq!(r.shares[0].volume.as_deref(), Some("vol1"));
        assert!(h.state().size_log.is_empty());
        // a second list meanwhile neither queues them again nor starts a second thread
        let (r, start) = m.report(&vols(), &list, 1_500, &awake);
        assert!(!start && r.measuring);
        m.run(&h, &awake, &|| 2_000);
        assert!(m.idle());
        assert_eq!(
            h.state().size_log,
            ["qgroup /vol1/photos", "walk /vol1/photos", "walk /vol1/old"]
        );
        let (r, start) = m.report(&vols(), &list, 3_000, &awake);
        assert!(!start && !r.measuring);
        assert_eq!(r.shares[0].bytes, Some(5000));
        assert_eq!(r.shares[1].bytes, Some(700));
        assert_eq!(r.shares[1].at_ms, Some(2_000));
        // an hour later the walked sizes are due again
        let (_, start) = m.report(&vols(), &list, 2_000 + SIZE_TTL_MS, &awake);
        assert!(start);
    }

    #[test]
    fn quota_group_sizes_are_exact_and_refreshed_sooner() {
        let h = host();
        h.state().qgroups.insert("/vol1/photos".into(), 4096);
        let m = ShareSizeMeter::default();
        let list = [folder("photos", "/vol1/photos", true)];
        let awake = |_: &str| false;
        m.report(&vols(), &list, 0, &awake);
        m.run(&h, &awake, &|| 0);
        assert_eq!(h.state().size_log, ["qgroup /vol1/photos"]);
        let (r, start) = m.report(&vols(), &list, 1, &awake);
        assert!(!start);
        assert!(r.shares[0].exact && !r.shares[0].partial);
        assert_eq!(r.shares[0].bytes, Some(4096));
        let (_, start) = m.report(&vols(), &list, EXACT_TTL_MS, &awake);
        assert!(start);
    }

    #[test]
    fn share_sizes_never_read_a_sleeping_disk() {
        let h = host();
        let m = ShareSizeMeter::default();
        let list = [folder("photos", "/vol1/photos", true)];
        // asleep when asked: standby, nothing queued
        let (r, start) = m.report(&vols(), &list, 0, &|_: &str| true);
        assert!(!start && !r.measuring);
        assert!(r.shares[0].standby && r.shares[0].bytes.is_none());
        // fell asleep after being queued: skipped by the thread
        let (_, start) = m.report(&vols(), &list, 0, &|_: &str| false);
        assert!(start);
        m.run(&h, &|_: &str| true, &|| 0);
        assert!(h.state().size_log.is_empty());
        assert!(m.idle());
    }

    #[test]
    fn share_sizes_thread_runs_the_queue() {
        let h: std::sync::Arc<dyn ShareHost> = std::sync::Arc::new(host());
        let m = std::sync::Arc::new(ShareSizeMeter::default());
        let list = [folder("photos", "/vol1/photos", true)];
        let (_, start) = m.report(&vols(), &list, 0, &|_: &str| false);
        assert!(start);
        m.start(h, std::sync::Arc::new(|_: &str| false));
        let t = Instant::now();
        while !m.idle() && t.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let (r, _) = m.report(&vols(), &list, 1, &|_: &str| false);
        assert_eq!(r.shares[0].bytes, Some(5000));
    }

    #[test]
    fn a_long_walk_resumes_behind_the_others_and_counts_hard_links_once() {
        use std::os::unix::fs::MetadataExt;
        // a real tree, read in slices of one folder each
        let t = tempfile::tempdir().unwrap();
        let big = t.path().join("big");
        std::fs::create_dir_all(big.join("a/b/c")).unwrap();
        for (i, d) in ["", "a", "a/b", "a/b/c"].iter().enumerate() {
            std::fs::write(big.join(d).join(format!("f{i}")), vec![7u8; 64 * 1024]).unwrap();
        }
        // the same file under two names: counted once
        std::fs::hard_link(big.join("f0"), big.join("a/f0-again")).unwrap();
        let small = t.path().join("small");
        std::fs::create_dir(&small).unwrap();
        std::fs::write(small.join("x"), b"x").unwrap();
        let (big_s, small_s) = (big.display().to_string(), small.display().to_string());
        let host = crate::host::HostShares;
        let m = ShareSizeMeter::default();
        // the first slice of the big folder: not done, "at least" what was counted
        assert!(m.measure_slice(&host, &big_s, false, 0, Duration::ZERO));
        let first = m.cache.get(&big_s).unwrap();
        assert!(first.partial);
        // the whole queue: the big one goes back behind the small one after each slice
        m.queue.lock().unwrap().waiting.extend([(big_s.clone(), false), (small_s.clone(), false)]);
        let order = std::sync::Mutex::new(Vec::new());
        m.run_sliced(&host, &|p: &str| { order.lock().unwrap().push(p.to_string()); false }, &|| 1, Duration::ZERO);
        let order = order.into_inner().unwrap();
        assert_eq!(order[..3], [big_s.clone(), small_s.clone(), big_s.clone()], "{order:?}");
        let done = m.cache.get(&big_s).unwrap();
        assert!(!done.partial && done.bytes > first.bytes);
        let (full, _) = crate::host::tree_size(&big, Duration::from_secs(10)).unwrap();
        // tree_size counts the linked file twice, the walk once
        let linked = std::fs::symlink_metadata(big.join("f0")).unwrap().blocks() * 512;
        assert_eq!(done.bytes, full - linked);
        assert!(m.walks.lock().unwrap().is_empty());
        assert!(m.queue.lock().unwrap().waiting.is_empty());
    }

    #[test]
    fn the_plan_lists_usages_and_blocks_what_must_block() {
        let h = host();
        let p = plan(
            &h,
            &vols(),
            "/vol1/photos",
            &src(),
            Usages {
                share: Some("photos".into()),
                own_nfs: true,
                jobs_removed: vec!["photos".into()],
                tasks: vec!["Nightly rsync".into()],
                ..Default::default()
            },
        );
        assert!(p.allowed, "{p:?}");
        assert_eq!((p.size_bytes, p.snapshots, p.recycle_bin), (5000, 2, true));
        assert_eq!(p.usages.len(), 3);
        let p = plan(
            &h,
            &vols(),
            "/vol1/data",
            &[],
            Usages {
                shares: vec!["inner".into()],
                jobs_kept: vec!["data".into()],
                ..Default::default()
            },
        );
        assert!(!p.allowed);
        assert_eq!(p.blockers.len(), 2);
        let p = plan(&h, &vols(), "/srv/elsewhere", &[], Usages::default());
        assert_eq!(p.blockers[0].kind, FolderUsageKind::NotOnVolume);
    }

    #[test]
    fn a_symlinked_folder_is_never_deleted() {
        // /vol1/link -> another subvolume: `btrfs subvolume delete` would delete the target
        let h = host();
        h.state().paths.insert(
            "/vol1/link".into(),
            crate::host::PathInfo {
                exists: true,
                is_dir: true,
                fs_type: "btrfs".into(),
                subvolume: true,
                symlink: true,
                ..Default::default()
            },
        );
        let p = plan(&h, &vols(), "/vol1/link", &[], Usages::default());
        assert!(!p.allowed, "{p:?}");
        let sys = MemorySystem::new("nas");
        let err = remove_folder(&sys, &h, &crate::crypt::MemoryCrypt::default(), &vols(), "/vol1/link", &[], &|_| {}).unwrap_err();
        assert!(err.contains("symbolic link"), "{err}");
        assert!(!sys.state().calls.iter().any(|c| c.starts_with("run ")));
        // the real probe reports the link
        let d = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(d.path(), d.path().join("l")).unwrap();
        let l = d.path().join("l");
        let i = crate::host::HostShares.probe(l.to_str().unwrap());
        assert!(i.symlink && i.is_dir, "{i:?}");
        assert!(!crate::host::HostShares.probe(d.path().to_str().unwrap()).symlink);
    }

    #[test]
    fn removal_runs_the_commands_in_order() {
        let h = host();
        let sys = MemorySystem::new("nas");
        let log = std::sync::Mutex::new(Vec::<String>::new());
        let lg = |l: &str| log.lock().unwrap().push(l.into());
        remove_folder(&sys, &h, &crate::crypt::MemoryCrypt::default(), &vols(), "/vol1/photos", &src(), &lg).unwrap();
        let calls = sys.state().calls.clone();
        let runs: Vec<&String> = calls
            .iter()
            .filter(|c| c.starts_with("run btrfs") || c.starts_with("run rm"))
            .collect();
        assert_eq!(runs.len(), 3, "{calls:?}");
        assert!(runs[0].contains("btrfs subvolume delete /vol1/.snapshots/photos/photos.20261104T1200"));
        assert!(runs[1].contains("photos.20261105T1200"));
        assert!(runs[2].contains("btrfs subvolume delete --recursive /vol1/photos"));
        // the snapshot folder unit was active (MemorySystem: every unit is) and stopped first
        let stop = calls.iter().position(|c| c.contains("systemctl stop")).unwrap();
        let first_run = calls.iter().position(|c| c.starts_with("run btrfs")).unwrap();
        assert!(stop < first_run, "{calls:?}");
        assert!(h.state().dir_log.contains(&"rmdir /vol1/.snapshots/photos".to_string()));

        // a plain directory goes with rm, never across filesystems
        let sys = MemorySystem::new("nas");
        remove_folder(&sys, &h, &crate::crypt::MemoryCrypt::default(), &vols(), "/vol1/old", &[], &lg).unwrap();
        let calls = sys.state().calls.clone();
        assert!(
            calls.iter().any(|c| c.contains("rm -rf --one-file-system --preserve-root=all -- /vol1/old")),
            "{calls:?}"
        );

        // never outside a volume
        let sys = MemorySystem::new("nas");
        assert!(remove_folder(&sys, &h, &crate::crypt::MemoryCrypt::default(), &vols(), "/srv/x", &[], &lg).is_err());
        assert!(remove_folder(&sys, &h, &crate::crypt::MemoryCrypt::default(), &vols(), "/vol1/.snapshots", &[], &lg).is_err());
        assert!(sys.state().calls.iter().all(|c| !c.starts_with("run ")));

        // a failing command stops the removal
        let sys = MemorySystem::new("nas");
        sys.state().fail_programs.push("btrfs".into());
        assert!(remove_folder(&sys, &h, &crate::crypt::MemoryCrypt::default(), &vols(), "/vol1/photos", &src(), &lg).is_err());
        assert_eq!(
            sys.state().calls.iter().filter(|c| c.starts_with("run btrfs")).count(),
            1
        );
    }

    #[test]
    fn an_encrypted_folder_goes_through_its_backend() {
        use crate::crypt::CryptHost;
        let sys = MemorySystem::new("nas");
        let h = std::sync::Arc::new(host());
        let c = crate::crypt::MemoryCrypt::new(true).with_shares(h.clone());
        let pw = crate::accounts::Secret::new("correct horse");
        c.create("/vol1/secret", &pw, &|_| {}).unwrap();
        c.set_busy("/vol1/secret", true);
        let err = remove_folder(&sys, h.as_ref(), &c, &vols(), "/vol1/secret", &[], &|_| {}).unwrap_err();
        assert!(err.contains("in use"), "{err}");
        assert_eq!(c.state("/vol1/secret"), rynas_api_types::CryptState::Unlocked);
        c.set_busy("/vol1/secret", false);
        // the snapshots of its ciphertext go too (a job on the ciphertext directory)
        remove_folder(&sys, h.as_ref(), &c, &vols(), "/vol1/secret", &src(), &|_| {}).unwrap();
        let runs: Vec<String> = sys.state().calls.iter().filter(|c| c.starts_with("run btrfs")).cloned().collect();
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(c.state("/vol1/secret"), rynas_api_types::CryptState::Missing);
        assert!(!h.probe("/vol1/secret").exists);
        // no plain delete ran on the mount point
        assert!(!sys.state().calls.iter().any(|c| c.contains("/vol1/secret")), "{:?}", sys.state().calls);
    }
}
