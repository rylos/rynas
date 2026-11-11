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
    /// Roots the hashfile holds hashes of (runs and estimates): a root that is no longer in
    /// the job is forgotten with `-R` before the next run, and stays here until that worked.
    #[serde(default)]
    pub roots: Vec<String>,
    /// Block size the hashfile was made with (a change resets it).
    #[serde(default)]
    pub block_size: Option<u32>,
    /// Failed runs in a row with a damaged hashfile (the second one resets it).
    #[serde(default)]
    pub failures: u32,
    /// The hashfile of the last run (removed with the job).
    #[serde(default)]
    pub hashfile: Option<String>,
}

/// A record that repeats the newest one: skips for the same reason, the same failure
/// without an exit code (engine missing): kept as one entry, its end moved.
fn repeats(new: &DedupRecord, last: &DedupRecord) -> bool {
    use rynas_api_types::dedup::DedupOutcome::{Failed, Skipped};
    new.outcome == last.outcome
        && new.message == last.message
        && new.kind == last.kind
        && (new.outcome == Skipped || (new.outcome == Failed && new.exit_code.is_none() && last.exit_code.is_none()))
}

impl JobState {
    /// Adds a record, newest first; a repeat of the newest one only moves its end. `true`
    /// when it was a repeat (no new notification for it).
    pub fn push(&mut self, r: DedupRecord) -> bool {
        if let Some(last) = self.history.first_mut()
            && repeats(&r, last)
        {
            last.end_ms = r.end_ms;
            return true;
        }
        self.history.insert(0, r);
        self.history.truncate(HISTORY);
        false
    }

    /// The last run that did something (skips are left out).
    pub fn last(&self) -> Option<&DedupRecord> {
        self.history.iter().find(|r| r.outcome != rynas_api_types::dedup::DedupOutcome::Skipped)
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

/// Writes a job's state (new file synced, then rename, then the folder synced).
pub fn write(dir: &Path, key: &str, s: &JobState) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let p = path(dir, key);
    let tmp = p.with_extension("json.new");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &p)?;
    std::fs::File::open(dir)?.sync_all()
}

/// Removes a job's state.
pub fn remove(dir: &Path, key: &str) -> std::io::Result<()> {
    match std::fs::remove_file(path(dir, key)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
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
        remove(d.path(), "vol1").unwrap();
        remove(d.path(), "vol1").unwrap();
        assert_eq!(read(d.path(), "vol1"), JobState::default());
    }

    #[test]
    fn repeated_skips_and_engine_failures_are_one_entry() {
        use rynas_api_types::dedup::DedupOutcome::{Done, Failed, Skipped};
        let mut s = JobState::default();
        let rec = |o, m: &str, end, code| DedupRecord { outcome: o, message: m.into(), end_ms: end, exit_code: code, ..Default::default() };
        assert!(!s.push(rec(Done, "", 1, Some(0))));
        for n in 2..10 {
            s.push(rec(Skipped, "the disks are in standby", n, None));
        }
        assert_eq!(s.history.len(), 2);
        assert_eq!(s.history[0].end_ms, 9);
        // the last run is the one that did something
        assert_eq!(s.last().unwrap().outcome, Done);
        assert!(!s.push(rec(Failed, "engine missing", 10, None)));
        assert!(s.push(rec(Failed, "engine missing", 11, None)));
        assert!(!s.push(rec(Failed, "exit 1", 12, Some(1))));
        assert!(!s.push(rec(Failed, "exit 1", 13, Some(1))));
        assert_eq!(s.history.len(), 5);
    }
}
