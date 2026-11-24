//! Pinned snapshots: a small persisted set of absolute paths (`<state>/space/pins.json`,
//! 0600, written through a temporary file and a rename). Nothing rynas runs removes a pinned
//! snapshot: Space, the automatic thinning, the app stack pruning, the system restore point
//! pruning and the share job runs (btrbk prunes with `--preserve-snapshots` and rynas removes
//! the rest itself, see [`crate::jobs`]). Unpinning gives it back to its retention.
//!
//! The set is read by rynasd and by `rynasctl` (job runs), so the file is the source and the
//! process-wide copy ([`get`]) is only a cache refreshed on every write.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// The pins file below rynasd's state directory.
pub fn file(state_dir: &Path) -> PathBuf {
    state_dir.join("space").join("pins.json")
}

/// The default state directory's pins file (what `rynasctl` reads).
pub const DEFAULT_FILE: &str = "/var/lib/rynas/space/pins.json";

/// Most pins.
pub const MAX_PINS: usize = 1000;

/// Reads a pins file (empty when missing or unreadable).
pub fn read(path: &Path) -> BTreeSet<String> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

fn write(path: &Path, set: &BTreeSet<String>) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.new");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(&set.iter().collect::<Vec<_>>()).unwrap_or_default())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// The pins of this rynasd.
pub struct Pins {
    path: PathBuf,
    set: Mutex<BTreeSet<String>>,
}

static PINS: OnceLock<Arc<Pins>> = OnceLock::new();

/// Opens the pins of `state_dir` for the whole process (the first call wins).
pub fn install(state_dir: &Path) -> Arc<Pins> {
    PINS.get_or_init(|| Arc::new(Pins::at(file(state_dir)))).clone()
}

/// The process-wide pins, if [`install`] ran.
pub fn get() -> Option<&'static Arc<Pins>> {
    PINS.get()
}

/// Whether `path` is pinned (false before [`install`]).
pub fn is_pinned(path: &Path) -> bool {
    get().is_some_and(|p| p.contains(&path.display().to_string()))
}

impl Pins {
    /// Pins in a file of their own (tests).
    pub fn at(path: PathBuf) -> Self {
        Self { set: Mutex::new(read(&path)), path }
    }

    pub fn contains(&self, path: &str) -> bool {
        self.set.lock().unwrap().contains(path)
    }

    pub fn all(&self) -> BTreeSet<String> {
        self.set.lock().unwrap().clone()
    }

    /// Pins or unpins `path`; the new set is written before the cache changes.
    pub fn set(&self, path: &str, pinned: bool) -> Result<(), String> {
        let mut g = self.set.lock().unwrap();
        let mut next = g.clone();
        let changed = if pinned { next.insert(path.to_string()) } else { next.remove(path) };
        if !changed {
            return Ok(());
        }
        if next.len() > MAX_PINS {
            return Err(format!("at most {MAX_PINS} pinned snapshots"));
        }
        write(&self.path, &next).map_err(|e| format!("{}: {e}", self.path.display()))?;
        *g = next;
        Ok(())
    }

    /// Forgets pins of snapshots that no longer exist (`exists` says which do).
    pub fn prune(&self, exists: &dyn Fn(&str) -> bool) {
        let gone: Vec<String> = self.all().into_iter().filter(|p| !exists(p)).collect();
        for p in gone {
            let _ = self.set(&p, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_persist_and_prune() {
        let d = tempfile::tempdir().unwrap();
        let path = file(d.path());
        let p = Pins::at(path.clone());
        p.set("/vol1/.snapshots/a/a.1", true).unwrap();
        p.set("/vol1/.snapshots/a/a.2", true).unwrap();
        p.set("/vol1/.snapshots/a/a.2", true).unwrap();
        assert_eq!(read(&path).len(), 2);
        let again = Pins::at(path.clone());
        assert!(again.contains("/vol1/.snapshots/a/a.1"));
        again.set("/vol1/.snapshots/a/a.1", false).unwrap();
        assert_eq!(read(&path).len(), 1);
        again.prune(&|_| false);
        assert!(read(&path).is_empty());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
