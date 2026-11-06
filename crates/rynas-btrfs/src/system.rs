//! System restore points: a read-only snapshot of the root subvolume with a btrbk name,
//! taken by `rynasctl snapshot system --reason apt` from the apt hook before dpkg runs.
//!
//! One ioctl (`BTRFS_IOC_SNAP_CREATE_V2` with `BTRFS_SUBVOL_RDONLY`), no daemon: the hook
//! works while rynasd itself is being upgraded. Skipped when nothing changed since the last
//! restore point (btrbk's `onchange` rule: the source generation is not newer than the
//! transaction that created the latest snapshot), or when the latest one is less than
//! [`DEDUP_WINDOW_S`] old (apt calls dpkg several times in one run). The oldest restore points
//! beyond `keep` are deleted afterwards (never the newest).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ioctl;
use crate::name::long_timestamp;
use crate::scan::{self, JobLock, SnapError};

/// A restore point younger than this is reused.
pub const DEDUP_WINDOW_S: i64 = 120;
/// Lines kept in the reasons log.
const REASONS_KEPT: usize = 500;

/// Outcome of [`take`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taken {
    Created {
        name: String,
        pruned: Vec<String>,
    },
    /// Nothing changed since `latest`, or it is recent.
    Skipped {
        latest: String,
        why: &'static str,
    },
}

/// One line of the reasons log (`<state>/snapshots/system.jsonl`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonLine {
    pub name: String,
    pub reason: String,
    pub ts_ms: u64,
}

pub struct SystemSnapshot<'a> {
    pub subvolume: &'a Path,
    pub snapshot_dir: &'a Path,
    pub prefix: &'a str,
    pub keep: u32,
    /// Reasons log; `None`: not recorded.
    pub reasons: Option<&'a Path>,
}

/// Takes a restore point now (`now_s`) for `reason`.
pub fn take(s: &SystemSnapshot, reason: &str, now_s: i64) -> Result<Taken, SnapError> {
    take_with(s, reason, now_s, false)
}

/// Like [`take`], but never skipped (the restore point a root rollback keeps of the current
/// system, reason "before rollback").
pub fn take_always(s: &SystemSnapshot, reason: &str, now_s: i64) -> Result<String, SnapError> {
    match take_with(s, reason, now_s, true)? {
        Taken::Created { name, .. } => Ok(name),
        Taken::Skipped { latest, .. } => Ok(latest),
    }
}

fn take_with(
    s: &SystemSnapshot,
    reason: &str,
    now_s: i64,
    force: bool,
) -> Result<Taken, SnapError> {
    let src = ioctl::open_dir(s.subvolume)
        .map_err(|e| SnapError::Failed(format!("{}: {e}", s.subvolume.display())))?;
    if !ioctl::is_subvolume(&src) {
        return Err(SnapError::Refused(format!(
            "{} is not a btrfs subvolume",
            s.subvolume.display()
        )));
    }
    std::fs::create_dir_all(s.snapshot_dir)?;
    let _lock = JobLock::acquire(&scan::lock_path(s.prefix))?;
    let existing = scan::scan(s.snapshot_dir, s.prefix)?;
    let latest = existing
        .iter()
        .find(|x| x.info.as_ref().is_some_and(|i| i.readonly));
    if let Some(l) = latest.filter(|_| !force) {
        let info = ioctl::subvol_info(&src)?;
        let li = l.info.as_ref().expect("read-only implies info");
        if info.generation <= li.otransid {
            return Ok(Taken::Skipped {
                latest: l.name.clone(),
                why: "nothing changed since the latest restore point",
            });
        }
        if now_s - li.otime < DEDUP_WINDOW_S && li.otime <= now_s {
            return Ok(Taken::Skipped {
                latest: l.name.clone(),
                why: "the latest restore point is less than two minutes old",
            });
        }
    }
    let base = format!("{}.{}", s.prefix, long_timestamp(now_s));
    let dir = ioctl::open_dir(s.snapshot_dir)?;
    if ioctl::fs_uuid(&dir).ok() != ioctl::fs_uuid(&src).ok() {
        return Err(SnapError::Refused(format!(
            "{} is not on the filesystem of {}",
            s.snapshot_dir.display(),
            s.subvolume.display()
        )));
    }
    let mut name = base.clone();
    let mut n = 1;
    while s.snapshot_dir.join(&name).exists() {
        name = format!("{base}_{n}");
        n += 1;
    }
    ioctl::create_snapshot(&src, &dir, &name, true)
        .map_err(|e| SnapError::Failed(format!("snapshot of {}: {e}", s.subvolume.display())))?;
    if let Some(r) = s.reasons {
        let _ = record_reason(r, &name, reason, now_s as u64 * 1000);
    }
    // retention: the oldest read-only ones beyond `keep` (not before a rollback, which may be
    // going back to one of them)
    let mut pruned = Vec::new();
    if force {
        return Ok(Taken::Created { name, pruned });
    }
    let all = scan::scan(s.snapshot_dir, s.prefix)?;
    let ro: Vec<_> = all
        .iter()
        .filter(|x| x.info.as_ref().is_some_and(|i| i.readonly))
        .collect();
    for old in ro.iter().skip(s.keep.max(1) as usize) {
        if old.name != name && ioctl::destroy_subvolume(&dir, &old.name).is_ok() {
            pruned.push(old.name.clone());
        }
    }
    Ok(Taken::Created { name, pruned })
}

