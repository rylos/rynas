//! Offline deduplication of shared folders (`/v1/storage/dedup…`, D-DEDUP,
//! `docs/phase2/dedup.md`). Types and endpoints: `rynas_api_types::dedup`.
//!
//! - The share's switch lives in the share (`services` key); a job per filesystem (`dedup`
//!   key) runs every enabled share of it in one engine run, so duplicates are found across
//!   shares. The job's scheduler task (`TaskAction::Dedup`) is staged with its settings.
//! - A run: engine check → the filesystem lock ([`crate::fslock`], other heavy work goes
//!   first and stops a run) → optional standby check → roots that left the job forgotten →
//!   the engine with the whole command line, in a transient unit with a deadline (the end of
//!   the window block, SIGINT, 5 minutes to save) → its report → history → notification.
//! - The hashfile is a cache (on the volume's `@rynas-cache`, else in rynasd's state);
//!   rynasd's own state of a job is `<state>/dedup/<key>.json` ([`rynas_dedup::state`]).

pub mod host;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use jiff::Zoned;
use rynas_api_types::backup::{BACKUP_KEY, BackupConfig, Destination};
use rynas_api_types::dedup::*;
use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
use rynas_api_types::scheduler::{NotifyPolicy, ScheduledTask, SchedulerConfig, TaskAction, TaskTrigger};
use rynas_api_types::snapshots::SnapshotsConfig;
use rynas_api_types::storage::SYSTEM_VOLUME;
use rynas_api_types::{Share, SharesConfig, StagedChanges, TaskStarted, error_codes};
use rynas_btrfs::SNAPSHOTS_KEY;
use rynas_dedup::engine::unit_properties;
use rynas_dedup::excludes::{self, Root};
use rynas_dedup::state::{self, JobState};
use rynas_dedup::{DedupEngine, DedupJob, EngineLine, ExitKind, window};
use rynas_notify::event;
use rynas_scheduler::SCHEDULER_KEY;
use rynas_services::SHARES_KEY;
use rynas_tasks::{CancellationToken, TaskCtx};

pub use host::{DedupHost, FsFacts, HostDedup, MemoryDedup, RunSpec, Script};

use crate::fslock::FsLocks;
use crate::ops::{Error, Res, user};
use crate::protection::{Protection, conflict, invalid, not_found};
use crate::scheduler::check_task;
use crate::services::{docs, from_value, pending_op, stage};
use crate::snapshots::json_body;

/// The service of this rynasd (scheduled runs start from `crate::jobs`).
pub static SVC: OnceLock<Arc<DedupSvc>> = OnceLock::new();

/// Engine facts are read again after this long.
const ENGINE_TTL: Duration = Duration::from_secs(60);
/// A run asked for by hand ends at most this far ahead.
const MAX_MANUAL: u64 = 24 * 3600 * 1000;
/// Engine output lines kept in a task log (progress lines are not logged).
const LOG_LINES: usize = 400;
const LOG_LINE_CHARS: usize = 400;

/// A job running or waiting.
struct Live {
    view: DedupRunning,
    /// An administrator's stop (or the task's cancel).
    stop: CancellationToken,
}

pub struct DedupSvc {
    pub prot: Arc<Protection>,
    pub host: Arc<dyn DedupHost>,
    pub engine: Arc<dyn DedupEngine>,
    /// `<state>/dedup`: job states, and hashfiles of filesystems without `@rynas-cache`.
    pub dir: PathBuf,
    /// Names the hardware modules exclude (`Module::dedup_excludes`).
    pub module_excludes: Vec<String>,
    pub locks: Arc<FsLocks>,
    live: Mutex<HashMap<String, Arc<Mutex<Live>>>>,
    engine_cache: Mutex<Option<(Instant, DedupEngineInfo)>>,
}

/// A filesystem with its shares.
#[derive(Debug, Clone)]
struct Job {
    key: String,
    volume: Option<String>,
    /// The place's folder (volumes) or the mount point.
    dir: String,
    facts: FsFacts,
    shares: Vec<(Share, DedupShare)>,
}

/// What a run needs, worked out before the task starts.
#[derive(Debug, Clone)]
struct Plan {
    key: String,
    volume_label: String,
    mountpoint: String,
    fs_key: String,
    hashfile: String,
    settings: VolumeDedup,
    shares: Vec<String>,
    roots: Vec<String>,
    excludes: Vec<String>,
}

fn dedup_fs(f: &FsFacts) -> bool {
    f.fs_type == "btrfs" || (f.fs_type == "xfs" && f.reflink == Some(true))
}

fn within(path: &str, dir: &str) -> bool {
    let d = dir.trim_end_matches('/');
    d.is_empty() || path == d || path.strip_prefix(d).is_some_and(|r| r.starts_with('/'))
}

fn now_ms() -> u64 {
    rynas_tasks::now_ms()
}

