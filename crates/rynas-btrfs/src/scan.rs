//! The snapshots of one prefix in a snapshot directory, and the chain-safe delete.
//!
//! From btrbk-nas `snapshot.rs` (MIT, same author), adapted to snapshots taken on the NAS
//! itself: there is no received chain to protect yet (replication is the `backup` plugin of
//! phase 2), so the rules are those btrbk relies on locally: the newest snapshot of a job is
//! the parent of the next incremental send and the base of `snapshot_create onchange`, and
//! only read-only subvolumes with the job's names are ever deleted. Deletions re-check on a
//! fresh scan under the job's lock, the same lock file btrbk uses, so a running `btrbk run`
//! and a delete never overlap.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use rynas_api_types::snapshots::SnapshotDeletion;

use crate::ioctl::{self, SubvolInfo};
use crate::name::{compare_names, parse_timestamp, split_snapshot_name};

/// One snapshot found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub name: String,
    pub path: PathBuf,
    /// The time in the name, milliseconds since the epoch.
    pub created_ms: i64,
    /// `None` when the entry is not a subvolume (or its facts could not be read).
    pub info: Option<SubvolInfo>,
    pub deletion: SnapshotDeletion,
}

/// Names in `dir` that are snapshots of `prefix` (directories only), newest first.
pub fn names(dir: &Path, prefix: &str) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(n) = e.file_name().into_string() else {
            continue;
        };
        if split_snapshot_name(&n).is_some_and(|(p, _)| p == prefix) {
            out.push(n);
        }
    }
    out.sort_by(|a, b| compare_names(b, a));
    Ok(out)
}

/// The snapshots of `prefix` in `dir`, newest first, with their deletion verdict. A missing
/// directory has none.
pub fn scan(dir: &Path, prefix: &str) -> io::Result<Vec<Snapshot>> {
    let names = match names(dir, prefix) {
        Ok(n) => n,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let parent = ioctl::open_dir(dir)?;
    let mut snaps: Vec<Snapshot> = names
        .into_iter()
        .map(|name| {
            let created_ms = split_snapshot_name(&name)
                .and_then(|(_, ts)| parse_timestamp(ts))
                .map_or(0, |s| s.epoch_ms());
            let info = ioctl::open_dir_at(&parent, &name)
                .ok()
                .filter(ioctl::is_subvolume)
                .and_then(|f| ioctl::subvol_info(&f).ok());
            Snapshot {
                path: dir.join(&name),
                name,
                created_ms,
                info,
                deletion: SnapshotDeletion::Allowed,
            }
        })
        .collect();
    mark_deletion(&mut snaps);
    Ok(snaps)
}

/// Whether each snapshot (newest first) may be deleted.
pub fn mark_deletion(snaps: &mut [Snapshot]) {
    let newest_ro = snaps
        .iter()
        .position(|s| s.info.as_ref().is_some_and(|i| i.readonly));
    for (i, s) in snaps.iter_mut().enumerate() {
        s.deletion = match &s.info {
            None => SnapshotDeletion::NotSubvolume,
            Some(info) if !info.readonly => SnapshotDeletion::Writable,
            Some(_) if Some(i) == newest_ro => SnapshotDeletion::Latest,
            Some(_) => SnapshotDeletion::Allowed,
        };
    }
}

/// Why a snapshot was not deleted (or restored, rolled back).
#[derive(Debug, PartialEq, Eq)]
pub enum SnapError {
    /// Not a snapshot name of this job, or a bad path.
    Invalid(String),
    NotFound(String),
    /// The checks say no (409).
    Refused(String),
    /// The lock, a system call or btrfs failed.
    Failed(String),
}

impl std::fmt::Display for SnapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (SnapError::Invalid(m)
        | SnapError::NotFound(m)
        | SnapError::Refused(m)
        | SnapError::Failed(m)) = self;
        f.write_str(m)
    }
}

impl std::error::Error for SnapError {}

impl From<io::Error> for SnapError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => SnapError::NotFound(e.to_string()),
            _ => SnapError::Failed(e.to_string()),
        }
    }
}

/// An exclusive `flock` held until dropped. btrbk takes the same lock (`lockfile`) and
/// gives up when it is held.
pub struct JobLock(#[allow(dead_code)] File);

impl JobLock {
    pub fn acquire(path: &Path) -> Result<Self, SnapError> {
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|e| SnapError::Failed(format!("lock {}: {e}", path.display())))?;
        // SAFETY: a valid descriptor; the lock goes with the file
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(SnapError::Refused(
                "the snapshot job is running: try again when it has finished".into(),
            ));
        }
        Ok(Self(f))
    }
}

