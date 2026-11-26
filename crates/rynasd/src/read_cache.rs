//! Answers of repository reads (the snapshot list, a folder of a snapshot) kept per
//! repository, so that going back and forth in a restore browser does not start a worker (and
//! restic's index load) for every click.
//!
//! A repository's entries go when something may have changed it: every task operation on it
//! (backup, forget, prune, restore, check: [`ReadCache::invalidate`] at its start and at its
//! end) and an agent's report of a finished upload. A read started before such a change is not
//! kept ([`ReadCache::generation`] is taken before the read and checked when it is stored).
//! Writers outside this NAS (another NAS on the same repository) are covered by the age limit.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long an answer is served without asking the repository again.
const MAX_AGE: Duration = Duration::from_secs(15 * 60);
/// Answers kept per repository (the oldest goes first).
const PER_REPO: usize = 128;
/// Repositories with answers kept.
const REPOS: usize = 64;

#[derive(Default)]
struct Repo {
    generation: u64,
    entries: HashMap<String, (Instant, std::sync::Arc<serde_json::Value>)>,
}

/// See the module documentation. The answers are kept as JSON values (one type for every
/// kind of read).
#[derive(Default)]
pub struct ReadCache {
    repos: Mutex<HashMap<String, Repo>>,
}

/// The cache of this rynasd (restic jobs and PC repositories; their keys do not collide:
/// `job:<name>` and `pc:<group>`).
pub fn global() -> &'static ReadCache {
    static C: std::sync::OnceLock<ReadCache> = std::sync::OnceLock::new();
    C.get_or_init(ReadCache::new)
}

/// [`ReadCache`] around one read: the kept answer of `key`, else `read()` (kept when it
/// succeeds and the repository did not change meanwhile).
pub async fn cached<T, F>(repo: &str, key: &str, read: F) -> Result<T, crate::ops::Error>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
    F: std::future::Future<Output = Result<T, crate::ops::Error>>,
{
    let c = global();
    if let Some(v) = c.get(repo, key)
        && let Ok(t) = serde_json::from_value::<T>((*v).clone())
    {
        return Ok(t);
    }
    let generation = c.generation(repo);
    let t = read.await?;
    if let Ok(v) = serde_json::to_value(&t) {
        c.put(repo, generation, key, v);
    }
    Ok(t)
}

impl ReadCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The answer kept for `key` of `repo`, if it is fresh.
    pub fn get(&self, repo: &str, key: &str) -> Option<std::sync::Arc<serde_json::Value>> {
        let g = self.repos.lock().unwrap_or_else(|p| p.into_inner());
        let (at, v) = g.get(repo)?.entries.get(key)?;
        (at.elapsed() < MAX_AGE).then(|| v.clone())
    }

    /// The repository's generation now: pass it to [`ReadCache::put`] with the answer of a
    /// read started after this call.
    pub fn generation(&self, repo: &str) -> u64 {
        let g = self.repos.lock().unwrap_or_else(|p| p.into_inner());
        g.get(repo).map_or(0, |r| r.generation)
    }

    /// Keeps `value` for `key` unless the repository changed since `generation`.
    pub fn put(&self, repo: &str, generation: u64, key: &str, value: serde_json::Value) {
        let mut g = self.repos.lock().unwrap_or_else(|p| p.into_inner());
        if g.get(repo).map_or(0, |r| r.generation) != generation {
            return;
        }
        if !g.contains_key(repo) && g.len() >= REPOS {
            // the repository whose newest answer is the oldest
            let stale = g
                .iter()
                .filter(|(_, r)| !r.entries.is_empty())
                .min_by_key(|(_, r)| r.entries.values().map(|(at, _)| *at).max())
                .map(|(k, _)| k.clone());
            if let Some(k) = stale
                && let Some(r) = g.get_mut(&k)
            {
                r.entries.clear();
            }
        }
        let r = g.entry(repo.to_string()).or_default();
        r.entries.retain(|_, (at, _)| at.elapsed() < MAX_AGE);
        if r.entries.len() >= PER_REPO
            && let Some(oldest) = r.entries.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone())
        {
            r.entries.remove(&oldest);
        }
        r.entries.insert(key.to_string(), (Instant::now(), std::sync::Arc::new(value)));
    }

    /// Something may have changed `repo`: its answers go, reads running now are not kept.
    pub fn invalidate(&self, repo: &str) {
        let mut g = self.repos.lock().unwrap_or_else(|p| p.into_inner());
        let r = g.entry(repo.to_string()).or_default();
        r.generation += 1;
        r.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_stay_until_the_repository_changes() {
        let c = ReadCache::new();
        let g = c.generation("a");
        c.put("a", g, "snapshots", serde_json::json!([1]));
        assert_eq!(*c.get("a", "snapshots").unwrap(), serde_json::json!([1]));
        assert!(c.get("b", "snapshots").is_none());
        c.invalidate("a");
        assert!(c.get("a", "snapshots").is_none());
        // a read that started before the change is not kept
        c.put("a", g, "snapshots", serde_json::json!([2]));
        assert!(c.get("a", "snapshots").is_none());
        let g = c.generation("a");
        c.put("a", g, "snapshots", serde_json::json!([3]));
        assert_eq!(*c.get("a", "snapshots").unwrap(), serde_json::json!([3]));
    }

    #[test]
    fn a_repository_keeps_a_bounded_number_of_answers() {
        let c = ReadCache::new();
        for i in 0..PER_REPO + 10 {
            c.put("a", 0, &format!("k{i}"), serde_json::json!(i));
        }
        let n = c.repos.lock().unwrap().get("a").unwrap().entries.len();
        assert_eq!(n, PER_REPO);
    }
}