/// `1.5 GiB`.
pub fn human(b: u64) -> String {
    const U: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

fn audit(who: &str, what: &str) {
    tracing::info!(target: "rynas::audit", "dedup {what} by {who}");
}

impl DedupSvc {
    pub fn new(
        prot: Arc<Protection>,
        host: Arc<dyn DedupHost>,
        engine: Arc<dyn DedupEngine>,
        dir: PathBuf,
        module_excludes: Vec<String>,
        locks: Arc<FsLocks>,
    ) -> Self {
        Self {
            prot,
            host,
            engine,
            dir,
            module_excludes,
            locks,
            live: Mutex::new(HashMap::new()),
            engine_cache: Mutex::new(None),
        }
    }

    // ---- engine ----

    fn engine_info(&self, fresh: bool) -> DedupEngineInfo {
        if !fresh
            && let Some((at, i)) = self.engine_cache.lock().unwrap().as_ref()
            && at.elapsed() < ENGINE_TTL
        {
            return i.clone();
        }
        let version = self
            .host
            .engine_version(&self.engine.version_command())
            .and_then(|o| self.engine.parse_version(&o));
        let (min, max) = self.engine.tested_range();
        let installed = version.is_some();
        let info = DedupEngineInfo {
            installed,
            supported: version.as_deref().is_some_and(|v| self.engine.supported(v)),
            version,
            min_version: min.into(),
            max_version: max.into(),
            installable: !installed && self.host.package_source().is_some(),
        };
        *self.engine_cache.lock().unwrap() = Some((Instant::now(), info.clone()));
        info
    }

    // ---- filesystems and shares ----

    fn applied<T: serde::de::DeserializeOwned + Default>(&self, key: &str) -> Res<T> {
        from_value(self.prot.ops.config.applied(key)?)
    }

    /// The jobs: every volume whose filesystem can be deduplicated, every other filesystem
    /// with an enabled share or settings (`configured`), and the shares on none of them.
    fn discover(&self, shares: &[Share], snaps: &SnapshotsConfig, configured: &[String]) -> (Vec<Job>, Vec<DedupShare>) {
        let mut jobs: Vec<Job> = Vec::new();
        for (label, dir) in self.host.places() {
            let Some(f) = self.host.fs_of(&dir) else { continue };
            if dedup_fs(&f) {
                jobs.push(Job { key: label.clone(), volume: Some(label), dir, facts: f, shares: Vec::new() });
            }
        }
        let places = self.host.places();
        let mut others = Vec::new();
        for sh in shares {
            let st = sh.dedup.clone().unwrap_or_default();
            let path = sh.path.clone();
            let snap_jobs: Vec<_> = snaps
                .jobs
                .iter()
                .filter(|j| j.share.as_deref() == Some(sh.name.as_str()) || j.subvolume == path)
                .collect();
            let mut view = DedupShare {
                name: sh.name.clone(),
                path: path.clone(),
                enabled: st.enabled,
                block: None,
                snapshots: !snap_jobs.is_empty(),
                replicated: snap_jobs.iter().any(|j| !j.replicas.is_empty()),
                include_recycle: st.include_recycle,
            };
            let facts = if sh.encrypted { None } else { self.host.fs_of(&path) };
            let place_job = jobs.iter().position(|j| j.volume.is_some() && within(&path, &j.dir));
            view.block = if sh.encrypted {
                Some(DedupBlock::Encrypted)
            } else if sh.subvolume.as_ref().is_some_and(|v| v.nocow) || self.host.nocow(&path) {
                Some(DedupBlock::Nocow)
            } else {
                match &facts {
                    None => Some(DedupBlock::NotMounted),
                    Some(f) if f.fs_type == "xfs" && f.reflink != Some(true) => Some(DedupBlock::XfsNoReflink),
                    Some(f) if !dedup_fs(f) => Some(DedupBlock::Filesystem),
                    Some(f) => match place_job {
                        Some(i) if jobs[i].facts.key != f.key => Some(DedupBlock::OtherFilesystem),
                        _ => None,
                    },
                }
            };
            let idx = match place_job {
                Some(i) => Some(i),
                None => match &facts {
                    // a filesystem that is no volume: a job of its own, once wanted
                    Some(f) if dedup_fs(f) && view.block.is_none() => f.uuid.as_ref().and_then(|u| {
                        let key = format!("{UUID_KEY_PREFIX}{u}");
                        if !(st.enabled || configured.contains(&key)) {
                            return None;
                        }
                        // a place on the same filesystem (outside its folder) is not this job
                        if places.iter().any(|(_, d)| self.host.fs_of(d).is_some_and(|pf| pf.key == f.key)) {
                            return None;
                        }
                        Some(match jobs.iter().position(|j| j.key == key) {
                            Some(i) => i,
                            None => {
                                jobs.push(Job { key, volume: None, dir: f.mountpoint.clone(), facts: f.clone(), shares: Vec::new() });
                                jobs.len() - 1
                            }
                        })
                    }),
                    _ => None,
                },
            };
            match idx {
                Some(i) => jobs[i].shares.push((sh.clone(), view)),
                None => {
                    if view.block.is_none() {
                        view.block = Some(DedupBlock::OtherFilesystem);
                    }
                    others.push(view);
                }
            }
        }
        for k in configured {
            if !jobs.iter().any(|j| &j.key == k) {
                jobs.push(Job { key: k.clone(), volume: None, dir: String::new(), facts: FsFacts::default(), shares: Vec::new() });
            }
        }
        (jobs, others)
    }

    fn hashfile(&self, job: &Job) -> String {
        if job.facts.fs_type == "btrfs"
            && let Some(v) = &job.volume
            && v != SYSTEM_VOLUME
        {
            let cache = format!("{}/@rynas-cache", job.dir.trim_end_matches('/'));
            if self.host.is_dir(&cache) {
                return format!("{cache}/dedup/volume.hash");
            }
        }
        self.dir.join(format!("{}.hash", job.key)).to_string_lossy().into_owned()
    }

    /// Folders of the volume never looked at: Docker's engine data, local backup
    /// repositories.
    fn absolute_excludes(&self, job: &Job) -> Vec<String> {
        let mut v = Vec::new();
        if job.volume.is_some() && !job.dir.is_empty() {
            let d = job.dir.trim_end_matches('/');
            for sub in ["docker/engine", "docker/containerd", "docker/.snapshots"] {
                v.push(format!("{d}/{sub}"));
            }
        }
        if let Ok(b) = self.applied::<BackupConfig>(BACKUP_KEY) {
            for j in b.jobs {
                if let Destination::Local { path } = j.destination
                    && job.shares.iter().any(|(s, _)| within(&path, &s.path))
                {
                    v.push(path);
                }
            }
        }
        v
    }

    fn view(&self, job: &Job, eff: &DedupConfig, cur: &DedupConfig, now: &Zoned) -> DedupJobView {
        let settings = eff.jobs.get(&job.key).cloned();
        let st = state::read(&self.dir, &job.key);
        let running = self.live.lock().unwrap().get(&job.key).map(|l| l.lock().unwrap().view.clone());
        let enabled: Vec<&DedupShare> = job.shares.iter().map(|(_, v)| v).filter(|v| v.enabled).collect();
        let mut warnings = Vec::new();
        if settings.is_some() && enabled.is_empty() {
            warnings.push(DedupWarning::NoShares);
        }
        if enabled.iter().any(|v| v.block.is_some()) {
            warnings.push(DedupWarning::SharesBlocked);
        }
        if enabled.iter().any(|v| v.replicated) {
            warnings.push(DedupWarning::Replicated);
        }
        if job.facts.fs_type == "btrfs" && !job.facts.mountpoint.is_empty() && self.host.qgroups(&job.facts.mountpoint) {
            warnings.push(DedupWarning::Qgroups);
        }
        if job.volume.as_deref() == Some(SYSTEM_VOLUME) {
            warnings.push(DedupWarning::Flash);
        }
        DedupJobView {
            key: job.key.clone(),
            volume: job.volume.clone(),
            mountpoint: job.facts.mountpoint.clone(),
            fs_type: job.facts.fs_type.clone(),
            next_window_ms: settings
                .as_ref()
                .filter(|s| s.enabled)
                .and_then(|s| window::next_start(&s.window, now))
                .map(|z| window::ms(&z)),
            settings,
            pending: pending_op(eff.jobs.get(&job.key), cur.jobs.get(&job.key)),
            task_id: task_id_of(&job.key),
            shares: job.shares.iter().map(|(_, v)| v.clone()).collect(),
            running,
            last: st.last().cloned(),
            estimate: st.estimate.clone(),
            report: st.report.clone(),
            warnings,
            hashfile: self.hashfile(job),
        }
    }

    fn overview(&self, owner: &str) -> Res<DedupOverview> {
        let (eff, cur, _): (DedupConfig, DedupConfig, _) = docs(&self.prot.ops.config, owner, DEDUP_KEY)?;
        let shares: SharesConfig = self.applied(SHARES_KEY)?;
        let snaps: SnapshotsConfig = self.applied(SNAPSHOTS_KEY)?;
        let configured: Vec<String> = eff.jobs.keys().chain(cur.jobs.keys()).cloned().collect();
        let (jobs, others) = self.discover(&shares.shares, &snaps, &configured);
        let now = Zoned::now();
        Ok(DedupOverview {
            engine: self.engine_info(false),
            jobs: jobs.iter().map(|j| self.view(j, &eff, &cur, &now)).collect(),
            other_shares: others,
            always_excluded: excludes::shown_always(&self.module_excludes),
        })
    }

    fn job_view(&self, owner: &str, key: &str) -> Res<DedupJobView> {
        self.overview(owner)?
            .jobs
            .into_iter()
            .find(|j| j.key == key)
            .ok_or_else(|| not_found(format!("deduplication job {key}")))
    }

    /// What a run of `key` covers (applied configuration).
    fn plan(&self, key: &str, kind: DedupKind, chosen: &[String]) -> Res<Plan> {
        let cfg: DedupConfig = self.applied(DEDUP_KEY)?;
        let shares: SharesConfig = self.applied(SHARES_KEY)?;
        let snaps: SnapshotsConfig = self.applied(SNAPSHOTS_KEY)?;
        let configured: Vec<String> = cfg.jobs.keys().cloned().collect();
        let (jobs, _) = self.discover(&shares.shares, &snaps, &configured);
        let job = jobs
            .into_iter()
            .find(|j| j.key == key)
            .ok_or_else(|| not_found(format!("deduplication job {key}")))?;
        if job.facts.mountpoint.is_empty() {
            return Err(conflict(error_codes::CONFLICT, format!("the filesystem of job {key} is not mounted")));
        }
        let settings = match (cfg.jobs.get(key), kind) {
            (Some(s), _) => s.clone(),
            (None, DedupKind::Estimate) => VolumeDedup::default(),
            (None, DedupKind::Run) => {
                return Err(conflict(error_codes::CONFLICT, format!("deduplication of {key} is not set up: save its settings first")));
            }
        };
        let usable = |v: &DedupShare| v.block.is_none();
        let picked: Vec<&(Share, DedupShare)> = match kind {
            DedupKind::Run => job.shares.iter().filter(|(_, v)| v.enabled && usable(v)).collect(),
            DedupKind::Estimate if !chosen.is_empty() => {
                let mut v = Vec::new();
                for n in chosen {
                    let s = job
                        .shares
                        .iter()
                        .find(|(s, _)| &s.name == n)
                        .ok_or_else(|| invalid(format!("share {n} is not on {key}")))?;
                    if !usable(&s.1) {
                        return Err(invalid(format!("share {n} cannot be deduplicated")));
                    }
                    v.push(s);
                }
                v
            }
            DedupKind::Estimate => {
                let on: Vec<_> = job.shares.iter().filter(|(_, v)| v.enabled && usable(v)).collect();
                if on.is_empty() { job.shares.iter().filter(|(_, v)| usable(v)).collect() } else { on }
            }
        };
        let roots: Vec<Root<'_>> = picked
            .iter()
            .map(|(s, _)| Root {
                path: s.path.as_str(),
                settings: s.dedup.as_ref().unwrap_or(&DEFAULT_SHARE),
            })
            .collect();
        let ex = excludes::build(&roots, &self.module_excludes, &self.absolute_excludes(&job));
        Ok(Plan {
            key: key.to_string(),
            volume_label: job.volume.clone().unwrap_or_else(|| job.facts.mountpoint.clone()),
            mountpoint: job.facts.mountpoint.clone(),
            fs_key: job.facts.key.clone(),
            hashfile: self.hashfile(&job),
            settings,
            shares: picked.iter().map(|(s, _)| s.name.clone()).collect(),
            roots: picked.iter().map(|(s, _)| s.path.clone()).collect(),
            excludes: ex,
        })
    }

    // ---- runs ----

    fn running(&self, key: &str) -> bool {
        self.live.lock().unwrap().contains_key(key)
    }

    /// Starts a run or an estimate; returns the task id.
    pub fn start(self: &Arc<Self>, key: &str, kind: DedupKind, until_ms: u64, chosen: &[String], who: &str, scheduled: bool) -> Res<String> {
        let eng = self.engine_info(false);
        if !eng.installed {
            return Err(conflict(error_codes::CONFLICT, "the deduplication engine is not installed"));
        }
        if !eng.supported {
            return Err(conflict(
                "engine_unsupported",
                format!(
                    "oans {} is outside the versions rynas was tested with ({} to {})",
                    eng.version.unwrap_or_default(),
                    eng.min_version,
                    eng.max_version
                ),
            ));
        }
        let plan = self.plan(key, kind, chosen)?;
        let live = {
            let mut m = self.live.lock().unwrap();
            if m.contains_key(key) {
                return Err(conflict(error_codes::CONFLICT, format!("deduplication of {key} is already running")));
            }
            let l = Arc::new(Mutex::new(Live {
                view: DedupRunning { task_id: String::new(), kind, started_ms: now_ms(), until_ms, ..Default::default() },
                stop: CancellationToken::new(),
            }));
            m.insert(key.to_string(), l.clone());
            l
        };
        let task_kind = match kind {
            DedupKind::Run => "storage.dedup",
            DedupKind::Estimate => "storage.dedup-estimate",
        };
        let s = self.clone();
        let l2 = live.clone();
        let k2 = key.to_string();
        let r = self.prot.ops.tasks.spawn(task_kind, who, move |ctx| async move {
            let res = s.run_task(&ctx, plan, kind, until_ms, scheduled, &l2).await;
            s.live.lock().unwrap().remove(&k2);
            res
        });
        match r {
            Ok(id) => {
                live.lock().unwrap().view.task_id = id.clone();
                Ok(id)
            }
            Err(e) => {
                self.live.lock().unwrap().remove(key);
                Err(e.into())
            }
        }
    }

    fn record(&self, key: &str, f: impl FnOnce(&mut JobState)) {
        let mut st = state::read(&self.dir, key);
        f(&mut st);
        if let Err(e) = state::write(&self.dir, key, &st) {
            tracing::warn!("dedup: state of {key}: {e}");
        }
    }

    async fn run_task(
        self: &Arc<Self>,
        ctx: &TaskCtx,
        plan: Plan,
        kind: DedupKind,
        until_ms: u64,
        scheduled: bool,
        live: &Arc<Mutex<Live>>,
    ) -> Result<(), String> {
        let user_stop = live.lock().unwrap().stop.clone();
        let started = now_ms();
        let base = DedupRecord {
            kind,
            start_ms: started,
            shares: plan.shares.clone(),
            task_id: Some(ctx.id().to_string()),
            ..Default::default()
        };
        ctx.log(&format!(
            "{} of {} until {} ({} shares: {})",
            if kind == DedupKind::Run { "deduplication" } else { "estimate" },
            plan.volume_label,
            jiff::Timestamp::from_millisecond(until_ms as i64).map(|t| t.to_string()).unwrap_or_default(),
            plan.roots.len(),
            plan.shares.join(", ")
        ));
        let skip = |msg: String, by: Option<String>| {
            let rec = DedupRecord { outcome: DedupOutcome::Skipped, end_ms: now_ms(), message: msg.clone(), by, ..base.clone() };
            ctx.log(&format!("skipped: {msg}"));
            self.record(&plan.key, |st| st.push(rec));
            Ok(())
        };
        if plan.roots.is_empty() {
            return skip("no shared folder to deduplicate".into(), None);
        }
        loop {
            if scheduled && plan.settings.only_if_awake && self.host.asleep(&plan.mountpoint) {
                return skip("the disks are in standby".into(), None);
            }
            // the filesystem lock: wait for other work, until the deadline or a stop
            let deadline = tokio::time::Instant::now() + Duration::from_millis(until_ms.saturating_sub(now_ms()));
            let l3 = live.clone();
            let waiting = move |h: Option<&str>| {
                l3.lock().unwrap().view.waiting_for = h.map(String::from);
            };
            let got = tokio::select! {
                r = self.locks.dedup(&plan.fs_key, deadline, &waiting) => r,
                _ = user_stop.cancelled() => Err("stop".into()),
                _ = ctx.token().cancelled() => Err("stop".into()),
            };
            let guard = match got {
                Ok(g) => g,
                Err(h) if h == "stop" => {
                    let rec = DedupRecord { outcome: DedupOutcome::Stopped, end_ms: now_ms(), ..base.clone() };
                    self.record(&plan.key, |st| st.push(rec));
                    return Ok(());
                }
                Err(h) => return skip(format!("{h} was running until the window closed"), Some(h)),
            };
            let rec = self.run_once(ctx, &plan, kind, until_ms, &guard, live, &user_stop, &base).await;
            let again = rec.outcome == DedupOutcome::Preempted && now_ms() + 60_000 < until_ms;
            drop(guard);
            let failed = rec.outcome == DedupOutcome::Failed;
            let msg = rec.message.clone();
            self.notify(&plan, &rec);
            self.record(&plan.key, |st| st.push(rec));
            if again {
                ctx.log("other work holds the filesystem: going on after it, while the window is open");
                live.lock().unwrap().view.progress = None;
                continue;
            }
            return if failed { Err(msg) } else { Ok(()) };
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_once(
        &self,
        ctx: &TaskCtx,
        plan: &Plan,
        kind: DedupKind,
        until_ms: u64,
        guard: &crate::fslock::DedupGuard,
        live: &Arc<Mutex<Live>>,
        user_stop: &CancellationToken,
        base: &DedupRecord,
    ) -> DedupRecord {
        let dedupe = kind == DedupKind::Run;
        let mut st = state::read(&self.dir, &plan.key);
        let hashdir = std::path::Path::new(&plan.hashfile)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let fail = |msg: String| DedupRecord { outcome: DedupOutcome::Failed, end_ms: now_ms(), message: msg, ..base.clone() };
        if let Err(e) = self.host.prepare_dir(&hashdir) {
            return fail(format!("hashfile folder: {e}"));
        }
        // a new block size: the stored hashes are of no use
        if st.block_size.is_some_and(|b| b != plan.settings.block_size) {
            ctx.log("the block size changed: the hashfile starts again (everything is hashed once more)");
            if let Err(e) = self.host.remove_hashfile(&plan.hashfile) {
                return fail(e);
            }
            st.roots.clear();
        }
        // roots that left the job: forgotten, so their hashes do not linger
        if dedupe {
            let gone: Vec<String> = st.roots.iter().filter(|r| !plan.roots.contains(r)).cloned().collect();
            if !gone.is_empty() {
                let argv = self.engine.remove_command(&plan.hashfile, &gone);
                ctx.log(&format!("forgetting {}", gone.join(", ")));
                match self.host.exec(argv).await {
                    Ok((Some(0), _, _)) => {}
                    Ok((c, _, e)) => ctx.log(&format!("not all forgotten (exit {c:?}): {}", e.trim())),
                    Err(e) => ctx.log(&format!("not forgotten: {e}")),
                }
            }
        }
        let job = DedupJob {
            hashfile: plan.hashfile.clone(),
            roots: plan.roots.clone(),
            excludes: plan.excludes.clone(),
            settings: plan.settings.clone(),
            dedupe,
        };
        let argv = self.engine.command(&job);
        let mut writable = plan.roots.clone();
        writable.push(hashdir.clone());
        let spec = RunSpec {
            key: plan.key.clone(),
            argv,
            properties: unit_properties(&plan.settings, &writable),
            max: Duration::from_millis(until_ms.saturating_sub(now_ms()).max(1000)),
        };
        ctx.log(&format!("$ {}", spec.argv.join(" ")));
        // every reason to stop ends in one token for the engine
        let stop = CancellationToken::new();
        let watcher = {
            let (stop, user, pre, task) = (stop.clone(), user_stop.clone(), guard.preempt.clone(), ctx.token().clone());
            let live = live.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = user.cancelled() => {}
                    _ = pre.cancelled() => {}
                    _ = task.cancelled() => {}
                    _ = stop.cancelled() => return,
                }
                live.lock().unwrap().view.stopping = true;
                stop.cancel();
            })
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        // the engine's final line: (reclaimed bytes, files scanned)
        type Finished = Option<(u64, Option<u64>)>;
        let done: Arc<Mutex<Finished>> = Arc::new(Mutex::new(None));
        let consumer = {
            let (engine, done, live, ctx) = (self.engine.clone(), done.clone(), live.clone(), ctx.clone());
            tokio::spawn(async move {
                let mut logged = 0usize;
                let mut last_pct = None;
                let mut last_note = Instant::now();
                while let Some(line) = rx.recv().await {
                    let l = line.strip_prefix("stderr: ").unwrap_or(&line);
                    match engine.parse_line(l) {
                        Some(EngineLine::Progress(p)) => {
                            let pct = overall_pct(&p);
                            if last_pct != Some(pct) {
                                ctx.progress(pct);
                                last_pct = Some(pct);
                            }
                            if last_note.elapsed() >= Duration::from_secs(300) {
                                ctx.log(&format!("{}: {pct} %", p.phase));
                                last_note = Instant::now();
                            }
                            live.lock().unwrap().view.progress = Some(p);
                        }
                        Some(EngineLine::Done { reclaimed_bytes, files_scanned }) => {
                            *done.lock().unwrap() = Some((reclaimed_bytes, files_scanned));
                        }
                        None if !l.trim().is_empty() => {
                            if logged < LOG_LINES {
                                ctx.log(&l.chars().take(LOG_LINE_CHARS).collect::<String>());
                            } else if logged == LOG_LINES {
                                ctx.log("(more engine messages left out)");
                            }
                            logged += 1;
                        }
                        None => {}
                    }
                }
            })
        };
        let end = self.host.run(spec, tx, stop.clone()).await;
        let _ = consumer.await;
        let stopping = stop.is_cancelled();
        stop.cancel();
        let _ = watcher.await;
        live.lock().unwrap().view.stopping = false;
        let finished = *done.lock().unwrap();
        let mut rec = DedupRecord { end_ms: now_ms(), exit_code: end.code, ..base.clone() };
        if let Some((r, files)) = finished {
            rec.reclaimed_bytes = r;
            rec.files_hashed = files;
        } else if let Some(p) = live.lock().unwrap().view.progress.as_ref() {
            rec.reclaimed_bytes = p.reclaimed_bytes;
        }
        let kind_of_exit = self.engine.exit_kind(end.code);
        rec.outcome = match (end.start_error.as_ref(), kind_of_exit) {
            (Some(e), _) => {
                rec.message = format!("the engine did not start: {e}");
                DedupOutcome::Failed
            }
            (None, ExitKind::Done) => DedupOutcome::Done,
            (None, ExitKind::Partial) => {
                rec.message = "some shares could not be covered (missing, unmounted or on another filesystem)".into();
                rec.missing = plan.roots.iter().filter(|r| !self.host.is_dir(r)).cloned().collect();
                DedupOutcome::Partial
            }
            (None, k) => {
                if guard.preempt.is_cancelled() {
                    rec.by = guard.preempted_by();
                    DedupOutcome::Preempted
                } else if stopping {
                    DedupOutcome::Stopped
                } else if now_ms() + 5_000 >= until_ms || k == ExitKind::Interrupted {
                    DedupOutcome::WindowEnded
                } else {
                    rec.message = format!("the engine failed (exit {})", end.code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into()));
                    DedupOutcome::Failed
                }
            }
        };
        // the hashfile's figures (read-only, quick)
        let report = match self.host.exec(self.engine.report_command(&plan.hashfile)).await {
            Ok((Some(0), out, _)) => self.engine.parse_report(&out),
            _ => None,
        };
        if let Some(r) = &report {
            rec.skipped_files = r.skipped_last_run.values().sum();
        }
        let failed = rec.outcome == DedupOutcome::Failed;
        let block = plan.settings.block_size;
        let roots = plan.roots.clone();
        let shares = plan.shares.clone();
        self.record(&plan.key, |s| {
            if let Some(mut r) = report.clone() {
                r.read_ms = now_ms();
                if kind == DedupKind::Estimate && rec.outcome == DedupOutcome::Done {
                    s.estimate = Some(DedupEstimate {
                        at_ms: now_ms(),
                        shares,
                        reclaimable_bytes: r.reclaimable_bytes,
                        dup_files: r.dup_files,
                        files: r.files_tracked,
                        logical_bytes: r.logical_bytes,
                    });
                }
                s.report = Some(r);
            }
            if dedupe {
                s.roots = roots;
            }
            s.block_size = Some(block);
            s.failures = if failed { s.failures + 1 } else { 0 };
        });
        // a second failure in a row with an unusable hashfile: start it again
        if failed && st.failures >= 1 && report.is_none() {
            ctx.log("the hashfile cannot be read after two failed runs: it starts again");
            if let Err(e) = self.host.remove_hashfile(&plan.hashfile) {
                ctx.log(&format!("hashfile not removed: {e}"));
            }
            self.record(&plan.key, |s| {
                s.roots.clear();
                s.failures = 0;
            });
        }
        ctx.log(&format!(
            "{:?}: {} freed{}",
            rec.outcome,
            human(rec.reclaimed_bytes),
            rec.by.as_ref().map(|b| format!(", stopped for {b}")).unwrap_or_default()
        ));
        rec
    }

    fn notify(&self, plan: &Plan, rec: &DedupRecord) {
        let (sev, code, title) = match rec.outcome {
            DedupOutcome::Done if rec.kind == DedupKind::Run => (
                NotifySeverity::Info,
                "dedup.ok",
                format!("Deduplication of {} done: {} freed", plan.volume_label, human(rec.reclaimed_bytes)),
            ),
            DedupOutcome::Partial => (
                NotifySeverity::Warning,
                "dedup.partial",
                format!("Deduplication of {} covered only part of its shares", plan.volume_label),
            ),
            DedupOutcome::Failed => (NotifySeverity::Error, "dedup.failed", format!("Deduplication of {} failed", plan.volume_label)),
            _ => return,
        };
        let mut ev = event::oneshot(NotifyCategory::Storage, sev, "dedup", format!("dedup:{}:{code}", plan.key), title);
        ev.body = match rec.outcome {
            DedupOutcome::Partial if !rec.missing.is_empty() => format!("{}\n{}", rec.message, rec.missing.join("\n")),
            _ => rec.message.clone(),
        };
        let ev = event::field(ev, "volume", &plan.volume_label);
        let ev = event::field(ev, "reclaimed", human(rec.reclaimed_bytes));
        let ev = event::field(ev, "shares", plan.shares.join(", "));
        self.prot.notifier.emit(event::code(ev, code));
    }

    /// Asks a running job to stop (the engine saves its work).
    pub fn stop(&self, key: &str) -> bool {
        match self.live.lock().unwrap().get(key) {
            Some(l) => {
                let mut l = l.lock().unwrap();
                l.view.stopping = true;
                l.stop.cancel();
                true
            }
            None => false,
        }
    }
}

