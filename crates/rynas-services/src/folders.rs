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

/// A size of the list as measured: bytes, partial (the time budget ran out), when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedSize {
    pub bytes: u64,
    pub partial: bool,
    pub at_ms: u64,
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

/// The size of one folder: the quota group of a subvolume when quotas are enabled, else a
/// walk of its files within the time budget.
fn measure_one(host: &dyn ShareHost, path: &str, subvolume: bool, now_ms: u64) -> Option<CachedSize> {
    if subvolume && let Some(bytes) = host.qgroup_size(path) {
        return Some(CachedSize {
            bytes,
            partial: false,
            at_ms: now_ms,
        });
    }
    host.tree_size(path, LIST_SIZE_BUDGET)
        .map(|(bytes, partial)| CachedSize {
            bytes,
            partial,
            at_ms: now_ms,
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
            c.put(&format!("/v/{i}"), CachedSize { bytes: i, partial: false, at_ms: i });
        }
        assert_eq!(c.len(), SIZE_CACHE_MAX);
        assert!(c.get("/v/0").is_none(), "the oldest went first");
        assert!(c.get(&format!("/v/{}", SIZE_CACHE_MAX + 4)).is_some());
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
