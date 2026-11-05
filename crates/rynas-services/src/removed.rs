//! Shares removed while their folder stayed: the share as it was (name, permissions, SMB
//! and NFS settings) and when, so that "Unshared folders" can say when the folder lost its
//! share and share it again with the same settings.
//!
//! One JSON file in rynasd's state directory (`shares-removed.json`, 0600), written by the
//! apply of `shares`; in memory for tests and without a state directory. At most
//! [`MAX_ENTRIES`] entries, one per path, the oldest dropped first.

use std::path::PathBuf;
use std::sync::Mutex;

use rynas_api_types::Share;
use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "shares-removed.json";
/// Cap of the record (M1): the oldest entries go first.
pub const MAX_ENTRIES: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedShare {
    pub share: Share,
    pub removed_at_ms: u64,
}

#[derive(Default)]
pub struct RemovedShares {
    file: Option<PathBuf>,
    mem: Mutex<Vec<RemovedShare>>,
}

impl RemovedShares {
    /// Kept in memory only.
    pub fn memory() -> Self {
        Self::default()
    }

    /// Kept in `<state_dir>/shares-removed.json`.
    pub fn in_dir(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            file: Some(state_dir.into().join(FILE_NAME)),
            mem: Mutex::new(Vec::new()),
        }
    }

    pub fn list(&self) -> Vec<RemovedShare> {
        match &self.file {
            Some(f) => std::fs::read(f)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default(),
            None => self.mem.lock().unwrap().clone(),
        }
    }

    fn save(&self, v: Vec<RemovedShare>) -> Result<(), String> {
        let Some(f) = &self.file else {
            *self.mem.lock().unwrap() = v;
            return Ok(());
        };
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = f.with_extension("json.tmp");
        let body = serde_json::to_vec(&v).map_err(|e| e.to_string())?;
        let mut w = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        w.write_all(&body)
            .and_then(|_| w.sync_all())
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, f).map_err(|e| format!("{}: {e}", f.display()))
    }

    /// Records a share whose folder stays (replaces an older entry of the same path).
    pub fn record(&self, share: &Share, now_ms: u64) -> Result<(), String> {
        let mut v = self.list();
        v.retain(|r| r.share.path != share.path);
        v.push(RemovedShare {
            share: share.clone(),
            removed_at_ms: now_ms,
        });
        if v.len() > MAX_ENTRIES {
            let drop = v.len() - MAX_ENTRIES;
            v.drain(..drop);
        }
        self.save(v)
    }

    /// Forgets the entries of these paths (shared again, or deleted). Writes only when
    /// something changes.
    pub fn forget(&self, paths: &[&str]) -> Result<(), String> {
        let mut v = self.list();
        let before = v.len();
        v.retain(|r| !paths.contains(&r.share.path.as_str()));
        if v.len() == before {
            return Ok(());
        }
        self.save(v)
    }

    pub fn find(&self, path: &str) -> Option<RemovedShare> {
        self.list().into_iter().rev().find(|r| r.share.path == path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(name: &str, path: &str) -> Share {
        Share {
            name: name.into(),
            path: path.into(),
            ..Default::default()
        }
    }

    #[test]
    fn records_forgets_and_caps() {
        let d = tempfile::tempdir().unwrap();
        let r = RemovedShares::in_dir(d.path());
        assert!(r.list().is_empty());
        r.record(&share("a", "/vol1/a"), 1).unwrap();
        r.record(&share("a2", "/vol1/a"), 2).unwrap();
        r.record(&share("b", "/vol1/b"), 3).unwrap();
        let again = RemovedShares::in_dir(d.path());
        assert_eq!(again.list().len(), 2);
        assert_eq!(again.find("/vol1/a").unwrap().share.name, "a2");
        again.forget(&["/vol1/a"]).unwrap();
        assert!(again.find("/vol1/a").is_none());
        let m = RemovedShares::memory();
        for i in 0..MAX_ENTRIES + 5 {
            m.record(&share("x", &format!("/vol1/{i}")), i as u64)
                .unwrap();
        }
        let l = m.list();
        assert_eq!(l.len(), MAX_ENTRIES);
        assert_eq!(l[0].removed_at_ms, 5, "the oldest go first");
    }
}