static DEFAULT_SHARE: ShareDedup = ShareDedup { enabled: false, excludes: Vec::new(), include_recycle: false };

/// One percentage over the phases: scanning 0–5, hashing 5–70, dedupe 70–100.
pub fn overall_pct(p: &DedupProgress) -> u8 {
    let frac = if p.total > 0 { (p.done as f64 / p.total as f64).clamp(0.0, 1.0) } else { 0.0 };
    let v = match p.phase.as_str() {
        "scanning" => 2.0,
        "hashing" => 5.0 + 65.0 * frac,
        "dedupe" => 70.0 + 30.0 * frac,
        _ => 0.0,
    };
    v.round().clamp(0.0, 100.0) as u8
}

/// A task type or scheduler unit of deduplication: never counted as busy by a scheduled
/// shutdown (the run is stopped with SIGINT, saves its work and resumes in its next window).
pub fn gives_way(kind_or_unit: &str) -> bool {
    kind_or_unit.starts_with("storage.dedup") || kind_or_unit.starts_with("rynas-task-dedup-")
}

/// The scheduler task of a job.
pub fn job_task(key: &str, s: &VolumeDedup) -> ScheduledTask {
    ScheduledTask {
        id: task_id_of(key),
        name: format!("Deduplication {key}"),
        description: String::new(),
        enabled: s.enabled,
        action: TaskAction::Dedup { volume: key.to_string() },
        trigger: TaskTrigger::Calendar {
            on_calendar: window::on_calendar(&s.window),
            cron: None,
            // a run missed while the machine was off is not caught up outside the window
            persistent: false,
            randomized_delay_s: 0,
        },
        timeout_s: None,
        nice: None,
        io_weight: None,
        // the run notifies by itself (dedup.ok / partial / failed)
        notify: NotifyPolicy::Never,
        notify_output: false,
    }
}

