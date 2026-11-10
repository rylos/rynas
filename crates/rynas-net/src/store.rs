//! The network engine's own state (`/var/lib/rynas/net`, root 0700), plain files written with
//! fsync so that `rynasctl` (timer, boot guard, watchdog, reset button) can work without
//! rynasd and without SQLite:
//!
//! - `revisions/<n>.json`: a [`Revision`]: the exact content (or absence) of every file it
//!   governs, with hashes, the services it wants enabled, the settings, and the observed
//!   state of its interfaces (to verify a rollback against what really ran);
//! - `state.json`: last known good revision, next id;
//! - `trial.json`: the [`Marker`] of an unconfirmed change (its presence is what the boot
//!   guard and the timer act on);
//! - `boot-adopt.json`: an ifupdown migration waiting for the next boot;
//! - `staged.json`: changes staged per user;
//! - `last-rollback.json`, `outbox.json`: the last rollback report and events waiting to be
//!   handed to the notification engine;
//! - `lock`: `flock` taken by every writer (rynasd, rynasctl).
//!
//! Retention: the last 50 revisions, plus the last known good one and the one of a trial.

use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use rynas_api_types::network::{InterfaceConfig, NetRollbackReport, NetTrial, TrialKind};
use rynas_api_types::notify::NewEvent;
use rynas_config::files::{rename_synced, sha256_hex, write_synced};
use serde::{Deserialize, Serialize};

/// Revisions kept.
pub const KEEP_REVISIONS: usize = 50;
/// Events kept while nobody collects them.
pub const OUTBOX_MAX: usize = 32;

/// Why a revision exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RevKind {
    /// What was on the system right before a change (the last known good).
    #[default]
    Snapshot,
    /// Interface settings applied by an administrator.
    Apply,
    /// ifupdown handed to networkd.
    Adopt,
    /// Every port on DHCP + link-local (reset button, watchdog).
    Rescue,
    /// A previous revision applied again.
    Restore,
}

/// One file as a revision wants it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    /// Absolute path on the target system.
    pub path: PathBuf,
    /// `None`: the file must not exist.
    pub content: Option<String>,
    #[serde(default = "default_mode")]
    pub mode: u32,
    /// SHA-256 of `content`.
    #[serde(default)]
    pub sha256: Option<String>,
}

fn default_mode() -> u32 {
    0o644
}

impl FileState {
    pub fn present(path: impl Into<PathBuf>, content: String) -> Self {
        let sha = sha256_hex(content.as_bytes());
        Self {
            path: path.into(),
            content: Some(content),
            mode: 0o644,
            sha256: Some(sha),
        }
    }

    pub fn absent(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            content: None,
            mode: 0o644,
            sha256: None,
        }
    }
}

/// Which network services a revision runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Services {
    pub networkd: bool,
    pub ifupdown: bool,
}

/// What an interface looked like when a snapshot was taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Observed {
    pub mtu: u32,
    /// Configured (non-leased) global IPv4 addresses, `a.b.c.d/nn`.
    pub static_v4: Vec<String>,
    /// Gateway of its default route, when not leased.
    #[serde(default)]
    pub gateway: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Revision {
    pub id: u64,
    pub ts_ms: u64,
    pub author: String,
    #[serde(default)]
    pub comment: String,
    pub kind: RevKind,
    /// rynas's interface settings in this revision (empty when rynas manages nothing).
    #[serde(default)]
    pub config: BTreeMap<String, InterfaceConfig>,
    pub files: Vec<FileState>,
    pub services: Services,
    /// Snapshots: the interfaces as they ran.
    #[serde(default)]
    pub observed: BTreeMap<String, Observed>,
    /// Confirmed (or a snapshot of a running system): safe to go back to.
    #[serde(default)]
    pub good: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StoreState {
    pub lkg: Option<u64>,
    pub next: u64,
}

/// The trial marker: an unconfirmed change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Marker {
    pub kind: TrialKind,
    pub revision: u64,
    pub lkg: u64,
    pub started_ms: u64,
    pub deadline_ms: u64,
    pub timeout_s: u32,
    /// SHA-256 of the confirmation token.
    pub token_sha256: String,
    pub interfaces: Vec<String>,
    pub management_touched: bool,
    /// Addresses the change removes (a confirmation arriving on one of them did not come
    /// over the new path).
    #[serde(default)]
    pub removed_addresses: Vec<String>,
    pub author: String,
    /// rynasd task that follows the trial.
    #[serde(default)]
    pub task_id: Option<String>,
}

