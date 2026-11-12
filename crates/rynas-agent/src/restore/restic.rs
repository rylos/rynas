//! The file method as a restore source: a restore session from a one-time restore code
//! (read-only, one PC; alive while it reads, ended by the report; its password opens a key
//! of its own that the NAS removes when the session ends), the group's repository opened with `rustic_core` over the
//! NAS' REST protocol, the PC's versions (tag `pc-<device>`), the manifest read from the
//! version itself, and a full restore with owners (numeric ids: the live system's users
//! mean nothing), modes, times, xattrs (ACLs, capabilities, SELinux labels), hard links and
//! sparse files.

use std::path::Path;
use std::sync::Arc;

use rustic_core::{
    Credentials, IndexedFullStatus, LocalDestination, LsOptions, Progress as RProgress, ProgressBars, ProgressType,
    Repository, RepositoryOptions, RestoreOptions, RusticProgress,
};
use rynas_api_types::pcagent::*;

use super::source::{Fill, Progress, RestoreSource, Version};
use crate::http::Client;

type Opened = Repository<IndexedFullStatus>;

pub struct ResticSource {
    client: Arc<Client>,
    pub dev: RestoreDevice,
    repo: Option<Opened>,
}

impl ResticSource {
    /// Logs in with a restore code; `device` picks one when the code covers several.
    pub fn login(client: Client, code: &str, device: Option<&str>) -> Result<ResticSource, String> {
        let s: RestoreSession = client.post_json("/api/v1/pcbackup/agent/restore/login", &RestoreLogin { code: code.to_string() })?;
        let dev = match device {
            Some(d) => s.devices.iter().find(|x| x.device == d).cloned(),
            None => s.devices.first().cloned(),
        }
        .ok_or("the restore code covers no such PC")?;
        Ok(ResticSource {
            client: Arc::new(client.with_token(&s.token)),
            dev,
            repo: None,
        })
    }

    fn repo(&mut self) -> Result<&Opened, String> {
        if self.repo.is_none() {
            let backends = crate::backend::NasBackend::new(self.client.clone(), &self.dev.group).backends();
            let r = Repository::new(&RepositoryOptions::default().no_cache(true), &backends)
                .map_err(|e| e.to_string())?
                .open(&Credentials::Password(self.dev.repo_password.clone()))
                .map_err(|e| format!("opening the repository: {e}"))?
                .to_indexed()
                .map_err(|e| e.to_string())?;
            self.repo = Some(r);
        }
        Ok(self.repo.as_ref().expect("set above"))
    }

    fn snapshot(&mut self, id: &str) -> Result<rustic_core::repofile::SnapshotFile, String> {
        let tag = device_tag(&self.dev.device);
        let repo = self.repo()?;
        let s = repo.get_snapshot_from_str(id, |_| true).map_err(|e| e.to_string())?;
        if !s.tags.iter().any(|t| *t == tag) {
            return Err(format!("version {id} is not a version of {}", self.dev.device));
        }
        Ok(s)
    }
}

/// rustic's progress, read by the callback of the data step.
#[derive(Debug)]
struct Shared {
    done: std::sync::atomic::AtomicU64,
    total: std::sync::atomic::AtomicU64,
}

#[derive(Debug)]
struct Bar(Arc<Shared>, bool);

impl RusticProgress for Bar {
    fn is_hidden(&self) -> bool {
        false
    }
    fn set_length(&self, len: u64) {
        if self.1 {
            self.0.total.store(len, std::sync::atomic::Ordering::Relaxed);
        }
    }
    fn set_title(&self, _: &str) {}
    fn inc(&self, inc: u64) {
        if self.1 {
            self.0.done.fetch_add(inc, std::sync::atomic::Ordering::Relaxed);
        }
    }
    fn finish(&self) {}
}

#[derive(Debug, Clone)]
struct SharedBars(Arc<Shared>);

impl ProgressBars for SharedBars {
    fn progress(&self, t: ProgressType, _: &str) -> RProgress {
        RProgress::new(Bar(self.0.clone(), matches!(t, ProgressType::Bytes)))
    }
}