/// Started by a job's scheduler task (`rynasctl job run`): runs until the end of the
/// window block. Outside the window, or while the job already runs, a task that only says
/// so (the timer may fire inside a block or outside the window: one calendar expression).
pub async fn spawn_scheduled(key: &str, who: &str) -> Res<String> {
    let s = SVC
        .get()
        .cloned()
        .ok_or_else(|| Error::internal("deduplication is not enabled in this rynasd"))?;
    let (k, w) = (key.to_string(), who.to_string());
    let s2 = s.clone();
    let decided: Result<(DedupKind, u64), String> = tokio::task::spawn_blocking(move || {
        let cfg: DedupConfig = s2.applied(DEDUP_KEY).map_err(|e| e.2)?;
        let Some(set) = cfg.jobs.get(&k) else {
            return Err(format!("deduplication of {k} is not set up"));
        };
        if !set.enabled {
            return Err(format!("deduplication of {k} is off"));
        }
        if s2.running(&k) {
            return Err(format!("deduplication of {k} is already running"));
        }
        let now = Zoned::now();
        match window::block_end(&set.window, &now) {
            Some(end) => Ok((DedupKind::Run, window::ms(&end))),
            None => Err(format!("outside the window of {k}: nothing to do")),
        }
    })
    .await
    .map_err(Error::internal)?;
    match decided {
        Ok((kind, until)) => {
            let s3 = s.clone();
            let k2 = key.to_string();
            tokio::task::spawn_blocking(move || s3.start(&k2, kind, until, &[], &w, true))
                .await
                .map_err(Error::internal)?
        }
        Err(msg) => Ok(s.prot.ops.tasks.spawn("storage.dedup", who, move |ctx| async move {
            ctx.log(&msg);
            Ok(())
        })?),
    }
}

