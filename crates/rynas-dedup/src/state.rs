//! What rynasd keeps about a job (`/var/lib/rynas/dedup/<key>.json`): the history, the last
//! report and estimate, the roots of the last run. The UI reads this, never the hashfile
//! (it lives on the data disks, which may be asleep).

use std::path::{Path, PathBuf};

use rynas_api_types::dedup::{DedupEstimate, DedupRecord, DedupReport};
use serde::{Deserialize, Serialize};

/// Runs kept in the history.
pub const HISTORY: usize = 52;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct JobState {
    /// Newest first.
    #[serde(default)]
    pub history: Vec<DedupRecord>,
    #[serde(default)]
    pub report: Option<DedupReport>,
    #[serde(default)]
    pub estimate: Option<DedupEstimate>,
    /// Roots of the last run that deduplicated: a root that leaves the job is forgotten with
    /// `-R` before the next run.
    #[serde(default)]
    pub roots: Vec<String>,
    /// Block size the hashfile was made with (a change resets it).
    #[serde(default)]
    pub block_size: Option<u32>,
    /// Failed runs in a row (a second one with a hashfile error resets the hashfile).
    #[serde(default)]
    pub failures: u32,
}

impl JobState {
    pub fn push(&mut self, r: DedupRecord) {
        self.history.insert(0, r);
        self.history.truncate(HISTORY);
    }

    /// The last run that did something (skips are left out).
    pub fn last(&self) -> Option<&DedupRecord> {
        self.history.first()
    }
}

/// The file of a job below `dir`.
pub fn path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.json"))
}

/// The state of a job (empty when missing or unreadable).
pub fn read(dir: &Path, key: &str) -> JobState {
    std::fs::read(path(dir, key))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Writes a job's state (new file, then rename).
pub fn write(dir: &Path, key: &str, s: &JobState) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let p = path(dir, key);
    let tmp = p.with_extension("json.new");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?)?;
    std::fs::rename(&tmp, &p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_capped_and_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let mut s = read(d.path(), "vol1");
        assert_eq!(s, JobState::default());
        for i in 0..60 {
            s.push(DedupRecord { start_ms: i, ..Default::default() });
        }
        assert_eq!(s.history.len(), HISTORY);
        assert_eq!(s.last().unwrap().start_ms, 59);
        write(d.path(), "vol1", &s).unwrap();
        assert_eq!(read(d.path(), "vol1"), s);
    }
}