impl RestoreSource for ResticSource {
    fn kind(&self) -> &'static str {
        "file backup"
    }

    fn device(&self) -> &str {
        &self.dev.device
    }

    fn versions(&mut self) -> Result<Vec<Version>, String> {
        let tag = device_tag(&self.dev.device);
        let repo = self.repo()?;
        let mut v: Vec<Version> = repo
            .get_all_snapshots()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|s| s.tags.iter().any(|t| *t == tag))
            .map(|s| Version {
                id: s.id.to_string(),
                time_ms: s.time.timestamp().as_millisecond().max(0) as u64,
                hostname: s.hostname.clone(),
                bytes: s.summary.as_ref().map(|x| x.total_bytes_processed).unwrap_or(0),
                files: s.summary.as_ref().map(|x| x.total_files_processed).unwrap_or(0),
            })
            .collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.time_ms));
        Ok(v)
    }

    fn manifest(&mut self, v: &Version) -> Result<Manifest, String> {
        let snap = self.snapshot(&v.id)?;
        let repo = self.repo()?;
        let node = repo
            .node_from_snapshot_and_path(&snap, MANIFEST_PATH)
            .map_err(|_| format!("version {} has no layout manifest ({MANIFEST_PATH})", &v.id[..8.min(v.id.len())]))?;
        if node.meta.size > MAX_MANIFEST as u64 {
            return Err("the manifest is too large".into());
        }
        let mut buf = Vec::new();
        repo.dump(&node, &mut buf).map_err(|e| e.to_string())?;
        serde_json::from_slice(&buf).map_err(|e| format!("the manifest does not parse: {e}"))
    }

    fn fill(&self) -> Fill {
        Fill::MountedTree
    }

    fn fill_tree(&mut self, v: &Version, root: &Path, p: Progress) -> Result<(), String> {
        let snap = self.snapshot(&v.id)?;
        let shared = Arc::new(Shared {
            done: Default::default(),
            total: Default::default(),
        });
        // a repository opened with progress bars for the restore
        let backends = crate::backend::NasBackend::new(self.client.clone(), &self.dev.group).backends();
        let repo = Repository::new_with_progress(&RepositoryOptions::default().no_cache(true), &backends, SharedBars(shared.clone()))
            .map_err(|e| e.to_string())?
            .open(&Credentials::Password(self.dev.repo_password.clone()))
            .map_err(|e| e.to_string())?
            .to_indexed()
            .map_err(|e| e.to_string())?;
        let node = repo.node_from_snapshot_and_path(&snap, "/").map_err(|e| e.to_string())?;
        let opts = RestoreOptions::default().numeric_id(true);
        let ls = repo.ls(&node, &LsOptions::default()).map_err(|e| e.to_string())?;
        let dest = LocalDestination::new(&root.to_string_lossy(), true, false).map_err(|e| e.to_string())?;
        let plan = repo.prepare_restore(&opts, ls.clone(), &dest, false).map_err(|e| e.to_string())?;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::scope(|sc| {
            let (sh, st) = (shared.clone(), stop.clone());
            sc.spawn(move || {
                while !st.load(std::sync::atomic::Ordering::SeqCst) {
                    p(sh.done.load(std::sync::atomic::Ordering::Relaxed), sh.total.load(std::sync::atomic::Ordering::Relaxed));
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            });
            let r = repo.restore(plan, &opts, ls, &dest).map_err(|e| e.to_string());
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            r
        })
    }

    fn report(&mut self, v: &Version, ok: bool, message: &str, log: &str) {
        let r = RestoreReport {
            device: self.dev.device.clone(),
            version: v.id.clone(),
            ok,
            message: message.to_string(),
            log: log.to_string(),
        };
        if let Err(e) = self.client.post_json::<_, serde_json::Value>("/api/v1/pcbackup/agent/restore/report", &r) {
            eprintln!("report to the NAS: {e}");
        }
    }
}