/// Lock file of a job, shared with btrbk (`lockfile` in its configuration).
pub fn lock_path(job: &str) -> PathBuf {
    PathBuf::from(format!("/run/lock/rynas-btrbk-{job}.lock"))
}

/// Reason a verdict refuses a delete, in English.
pub fn refusal(d: SnapshotDeletion) -> &'static str {
    match d {
        SnapshotDeletion::Allowed => "it may be deleted",
        SnapshotDeletion::Latest => {
            "it is the newest snapshot of the job: the base of the next incremental send; it can go once a newer one exists"
        }
        SnapshotDeletion::Writable => {
            "it is not read-only: not a snapshot taken by btrbk (or an interrupted receive)"
        }
        SnapshotDeletion::NotSubvolume => "it is not a btrfs subvolume",
    }
}

/// Deletes snapshot `name` of `prefix` in `dir` after checking again, on a fresh scan under
/// the job lock, that it may go.
pub fn delete(dir: &Path, prefix: &str, name: &str, lock: &Path) -> Result<(), SnapError> {
    if name.contains('/') || split_snapshot_name(name).is_none_or(|(p, _)| p != prefix) {
        return Err(SnapError::Invalid(format!(
            "{name:?} is not a snapshot of {prefix}"
        )));
    }
    let _lock = JobLock::acquire(lock)?;
    let snaps = scan(dir, prefix)?;
    let s = snaps
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| SnapError::NotFound(format!("snapshot {name} not found")))?;
    if s.deletion != SnapshotDeletion::Allowed {
        return Err(SnapError::Refused(format!(
            "{name} is kept: {}",
            refusal(s.deletion)
        )));
    }
    let parent = ioctl::open_dir(dir)?;
    ioctl::destroy_subvolume(&parent, name)
        .map_err(|e| SnapError::Failed(format!("deleting {name}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(name: &str, info: Option<(bool, u64)>) -> Snapshot {
        Snapshot {
            name: name.into(),
            path: PathBuf::from("/x").join(name),
            created_ms: 0,
            info: info.map(|(ro, id)| SubvolInfo {
                root_id: id,
                readonly: ro,
                ..Default::default()
            }),
            deletion: SnapshotDeletion::Allowed,
        }
    }

    #[test]
    fn the_newest_read_only_snapshot_stays() {
        let mut s = vec![
            snap("d.20261105T1300", Some((false, 4))),
            snap("d.20261105T1200", Some((true, 3))),
            snap("d.20261105T1100", None),
            snap("d.20261105T1000", Some((true, 1))),
        ];
        mark_deletion(&mut s);
        let v: Vec<_> = s.iter().map(|s| s.deletion).collect();
        assert_eq!(
            v,
            [
                SnapshotDeletion::Writable,
                SnapshotDeletion::Latest,
                SnapshotDeletion::NotSubvolume,
                SnapshotDeletion::Allowed
            ]
        );
    }

    #[test]
    fn names_of_one_prefix_newest_first() {
        let d = tempfile::tempdir().unwrap();
        for n in [
            "docs.20261105T1000",
            "docs.20261105T1200",
            "docs.20261105T1200_1",
            "photos.20261105T1300",
            "docs.notes",
        ] {
            std::fs::create_dir(d.path().join(n)).unwrap();
        }
        std::fs::write(d.path().join("docs.20261105T1400"), "a file").unwrap();
        assert_eq!(
            names(d.path(), "docs").unwrap(),
            [
                "docs.20261105T1200_1",
                "docs.20261105T1200",
                "docs.20261105T1000"
            ]
        );
        // plain directories are not subvolumes: listed, never deletable
        let s = scan(d.path(), "docs").unwrap();
        assert!(
            s.iter()
                .all(|s| s.deletion == SnapshotDeletion::NotSubvolume)
        );
        let lock = d.path().join("lock");
        assert!(matches!(
            delete(d.path(), "docs", "docs.20261105T1000", &lock),
            Err(SnapError::Refused(_))
        ));
        assert!(matches!(
            delete(d.path(), "docs", "photos.20261105T1300", &lock),
            Err(SnapError::Invalid(_))
        ));
        assert!(matches!(
            delete(d.path(), "docs", "docs.20261105T0900", &lock),
            Err(SnapError::NotFound(_))
        ));
        assert!(scan(&d.path().join("missing"), "docs").unwrap().is_empty());
    }

    #[test]
    fn a_held_lock_refuses() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("l");
        let _a = JobLock::acquire(&p).unwrap();
        assert!(matches!(JobLock::acquire(&p), Err(SnapError::Refused(_))));
    }
}