// ------------------------------------------------------------------------------------------
// HTTP
// ------------------------------------------------------------------------------------------

pub type DedupState = Option<Arc<DedupSvc>>;

fn svc(st: &DedupState) -> Res<Arc<DedupSvc>> {
    st.clone().ok_or_else(|| {
        Error::new(StatusCode::SERVICE_UNAVAILABLE, error_codes::BACKEND_UNAVAILABLE, "deduplication is not enabled in this rynasd".into())
    })
}

async fn blocking<T: Send + 'static>(s: &Arc<DedupSvc>, f: impl FnOnce(&Arc<DedupSvc>) -> Res<T> + Send + 'static) -> Res<T> {
    let s = s.clone();
    tokio::task::spawn_blocking(move || f(&s)).await.map_err(Error::internal)?
}

fn admin(s: &DedupSvc, h: &HeaderMap, what: &str) -> Res<String> {
    let who = user(h)?;
    s.prot.ops.admins.require_admin(&who, what)?;
    Ok(who)
}

fn check_key(k: &str) -> Res<()> {
    if job_key_valid(k) { Ok(()) } else { Err(invalid(format!("invalid deduplication job {k:?}"))) }
}

pub fn router(st: DedupState) -> Router {
    Router::new()
        .route("/v1/storage/dedup", get(overview_get))
        .route("/v1/storage/dedup/install", post(install_post))
        .route("/v1/storage/dedup/jobs/{key}", put(job_put).delete(job_delete))
        .route("/v1/storage/dedup/jobs/{key}/estimate", post(estimate_post))
        .route("/v1/storage/dedup/jobs/{key}/run", post(run_post))
        .route("/v1/storage/dedup/jobs/{key}/stop", post(stop_post))
        .route("/v1/storage/dedup/jobs/{key}/history", get(history_get))
        .route("/v1/storage/dedup/jobs/{key}/reset", post(reset_post))
        .with_state(st)
}