impl Marker {
    pub fn view(&self) -> NetTrial {
        NetTrial {
            kind: self.kind,
            revision: self.revision,
            lkg_revision: self.lkg,
            started_ms: self.started_ms,
            deadline_ms: self.deadline_ms,
            timeout_s: self.timeout_s,
            interfaces: self.interfaces.clone(),
            management_touched: self.management_touched,
            author: self.author.clone(),
        }
    }
}

/// An ifupdown migration planned for the next boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BootAdopt {
    pub revision: u64,
    pub author: String,
    pub ts_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LastRollback {
    pub report: NetRollbackReport,
}

pub struct Store {
    dir: PathBuf,
}

/// An exclusive `flock` on the store, released on drop.
pub struct Lock {
    _file: std::fs::File,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.dir.join("revisions"))?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
        Ok(())
    }

    /// Takes the store lock (blocks while another writer holds it).
    pub fn lock(&self) -> std::io::Result<Lock> {
        self.ensure()?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("lock"))?;
        // SAFETY: flock on a descriptor we own
        let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Lock { _file: f })
    }

    fn read_json<T: for<'de> Deserialize<'de>>(&self, name: &str) -> std::io::Result<Option<T>> {
        match std::fs::read(self.dir.join(name)) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| std::io::Error::other(format!("{name}: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write_json<T: Serialize>(&self, name: &str, v: &T) -> std::io::Result<()> {
        self.ensure()?;
        let data = serde_json::to_vec_pretty(v).map_err(std::io::Error::other)?;
        let path = self.dir.join(name);
        let tmp = self.dir.join(format!(".{}.tmp", name.replace('/', "_")));
        write_synced(&tmp, &data, 0o600)?;
        rename_synced(&tmp, &path)
    }

    fn remove(&self, name: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.dir.join(name)) {
            Ok(()) => {
                if let Ok(d) = std::fs::File::open(&self.dir) {
                    let _ = d.sync_all();
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn state(&self) -> std::io::Result<StoreState> {
        Ok(self
            .read_json("state.json")?
            .unwrap_or(StoreState { lkg: None, next: 1 }))
    }

    pub fn set_lkg(&self, id: u64) -> std::io::Result<()> {
        let mut s = self.state()?;
        s.lkg = Some(id);
        self.write_json("state.json", &s)
    }

    /// Stores a new revision (its id is assigned here) and prunes old ones.
    pub fn add_revision(&self, mut r: Revision) -> std::io::Result<Revision> {
        let mut s = self.state()?;
        r.id = s.next.max(1);
        s.next = r.id + 1;
        self.write_json(&format!("revisions/{}.json", r.id), &r)?;
        self.write_json("state.json", &s)?;
        self.prune()?;
        Ok(r)
    }

    pub fn update_revision(&self, r: &Revision) -> std::io::Result<()> {
        self.write_json(&format!("revisions/{}.json", r.id), r)
    }

    pub fn revision(&self, id: u64) -> std::io::Result<Option<Revision>> {
        self.read_json(&format!("revisions/{id}.json"))
    }

    /// Ids of the stored revisions, oldest first.
    pub fn revision_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = std::fs::read_dir(self.dir.join("revisions"))
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .strip_suffix(".json")
                            .and_then(|n| n.parse().ok())
                    })
                    .collect()
            })
            .unwrap_or_default();
        ids.sort_unstable();
        ids
    }

    fn prune(&self) -> std::io::Result<()> {
        let ids = self.revision_ids();
        if ids.len() <= KEEP_REVISIONS {
            return Ok(());
        }
        let keep_lkg = self.state()?.lkg;
        let marker = self.marker()?;
        let boot = self.boot_adopt()?;
        let protected = |id: u64| {
            Some(id) == keep_lkg
                || marker
                    .as_ref()
                    .is_some_and(|m| m.revision == id || m.lkg == id)
                || boot.as_ref().is_some_and(|b| b.revision == id)
        };
        let excess = ids.len() - KEEP_REVISIONS;
        for id in ids.into_iter().filter(|i| !protected(*i)).take(excess) {
            self.remove(&format!("revisions/{id}.json"))?;
        }
        Ok(())
    }

    pub fn marker(&self) -> std::io::Result<Option<Marker>> {
        self.read_json("trial.json")
    }

    pub fn set_marker(&self, m: &Marker) -> std::io::Result<()> {
        self.write_json("trial.json", m)
    }

    pub fn clear_marker(&self) -> std::io::Result<()> {
        self.remove("trial.json")
    }

    pub fn boot_adopt(&self) -> std::io::Result<Option<BootAdopt>> {
        self.read_json("boot-adopt.json")
    }

    pub fn set_boot_adopt(&self, b: &BootAdopt) -> std::io::Result<()> {
        self.write_json("boot-adopt.json", b)
    }

    pub fn clear_boot_adopt(&self) -> std::io::Result<()> {
        self.remove("boot-adopt.json")
    }

    pub fn staged(&self) -> std::io::Result<BTreeMap<String, BTreeMap<String, InterfaceConfig>>> {
        Ok(self.read_json("staged.json")?.unwrap_or_default())
    }

    pub fn set_staged(
        &self,
        s: &BTreeMap<String, BTreeMap<String, InterfaceConfig>>,
    ) -> std::io::Result<()> {
        self.write_json("staged.json", s)
    }

    pub fn last_rollback(&self) -> std::io::Result<Option<LastRollback>> {
        self.read_json("last-rollback.json")
    }

    pub fn set_last_rollback(&self, r: &NetRollbackReport) -> std::io::Result<()> {
        self.write_json("last-rollback.json", &LastRollback { report: r.clone() })
    }

    pub fn clear_last_rollback(&self) -> std::io::Result<()> {
        self.remove("last-rollback.json")
    }

    /// Queues an event for the notification engine (rynasd collects them).
    pub fn push_event(&self, e: NewEvent) -> std::io::Result<()> {
        let mut v: Vec<NewEvent> = self.read_json("outbox.json")?.unwrap_or_default();
        v.push(e);
        let skip = v.len().saturating_sub(OUTBOX_MAX);
        let v: Vec<NewEvent> = v.into_iter().skip(skip).collect();
        self.write_json("outbox.json", &v)
    }

    /// Takes the queued events.
    pub fn take_events(&self) -> std::io::Result<Vec<NewEvent>> {
        let v: Vec<NewEvent> = self.read_json("outbox.json")?.unwrap_or_default();
        if !v.is_empty() {
            self.remove("outbox.json")?;
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_markers_and_pruning() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path().join("net"));
        let _l = s.lock().unwrap();
        assert_eq!(s.state().unwrap().lkg, None);
        let r = s
            .add_revision(Revision {
                files: vec![FileState::present("/etc/x", "a\n".into())],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.id, 1);
        assert_eq!(
            r.files[0].sha256.as_deref(),
            Some("87428fc522803d31065e7bce3cf03fe475096631e5e07bbd7a0fde60c4cf25c7")
        );
        s.set_lkg(1).unwrap();
        assert_eq!(s.revision(1).unwrap().unwrap(), r);
        for _ in 0..KEEP_REVISIONS + 5 {
            s.add_revision(Revision::default()).unwrap();
        }
        let ids = s.revision_ids();
        assert_eq!(ids.len(), KEEP_REVISIONS);
        assert_eq!(ids[0], 1, "the last known good revision is kept");
        assert_eq!(s.state().unwrap().lkg, Some(1));
        let m = Marker {
            revision: 7,
            token_sha256: "x".into(),
            ..Default::default()
        };
        s.set_marker(&m).unwrap();
        assert_eq!(s.marker().unwrap(), Some(m));
        s.clear_marker().unwrap();
        s.clear_marker().unwrap();
        assert_eq!(s.marker().unwrap(), None);
        for i in 0..OUTBOX_MAX + 3 {
            s.push_event(NewEvent {
                title: format!("e{i}"),
                ..Default::default()
            })
            .unwrap();
        }
        let ev = s.take_events().unwrap();
        assert_eq!(ev.len(), OUTBOX_MAX);
        assert_eq!(ev.last().unwrap().title, format!("e{}", OUTBOX_MAX + 2));
        assert!(s.take_events().unwrap().is_empty());
    }
}
