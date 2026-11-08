//! What each job's last run left: `<state>/backup/<job>.json` (root, 0600), written whole
//! (temporary file + rename).

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rynas_api_types::backup::BackupStatus;

use crate::valid_job_name;

pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("backup")
}

fn path(state_dir: &Path, job: &str) -> Option<PathBuf> {
    valid_job_name(job).then(|| dir(state_dir).join(format!("{job}.json")))
}

/// The status of `job` (default when there is none yet).
pub fn read(state_dir: &Path, job: &str) -> BackupStatus {
    path(state_dir, job)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn write(state_dir: &Path, job: &str, s: &BackupStatus) -> std::io::Result<()> {
    let p = path(state_dir, job).ok_or_else(|| std::io::Error::other("invalid job name"))?;
    std::fs::create_dir_all(dir(state_dir))?;
    let tmp = p.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?)?;
    f.sync_all()?;
    std::fs::rename(tmp, p)
}

/// Forgets a removed job.
pub fn remove(state_dir: &Path, job: &str) {
    if let Some(p) = path(state_dir, job) {
        let _ = std::fs::remove_file(p);
    }
}

/// Whether a maintenance step every `days` is due after `last_ms`.
pub fn due(last_ms: Option<u64>, days: u32, now_ms: u64) -> bool {
    if days == 0 {
        return false;
    }
    // a little early is fine: a daily job must not skip a day because it ran 1 min sooner
    let every = u64::from(days) * 86_400_000 - 3_600_000;
    last_ms.is_none_or(|l| now_ms.saturating_sub(l) >= every)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_due() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(read(t.path(), "docs"), BackupStatus::default());
        let s = BackupStatus {
            snapshots: 3,
            last_ok_ms: Some(5),
            ..Default::default()
        };
        write(t.path(), "docs", &s).unwrap();
        assert_eq!(read(t.path(), "docs"), s);
        assert!(write(t.path(), "../x", &s).is_err());
        remove(t.path(), "docs");
        assert_eq!(read(t.path(), "docs").snapshots, 0);
        let day = 86_400_000;
        assert!(due(None, 7, day));
        assert!(!due(Some(day), 7, 3 * day));
        assert!(due(Some(day), 1, 2 * day - 60_000));
        assert!(!due(None, 0, day));
    }
}