async fn overview_get(State(st): State<DedupState>, h: HeaderMap) -> Res<Json<DedupOverview>> {
    let s = svc(&st)?;
    let who = admin(&s, &h, "deduplication")?;
    Ok(Json(blocking(&s, move |s| s.overview(&who)).await?))
}

fn save(p: &Protection, owner: &str, d: &DedupConfig, sched: &SchedulerConfig) -> Res<()> {
    let c = &p.ops.config;
    stage(c, owner, DEDUP_KEY, d)?;
    stage(c, owner, SCHEDULER_KEY, sched)?;
    // the settings and their task go live together
    c.stage_group(owner, &[DEDUP_KEY, SCHEDULER_KEY], false)?;
    Ok(())
}

async fn job_put(
    State(st): State<DedupState>,
    h: HeaderMap,
    Path(key): Path<String>,
    body: Result<Json<VolumeDedup>, JsonRejection>,
) -> Res<Json<DedupJobView>> {
    let s = svc(&st)?;
    let who = admin(&s, &h, "deduplication")?;
    check_key(&key)?;
    let v = json_body(body)?;
    rynas_dedup::config::check_job(&key, &v).map_err(invalid)?;
    let (o, k) = (who.clone(), key.clone());
    let view = blocking(&s, move |s| {
        let c = &s.prot.ops.config;
        // the job must be a filesystem rynas sees (or one already set up)
        let (mut d, _, _): (DedupConfig, DedupConfig, _) = docs(c, &o, DEDUP_KEY)?;
        if !d.jobs.contains_key(&k) {
            let known = s.overview(&o)?.jobs.iter().any(|j| j.key == k && !j.mountpoint.is_empty());
            if !known {
                return Err(not_found(format!("filesystem {k}")));
            }
        }
        let (mut sched, _, _): (SchedulerConfig, SchedulerConfig, _) = docs(c, &o, SCHEDULER_KEY)?;
        let task = check_task(&s.prot, &o, job_task(&k, &v), None)?;
        if sched.tasks.iter().any(|t| t.id == task.id && !matches!(&t.action, TaskAction::Dedup { volume } if *volume == k)) {
            return Err(conflict(error_codes::ALREADY_EXISTS, format!("scheduler task {} already exists", task.id)));
        }
        sched.tasks.retain(|t| t.id != task.id);
        sched.tasks.push(task);
        d.jobs.insert(k.clone(), v);
        let d = rynas_dedup::normalize(d).map_err(invalid)?;
        save(&s.prot, &o, &d, &sched)?;
        s.job_view(&o, &k)
    })
    .await?;
    audit(&who, &format!("settings of {key} staged"));
    Ok(Json(view))
}