fn record_reason(path: &Path, name: &str, reason: &str, ts_ms: u64) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut lines: Vec<String> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    lines.push(
        serde_json::to_string(&ReasonLine {
            name: name.into(),
            reason: reason.into(),
            ts_ms,
        })
        .map_err(std::io::Error::other)?,
    );
    let start = lines.len().saturating_sub(REASONS_KEPT);
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp)?;
    for l in &lines[start..] {
        writeln!(f, "{l}")?;
    }
    f.sync_all()?;
    std::fs::rename(tmp, path)
}

/// Reasons by snapshot name.
pub fn reasons(path: &Path) -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<ReasonLine>(l).ok())
        .map(|r| (r.name, r.reason))
        .collect()
}

/// The reasons log below a state directory.
pub fn reasons_path(state_dir: &Path) -> PathBuf {
    state_dir.join("snapshots").join("system.jsonl")
}

/// Reason of the restore points the apt hook takes.
pub const APT_REASON: &str = "apt";
/// Reason of a restore point taken by hand (the default of `rynasctl snapshot system`).
pub const MANUAL_REASON: &str = "manual";

/// The code of a recorded reason; free text is `Other`.
pub fn reason_code(r: &str) -> rynas_api_types::snapshots::RestorePointReason {
    use rynas_api_types::snapshots::RestorePointReason as R;
    match r {
        APT_REASON => R::Apt,
        MANUAL_REASON => R::Manual,
        crate::rootfs::BEFORE_REASON => R::BeforeRollback,
        _ => R::Other,
    }
}

/// A reason given on the command line: short, printable.
pub fn valid_reason(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 64
        && r.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b' '))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_reasons_have_a_code() {
        use rynas_api_types::snapshots::RestorePointReason as R;
        assert_eq!(reason_code("apt"), R::Apt);
        assert_eq!(reason_code("manual"), R::Manual);
        assert_eq!(reason_code("before rollback"), R::BeforeRollback);
        assert_eq!(reason_code("before upgrade"), R::Other);
        let j = serde_json::to_string(&R::BeforeRollback).unwrap();
        assert_eq!(j, r#""before_rollback""#);
        assert_eq!(serde_json::from_str::<R>(r#""cron""#).unwrap(), R::Other);
    }

    #[test]
    fn reasons_log_is_capped_and_read_back() {
        let d = tempfile::tempdir().unwrap();
        let p = reasons_path(d.path());
        for i in 0..(REASONS_KEPT + 3) {
            record_reason(&p, &format!("rootfs.{i}"), "apt", i as u64).unwrap();
        }
        let r = reasons(&p);
        assert_eq!(r.len(), REASONS_KEPT);
        assert!(!r.contains_key("rootfs.0"));
        assert_eq!(r[&format!("rootfs.{}", REASONS_KEPT + 2)], "apt");
        assert!(valid_reason("apt") && valid_reason("before upgrade"));
        assert!(!valid_reason("") && !valid_reason("a;b") && !valid_reason("x\ny"));
    }

    #[test]
    fn refuses_a_plain_directory() {
        let d = tempfile::tempdir().unwrap();
        let s = SystemSnapshot {
            subvolume: d.path(),
            snapshot_dir: &d.path().join(".snapshots"),
            prefix: "rootfs",
            keep: 3,
            reasons: None,
        };
        assert!(matches!(take(&s, "apt", 0), Err(SnapError::Refused(_))));
    }
}