async fn job_delete(State(st): State<DedupState>, h: HeaderMap, Path(key): Path<String>) -> Res<Json<StagedChanges>> {
    let s = svc(&st)?;
    let who = admin(&s, &h, "deduplication")?;
    check_key(&key)?;
    let (o, k) = (who.clone(), key.clone());
    blocking(&s, move |s| {
        let c = &s.prot.ops.config;
        let (mut d, _, _): (DedupConfig, DedupConfig, _) = docs(c, &o, DEDUP_KEY)?;
        if d.jobs.remove(&k).is_none() {
            return Err(not_found(format!("deduplication job {k}")));
        }
        let (mut sched, _, _): (SchedulerConfig, SchedulerConfig, _) = docs(c, &o, SCHEDULER_KEY)?;
        sched.tasks.retain(|t| !matches!(&t.action, TaskAction::Dedup { volume } if *volume == k));
        save(&s.prot, &o, &d, &sched)
    })
    .await?;
    audit(&who, &format!("job {key} removed (staged)"));
    Ok(Json(StagedChanges { staged: 2 }))
}

/// The end of a run asked for by hand: the given time (at most 24 h ahead), or the end of
/// the current or next window block, or one hour without a window.
fn manual_until(s: &DedupSvc, key: &str, asked: Option<u64>) -> Res<u64> {
    let now = now_ms();
    if let Some(u) = asked {
        if u < now + 60_000 || u > now + MAX_MANUAL {
            return Err(invalid("the end is one minute to 24 hours from now"));
        }
        return Ok(u);
    }
    let cfg: DedupConfig = s.applied(DEDUP_KEY)?;
    let w = cfg.jobs.get(key).map(|j| j.window.clone()).unwrap_or_else(default_window);
    Ok(window::default_until(&w, &Zoned::now()).map(|z| window::ms(&z)).unwrap_or(now + 3_600_000).min(now + MAX_MANUAL))
}

async fn start_post(st: DedupState, h: HeaderMap, key: String, body: Result<Json<DedupRunRequest>, JsonRejection>, kind: DedupKind) -> Res<Response> {
    let s = svc(&st)?;
    let who = admin(&s, &h, "deduplication")?;
    check_key(&key)?;
    let req = json_body(body).unwrap_or_default();
    let w = who.clone();
    let k = key.clone();
    let id = blocking(&s, move |s| {
        let until = manual_until(s, &k, req.until_ms)?;
        s.start(&k, kind, until, &req.shares, &w, false)
    })
    .await?;
    audit(&who, &format!("{kind:?} of {key} started"));
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

async fn estimate_post(
    State(st): State<DedupState>,
    h: HeaderMap,
    Path(key): Path<String>,
    body: Result<Json<DedupRunRequest>, JsonRejection>,
) -> Res<Response> {
    start_post(st, h, key, body, DedupKind::Estimate).await
}

async fn run_post(
    State(st): State<DedupState>,
    h: HeaderMap,
    Path(key): Path<String>,
    body: Result<Json<DedupRunRequest>, JsonRejection>,
) -> Res<Response> {
    start_post(st, h, key, body, DedupKind::Run).await
}

async fn stop_post(State(st): State<DedupState>, h: HeaderMap, Path(key): Path<String>) -> Res<StatusCode> {
    let s = svc(&st)?;
    let who = admin(&s, &h, "deduplication")?;
    check_key(&key)?;
    if !s.stop(&key) {
        return Err(conflict(error_codes::CONFLICT, format!("deduplication of {key} is not running")));
    }
    audit(&who, &format!("stop of {key}"));
    Ok(StatusCode::NO_CONTENT)
}

async fn history_get(State(st): State<DedupState>, h: HeaderMap, Path(key): Path<String>) -> Res<Json<Vec<DedupRecord>>> {
    let s = svc(&st)?;
    admin(&s, &h, "deduplication")?;
    check_key(&key)?;
    Ok(Json(blocking(&s, move |s| Ok(state::read(&s.dir, &key).history)).await?))
}

async fn reset_post(
    State(st): State<DedupState>,
    h: HeaderMap,
    Path(key): Path<String>,
    body: Result<Json<DedupResetRequest>, JsonRejection>,
) -> Res<Response> {
    let s = svc(&st)?;
    let who = admin(&s, &h, "deduplication")?;
    check_key(&key)?;
    let req = json_body(body)?;
    if req.confirm != key {
        return Err(invalid(format!("type {key} to confirm")));
    }
    let k = key.clone();
    let (hashfile, fs_key) = blocking(&s, move |s| {
        let p = s.plan(&k, DedupKind::Estimate, &[])?;
        Ok((p.hashfile, p.fs_key))
    })
    .await?;
    if s.running(&key) {
        return Err(conflict(error_codes::CONFLICT, format!("deduplication of {key} is running: stop it first")));
    }
    audit(&who, &format!("hashfile of {key} reset"));
    let s2 = s.clone();
    let id = s.prot.ops.tasks.spawn("storage.dedup-reset", &who, move |ctx| async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        let _g = s2.locks.dedup(&fs_key, deadline, &|_| {}).await.map_err(|h| format!("{h} holds the filesystem"))?;
        ctx.log(&format!("removing {hashfile}"));
        s2.host.remove_hashfile(&hashfile)?;
        s2.record(&key, |st| {
            st.report = None;
            st.estimate = None;
            st.roots.clear();
            st.block_size = None;
            st.failures = 0;
        });
        Ok(())
    })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

async fn install_post(State(st): State<DedupState>, h: HeaderMap) -> Res<Response> {
    let s = svc(&st)?;
    let who = admin(&s, &h, "deduplication engine install")?;
    let s2 = s.clone();
    let (info, source) = tokio::task::spawn_blocking(move || (s2.engine_info(true), s2.host.package_source()))
        .await
        .map_err(Error::internal)?;
    if info.installed {
        return Err(conflict(error_codes::ALREADY_EXISTS, "the deduplication engine is already installed"));
    }
    let Some(source) = source else {
        return Err(conflict(
            error_codes::CONFLICT,
            format!("{ENGINE_PACKAGE} is in no apt source and not in {}", host::LOCAL_PACKAGES),
        ));
    };
    audit(&who, &format!("install of {ENGINE_PACKAGE} from {source}"));
    let s3 = s.clone();
    let id = s.prot.ops.tasks.spawn("dedup.install", &who, move |ctx| async move {
        if s3.host.simulated() {
            ctx.log(&format!("dry system, not run: apt-get install {ENGINE_PACKAGE}"));
            tokio::time::sleep(Duration::from_millis(600)).await;
            s3.host.mark_installed();
        } else {
            let target = if source == "apt" { ENGINE_PACKAGE.to_string() } else { source };
            ctx.log(&format!("installing {target}"));
            let cmd = crate::updates::apt(&["install", "--", &target], Duration::from_secs(1800));
            ctx.run(&cmd).await.map_err(|e| e.to_string())?;
        }
        let s4 = s3.clone();
        let info = tokio::task::spawn_blocking(move || s4.engine_info(true)).await.map_err(|e| e.to_string())?;
        if !info.installed {
            return Err(format!("{ENGINE_PACKAGE} is still missing after the install"));
        }
        Ok(())
    })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}
