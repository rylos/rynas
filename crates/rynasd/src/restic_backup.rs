//! Backups to another NAS, a disk or the cloud (D-BACKUP): `/v1/backup/restic/…`, public
//! under `/api/v1/backup/restic/…`. The engine is `rynas-backup-worker` (rustic_core, restic
//! repository format); this module holds the routes, the secrets and the run sequence.
//!
//! - Jobs are configuration (`backup` key, [`rynas_backup::BackupDomain`]): creating one
//!   stages the job and its scheduler task `backup-<job>` together. Only applied jobs run.
//! - Secrets (repository password, REST/S3/B2 secret, rclone configuration, SFTP private
//!   key) are sealed with rynasd's age key (`<state>/notify/secret.key`, as the UPS network
//!   passwords) into the job; the API never returns them. A generated repository password is
//!   answered once, by the create.
//! - A run (`backup.run` task, from the timer or "Back up now"): create the repository when
//!   missing → stop the chosen Docker stacks → a read-only btrfs snapshot of each source
//!   that is a subvolume (`<parent>/.rynas-backup/<job>-<n>`) → start the stacks → one
//!   restic snapshot per source, recorded at the source's real path → retention (forget) →
//!   prune and check when due → repository size → the temporary snapshots deleted.
//! - One worker operation per repository at a time ([`repo_key`]: the maintenance lock of
//!   D-BACKUP, never two prunes on one repository, also from two jobs), at most
//!   [`MAX_WORKERS`] task workers on the NAS; reads (snapshots, browse) may run alongside,
//!   as restic allows. Snapshots are scoped to the job's tag and this NAS' host.
//! - Each run writes what it changes on the system (stopped containers, temporary
//!   snapshots, its unit) into the job status first; [`recover`] puts it back at rynasd's
//!   start. Restores write only below a volume or a mounted disk; in place through a
//!   staging folder, confirmed.

use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use rynas_api_types::backup::*;
use rynas_api_types::scheduler::{
    NotifyPolicy, ScheduledTask, SchedulerConfig, TaskAction, TaskTrigger,
};
use rynas_api_types::{SharesConfig, StagedChanges, TaskStarted, error_codes};
use rynas_backup::repo::generate_password;
use rynas_backup::{
    JobSecrets, Launch, Limits, job_tag, normalize_job, repo_key, repo_spec, run_worker, status, task_id_of,
};
use rynas_scheduler::SCHEDULER_KEY;
use rynas_scheduler::units::timer_name;
use rynas_services::SHARES_KEY;
use rynas_tasks::{CancellationToken, TaskCtx};
use serde::Deserialize;

use crate::error::created;
use crate::ops::{Error, Res, user};
use crate::protection::{Protection, blocking, conflict, invalid, not_found};
use crate::scheduler::check_task;
use crate::services::{docs, from_value, pending_op, stage};

/// Reads (test, snapshots, browse, preview) give up after this long.
const READ_TIMEOUT: Duration = Duration::from_secs(110);
/// A download is read whole by the web server: larger files are restored to a folder.
pub const MAX_DOWNLOAD: u64 = 64 << 20;
/// Generated SSH keys wait this long for their job.
const KEY_TTL: Duration = Duration::from_secs(3600);

/// Worker operations of tasks (backup, check, restore) running at once on this NAS; more
/// wait (each worker may take its `MemoryMax`).
const MAX_WORKERS: usize = 2;
/// A task's worker that sends no event for this long is stopped (stacks restarted,
/// snapshots dropped as on any failure).
const STALL: Duration = Duration::from_secs(30 * 60);

/// A repository in use by one worker operation (the maintenance lock, per repository).
#[derive(Clone)]
struct Slot {
    job: String,
    /// UPID of the task (empty while it starts).
    task: String,
    /// The reservation's own id: only its owner frees it.
    token: u64,
}

/// The backup service.
pub struct ResticSvc {
    pub prot: Arc<Protection>,
    pub launch: Launch,
    /// `--dry-system`: no btrfs snapshots, no Docker stops.
    pub dry: bool,
    /// `<state>/notify`: rynasd's age key.
    pub secret_dir: PathBuf,
    /// The jobs' persistent rustic caches (`/var/cache/rynas/backup/<job>`).
    pub cache_root: PathBuf,
    /// repository key ([`repo_key`]) → the operation holding it.
    running: Mutex<HashMap<String, Slot>>,
    /// public key line → (private key, created).
    keys: Mutex<HashMap<String, (String, Instant)>>,
    /// `host:port` → (host key lines seen by a test, when): pinned by the job's save.
    host_keys: Mutex<HashMap<String, (String, Instant)>>,
    /// Worker operations of tasks at once ([`MAX_WORKERS`]).
    workers: Arc<tokio::sync::Semaphore>,
}

pub type RsState = Option<Arc<ResticSvc>>;

/// The service, for the scheduler's internal action (`rynasctl job run backup-<job>`).
pub static SVC: OnceLock<Arc<ResticSvc>> = OnceLock::new();

static TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl ResticSvc {
    pub fn new(prot: Arc<Protection>, launch: Launch, dry: bool, secret_dir: PathBuf) -> Arc<Self> {
        let cache_root = if dry {
            prot.state_dir.join("backup-cache")
        } else {
            PathBuf::from("/var/cache/rynas/backup")
        };
        Arc::new(ResticSvc {
            prot,
            launch,
            dry,
            secret_dir,
            cache_root,
            running: Mutex::new(HashMap::new()),
            keys: Mutex::new(HashMap::new()),
            host_keys: Mutex::new(HashMap::new()),
            workers: Arc::new(tokio::sync::Semaphore::new(MAX_WORKERS)),
        })
    }

    /// The task running for `job`, if any.
    fn running_task(&self, job: &str) -> Option<String> {
        let r = self.running.lock().unwrap_or_else(|p| p.into_inner());
        r.values().find(|s| s.job == job).map(|s| s.task.clone())
    }

    /// The job's cache folder, made 0700 when missing.
    fn cache_dir(&self, job: &str) -> Option<String> {
        use std::os::unix::fs::DirBuilderExt;
        if !rynas_backup::valid_job_name(job) {
            return None;
        }
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.cache_root);
        let d = self.cache_root.join(job);
        let _ = std::fs::DirBuilder::new().mode(0o700).create(&d);
        d.is_dir().then(|| d.display().to_string())
    }

    fn request(&self, job: &str, repo: RepoSpec, op: WorkerOp) -> WorkerRequest {
        WorkerRequest {
            repo,
            op,
            cache_dir: self.cache_dir(job),
        }
    }
}

fn svc(st: &RsState) -> Res<Arc<ResticSvc>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "backups are not enabled in this rynasd".into(),
        )
    })
}

pub fn router(st: RsState) -> Router {
    Router::new()
        .route("/v1/backup/restic/jobs", get(jobs).post(job_create))
        .route(
            "/v1/backup/restic/jobs/{name}",
            get(job_get).put(job_put).delete(job_delete),
        )
        .route("/v1/backup/restic/test", post(test))
        .route("/v1/backup/restic/ssh-key", post(ssh_key))
        .route("/v1/backup/restic/jobs/{name}/run", post(run_now))
        .route("/v1/backup/restic/jobs/{name}/check", post(check_now))
        .route("/v1/backup/restic/jobs/{name}/snapshots", get(snapshots))
        .route("/v1/backup/restic/jobs/{name}/browse", get(browse))
        .route("/v1/backup/restic/jobs/{name}/restore", post(restore))
        .route("/v1/backup/restic/jobs/{name}/download", get(download))
        .route(
            "/v1/backup/restic/jobs/{name}/preview-retention",
            get(preview_retention),
        )
        .with_state(st)
}

fn json_body<T>(b: Result<Json<T>, JsonRejection>) -> Res<T> {
    b.map(|Json(v)| v).map_err(Error::from)
}

fn check_name(n: &str) -> Res<()> {
    if rynas_backup::valid_job_name(n) {
        Ok(())
    } else {
        Err(invalid(format!("invalid job name {n:?}")))
    }
}

fn audit(who: &str, what: &str) {
    tracing::info!(target: "rynas::audit", "backup {what} by {who}");
}

// ---- secrets ----

fn seal(s: &ResticSvc, sec: &JobSecrets) -> Res<String> {
    let sb = crate::ups_netserver::secret_box(&s.secret_dir).map_err(Error::internal)?;
    let sealed = sb.seal(&sec.to_map()).map_err(Error::internal)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(sealed))
}

fn unseal(s: &ResticSvc, job: &BackupJob) -> Res<JobSecrets> {
    let Some(b) = job.sealed.as_deref() else {
        return Ok(JobSecrets::default());
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b)
        .map_err(|_| Error::internal("sealed secrets: not base64"))?;
    let sb = crate::ups_netserver::secret_box(&s.secret_dir).map_err(Error::internal)?;
    let m = sb
        .open_sealed(&bytes)
        .map_err(|_| Error::internal("sealed secrets cannot be opened with this NAS' key"))?;
    Ok(JobSecrets::from_map(&m))
}

/// The private key generated for `public` (by [`ssh_key`]), if still waiting.
fn take_key(s: &ResticSvc, public: &str) -> Option<String> {
    let mut k = s.keys.lock().unwrap_or_else(|p| p.into_inner());
    k.retain(|_, (_, at)| at.elapsed() < KEY_TTL);
    k.remove(public.trim()).map(|(p, _)| p)
}

/// Secrets a worker changed after the job was saved (rclone's refreshed token, a host key
/// learned on first use): `<state>/backup/<job>.secrets`, sealed, applied on top of the
/// job's own only while the job still holds the value they replaced (a new value saved by
/// the user wins). The configuration is never changed behind the user's back.
fn overlay_path(s: &ResticSvc, job: &str) -> Option<PathBuf> {
    rynas_backup::valid_job_name(job).then(|| status::dir(&s.prot.state_dir).join(format!("{job}.secrets")))
}

fn read_overlay(s: &ResticSvc, job: &str) -> std::collections::BTreeMap<String, String> {
    let Some(p) = overlay_path(s, job) else {
        return Default::default();
    };
    let Ok(bytes) = std::fs::read(p) else {
        return Default::default();
    };
    crate::ups_netserver::secret_box(&s.secret_dir)
        .ok()
        .and_then(|sb| sb.open_sealed(&bytes).ok())
        .unwrap_or_default()
}

/// `sec` (the job's own secrets) with the overlay's newer values.
fn apply_overlay(mut sec: JobSecrets, ov: &std::collections::BTreeMap<String, String>) -> JobSecrets {
    for (field, slot) in [
        ("rclone_config", &mut sec.rclone_config),
        ("ssh_known_hosts", &mut sec.ssh_known_hosts),
    ] {
        if let (Some(new), Some(base)) = (ov.get(field), ov.get(&format!("{field}.base")))
            && base == slot
        {
            *slot = new.clone();
        }
    }
    sec
}

/// The job's secrets as a worker must use them (overlay applied).
fn unseal_job(s: &ResticSvc, job: &BackupJob) -> Res<JobSecrets> {
    Ok(apply_overlay(unseal(s, job)?, &read_overlay(s, &job.name)))
}

/// Seals the secret files a worker reported changed into the job's overlay.
fn store_changed(s: &ResticSvc, job: &BackupJob, changed: &[(String, String)], ctx: Option<&TaskCtx>) {
    if changed.is_empty() {
        return;
    }
    let Ok(own) = unseal(s, job) else { return };
    let own = own.to_map();
    let mut ov = read_overlay(s, &job.name);
    for (name, content) in changed {
        let Some(field) = JobSecrets::field_of_file(name) else { continue };
        ov.insert(field.to_string(), content.clone());
        ov.insert(format!("{field}.base"), own.get(field).cloned().unwrap_or_default());
        if let Some(c) = ctx {
            c.log(match field {
                "ssh_known_hosts" => "the SFTP server's host key was trusted on first use and is pinned now",
                _ => "the rclone configuration was refreshed by rclone (new token kept)",
            });
        }
    }
    let r = crate::ups_netserver::secret_box(&s.secret_dir)
        .map_err(|e| e.to_string())
        .and_then(|sb| sb.seal(&ov).map_err(|e| e.to_string()))
        .and_then(|bytes| {
            let p = overlay_path(s, &job.name).ok_or("invalid job name")?;
            write_private_file(&p, &bytes).map_err(|e| e.to_string())
        });
    if let Err(e) = r {
        tracing::warn!("backup {}: changed secrets not kept: {e}", job.name);
    }
}

/// `bytes` into `p` (0600, temporary file + rename).
fn write_private_file(p: &FsPath, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(tmp, p)
}

/// The host key lines a destination test saw for `host:port`, if still waiting.
fn take_host_key(s: &ResticSvc, host: &str, port: u16) -> Option<String> {
    let mut k = s.host_keys.lock().unwrap_or_else(|p| p.into_inner());
    k.retain(|_, (_, at)| at.elapsed() < KEY_TTL);
    k.remove(&format!("{host}:{port}")).map(|(v, _)| v)
}

/// The fingerprints of `known_hosts` lines (`ssh-keygen -l`), for the test's message.
fn fingerprints(lines: &str) -> String {
    let Ok(dir) = tempfile::tempdir() else {
        return String::new();
    };
    let p = dir.path().join("kh");
    if std::fs::write(&p, lines).is_err() {
        return String::new();
    }
    std::process::Command::new("ssh-keygen")
        .arg("-l")
        .arg("-f")
        .arg(&p)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| {
                    // "256 SHA256:… host (ED25519)"
                    let mut f = l.split_whitespace();
                    let (_, fp) = (f.next()?, f.next()?);
                    let kind = l.rsplit_once('(').map(|(_, k)| k.trim_end_matches(')'))?;
                    Some(format!("{kind} {fp}"))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// Removes what rynasd keeps for jobs that are not applied (only applied jobs run, so only
/// they have any): the status, the changed secrets and the cache. At start and with each
/// job list, so a deleted job's files go once its removal is applied.
fn sweep(s: &ResticSvc) {
    let Ok(Some(v)) = s.prot.ops.config.applied(BACKUP_KEY) else {
        // nothing applied yet, or unreadable: keep everything
        return;
    };
    let Ok(b) = serde_json::from_value::<BackupConfig>(v) else { return };
    let live: std::collections::HashSet<String> = b.jobs.into_iter().map(|j| j.name).collect();
    let dir = status::dir(&s.prot.state_dir);
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let Some((job, ext)) = n.rsplit_once('.') else { continue };
        if matches!(ext, "json" | "secrets") && rynas_backup::valid_job_name(job) && !live.contains(job) {
            if ext == "json" {
                status::remove(&s.prot.state_dir, job);
            } else {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    for e in std::fs::read_dir(&s.cache_root).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if rynas_backup::valid_job_name(&n) && !live.contains(&n) && e.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn merge_secrets(
    s: &ResticSvc,
    job: &BackupJob,
    prev: JobSecrets,
    req: &BackupJobRequest,
    creating: bool,
) -> Res<(JobSecrets, Option<String>)> {
    let mut sec = prev;
    let mut generated = None;
    if !req.repository_password.is_empty() {
        if req.repository_password.len() < 8 {
            return Err(invalid("the repository password needs at least 8 characters"));
        }
        // the password is the repository's key: a saved job keeps it (a new one would not
        // open the repository its snapshots are in)
        if !creating && !sec.repository_password.is_empty() && sec.repository_password != req.repository_password {
            return Err(invalid(
                "the repository password of a saved job cannot change: it is the key of the repository's snapshots (create a new job for another repository)",
            ));
        }
        sec.repository_password = req.repository_password.clone();
    } else if creating || sec.repository_password.is_empty() {
        let pw = generate_password().map_err(Error::internal)?;
        sec.repository_password = pw.clone();
        generated = Some(pw);
    }
    if !req.secrets.password.is_empty() {
        sec.password = req.secrets.password.clone();
    }
    if !req.secrets.rclone_config.is_empty() {
        sec.rclone_config = req.secrets.rclone_config.clone();
    }
    match &job.destination {
        Destination::Sftp { host, port, .. } => {
            if let Some(k) = take_key(s, &job.ssh_public_key) {
                sec.ssh_private_key = k;
            }
            if sec.ssh_private_key.is_empty() {
                return Err(invalid("SFTP: generate a key pair first"));
            }
            // the host key the last test of this server saw is pinned (the user compared
            // its fingerprint in the test's message)
            if let Some(k) = take_host_key(s, host, *port) {
                sec.ssh_known_hosts = k;
            }
        }
        _ => {
            sec.ssh_private_key.clear();
            sec.ssh_known_hosts.clear();
        }
    }
    let needs_password = match &job.destination {
        Destination::S3 { .. } | Destination::B2 { .. } => true,
        Destination::Rest { user, .. } => !user.is_empty(),
        _ => false,
    };
    if needs_password && sec.password.is_empty() {
        return Err(invalid("the destination's secret key or password is required"));
    }
    if !matches!(job.destination, Destination::S3 { .. } | Destination::B2 { .. } | Destination::Rest { .. }) {
        sec.password.clear();
    }
    if !matches!(job.destination, Destination::Rclone { .. }) {
        sec.rclone_config.clear();
    }
    Ok((sec, generated))
}

/// What a changed job keeps of its stored secrets (review L3): the repository password
/// always; the destination's secret and rclone configuration only for the same destination;
/// the SFTP key pair while the public key stays, its pinned host key only for the same
/// server and port.
fn kept_secrets(old: &BackupJob, new: &BackupJob, prev: JobSecrets) -> JobSecrets {
    let same = old.destination == new.destination;
    let mut keep = JobSecrets {
        repository_password: prev.repository_password,
        ..Default::default()
    };
    if same {
        keep.password = prev.password;
        keep.rclone_config = prev.rclone_config;
    }
    if let (Destination::Sftp { host: h1, port: p1, .. }, Destination::Sftp { host: h2, port: p2, .. }) =
        (&old.destination, &new.destination)
    {
        if old.ssh_public_key == new.ssh_public_key {
            keep.ssh_private_key = prev.ssh_private_key;
        }
        if h1.eq_ignore_ascii_case(h2) && p1 == p2 {
            keep.ssh_known_hosts = prev.ssh_known_hosts;
        }
    }
    keep
}

// ---- views ----

fn strip(mut j: BackupJob) -> BackupJob {
    j.sealed = None;
    j
}

fn job_views(
    s: &ResticSvc,
    owner: &str,
    rt: &crate::protection::Runtime,
) -> Res<Vec<BackupJobView>> {
    let p = &s.prot;
    let c = &p.ops.config;
    let (eff, cur, _): (BackupConfig, BackupConfig, _) = docs(c, owner, BACKUP_KEY)?;
    let (sched, _, applied_sched): (SchedulerConfig, SchedulerConfig, Option<SchedulerConfig>) =
        docs(c, owner, SCHEDULER_KEY)?;
    let latest = p.history.latest().map_err(Error::internal)?;
    let view = |j: &BackupJob, pending| {
        let tid = task_id_of(&j.name);
        let task = sched.tasks.iter().find(|t| t.id == tid);
        let applied = applied_sched
            .as_ref()
            .is_some_and(|a| a.tasks.iter().any(|t| t.id == tid));
        let sec = unseal(s, j).unwrap_or_default();
        BackupJobView {
            schedule: task.map(|t| t.trigger.clone()),
            enabled: task.is_some_and(|t| t.enabled),
            notify: task.map(|t| t.notify),
            next_run_ms: if applied && task.is_some_and(|t| t.enabled) {
                let timer = timer_name(&tid);
                rt.timers
                    .iter()
                    .find(|t| t.unit == timer)
                    .and_then(|t| t.next_ms)
            } else {
                None
            },
            last_run: latest.get(&tid).cloned(),
            running_task: s.running_task(&j.name).filter(|t| !t.is_empty()),
            status: status::read(&p.state_dir, &j.name),
            task_id: tid,
            pending,
            has_destination_secret: !sec.password.is_empty(),
            has_rclone_config: !sec.rclone_config.is_empty(),
            job: strip(j.clone()),
        }
    };
    let mut out: Vec<_> = eff
        .jobs
        .iter()
        .map(|j| view(j, pending_op(Some(j), cur.jobs.iter().find(|x| x.name == j.name))))
        .collect();
    for j in cur.jobs.iter().filter(|j| !eff.jobs.iter().any(|x| x.name == j.name)) {
        out.push(view(j, pending_op(None, Some(j))));
    }
    Ok(out)
}

async fn job_view(s: &Arc<ResticSvc>, owner: String, name: String) -> Res<BackupJobView> {
    let rt = s.prot.runtime().await;
    let s2 = s.clone();
    blocking(&s.prot, move |_| {
        job_views(&s2, &owner, &rt)?
            .into_iter()
            .find(|v| v.job.name == name)
            .ok_or_else(|| not_found(format!("backup job {name}")))
    })
    .await
}

async fn jobs(State(st): State<RsState>, h: HeaderMap) -> Res<Json<Vec<BackupJobView>>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let rt = s.prot.runtime().await;
    let s2 = s.clone();
    Ok(Json(
        blocking(&s.prot, move |_| {
            sweep(&s2);
            job_views(&s2, &owner, &rt)
        })
        .await?,
    ))
}

async fn job_get(
    State(st): State<RsState>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Res<Json<BackupJobView>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    check_name(&name)?;
    Ok(Json(job_view(&s, owner, name).await?))
}

// ---- create, change, delete ----

fn default_schedule() -> TaskTrigger {
    TaskTrigger::Calendar {
        on_calendar: "*-*-* 02:00:00".into(),
        cron: None,
        persistent: true,
        randomized_delay_s: 600,
    }
}

fn job_task(
    p: &Protection,
    owner: &str,
    job: &BackupJob,
    schedule: TaskTrigger,
    enabled: bool,
    notify: NotifyPolicy,
) -> Res<ScheduledTask> {
    check_task(
        p,
        owner,
        ScheduledTask {
            id: task_id_of(&job.name),
            name: format!("Backup {}", job.name),
            description: String::new(),
            enabled,
            action: TaskAction::Backup {
                job: job.name.clone(),
            },
            trigger: schedule,
            timeout_s: None,
            nice: None,
            io_weight: None,
            notify,
            notify_output: false,
        },
        None,
    )
}

/// Shares named by the job must exist (in the session user's view).
fn check_sources(p: &Protection, owner: &str, job: &BackupJob) -> Res<()> {
    let (shares, _, _): (SharesConfig, SharesConfig, _) = docs(&p.ops.config, owner, SHARES_KEY)?;
    for src in &job.sources {
        if let BackupSource::Share { name } = src
            && !shares.shares.iter().any(|s| &s.name == name)
        {
            return Err(invalid(format!("there is no share {name}")));
        }
    }
    Ok(())
}

fn save(p: &Protection, owner: &str, b: &BackupConfig, sched: &SchedulerConfig) -> Res<()> {
    let c = &p.ops.config;
    stage(c, owner, BACKUP_KEY, b)?;
    stage(c, owner, SCHEDULER_KEY, sched)?;
    // a job and its task go live together
    c.stage_group(owner, &[BACKUP_KEY, SCHEDULER_KEY], false)?;
    Ok(())
}

async fn job_create(
    State(st): State<RsState>,
    h: HeaderMap,
    body: Result<Json<BackupJobRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let req = json_body(body)?;
    let (o, s2) = (owner.clone(), s.clone());
    let (name, generated) = blocking(&s.prot, move |p| {
        let c = &p.ops.config;
        let (mut b, _, _): (BackupConfig, BackupConfig, _) = docs(c, &o, BACKUP_KEY)?;
        let (mut sched, _, _): (SchedulerConfig, SchedulerConfig, _) = docs(c, &o, SCHEDULER_KEY)?;
        let mut job = req.job.clone();
        // shape first (without secrets), then the secrets
        job.sealed = Some("-".into());
        let mut job = normalize_job(job).map_err(invalid)?;
        if b.jobs.iter().any(|j| j.name == job.name) {
            return Err(conflict(
                error_codes::ALREADY_EXISTS,
                format!("backup job {} already exists", job.name),
            ));
        }
        let tid = task_id_of(&job.name);
        if sched.tasks.iter().any(|t| t.id == tid) {
            return Err(conflict(
                error_codes::ALREADY_EXISTS,
                format!("scheduler task {tid} already exists"),
            ));
        }
        check_sources(p, &o, &job)?;
        let (sec, generated) = merge_secrets(&s2, &job, JobSecrets::default(), &req, true)?;
        job.sealed = Some(seal(&s2, &sec)?);
        let task = job_task(
            p,
            &o,
            &job,
            req.schedule.clone().unwrap_or_else(default_schedule),
            req.enabled.unwrap_or(true),
            req.notify.unwrap_or(NotifyPolicy::OnFailure),
        )?;
        b.jobs.push(job.clone());
        sched.tasks.push(task);
        save(p, &o, &b, &sched)?;
        Ok((job.name, generated))
    })
    .await?;
    audit(&owner, &format!("job {name} created"));
    let view = job_view(&s, owner, name.clone()).await?;
    Ok(created(
        "/api/v1/backup/restic/jobs",
        &name,
        BackupJobCreated {
            view,
            repository_password: generated,
        },
    ))
}

async fn job_put(
    State(st): State<RsState>,
    h: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<BackupJobRequest>, JsonRejection>,
) -> Res<Json<BackupJobView>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    check_name(&name)?;
    let mut req = json_body(body)?;
    if req.job.name.is_empty() {
        req.job.name = name.clone();
    }
    if req.job.name != name {
        return Err(invalid("the job name cannot change"));
    }
    let (o, n, s2) = (owner.clone(), name.clone(), s.clone());
    blocking(&s.prot, move |p| {
        let c = &p.ops.config;
        let (mut b, _, _): (BackupConfig, BackupConfig, _) = docs(c, &o, BACKUP_KEY)?;
        let (mut sched, _, _): (SchedulerConfig, SchedulerConfig, _) = docs(c, &o, SCHEDULER_KEY)?;
        let old = b
            .jobs
            .iter()
            .find(|j| j.name == n)
            .cloned()
            .ok_or_else(|| not_found(format!("backup job {n}")))?;
        let mut job = req.job.clone();
        job.sealed = Some("-".into());
        let mut job = normalize_job(job).map_err(invalid)?;
        check_sources(p, &o, &job)?;
        // with the secrets workers changed since (a refreshed rclone token)
        let prev = unseal_job(&s2, &old)?;
        let prev = kept_secrets(&old, &job, prev);
        let (sec, _) = merge_secrets(&s2, &job, prev, &req, false)?;
        job.sealed = Some(seal(&s2, &sec)?);
        let tid = task_id_of(&n);
        let cur = sched.tasks.iter().find(|t| t.id == tid).cloned();
        let task = job_task(
            p,
            &o,
            &job,
            req.schedule
                .clone()
                .or_else(|| cur.as_ref().map(|t| t.trigger.clone()))
                .unwrap_or_else(default_schedule),
            req.enabled.or(cur.as_ref().map(|t| t.enabled)).unwrap_or(true),
            req.notify
                .or(cur.as_ref().map(|t| t.notify))
                .unwrap_or(NotifyPolicy::OnFailure),
        )?;
        for j in &mut b.jobs {
            if j.name == n {
                *j = job.clone();
            }
        }
        sched.tasks.retain(|t| t.id != tid);
        sched.tasks.push(task);
        save(p, &o, &b, &sched)
    })
    .await?;
    audit(&owner, &format!("job {name} changed"));
    Ok(Json(job_view(&s, owner, name).await?))
}

async fn job_delete(
    State(st): State<RsState>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Res<Json<StagedChanges>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    check_name(&name)?;
    let (o, n) = (owner.clone(), name.clone());
    blocking(&s.prot, move |p| {
        let c = &p.ops.config;
        let (mut b, _, _): (BackupConfig, BackupConfig, _) = docs(c, &o, BACKUP_KEY)?;
        let (mut sched, _, _): (SchedulerConfig, SchedulerConfig, _) = docs(c, &o, SCHEDULER_KEY)?;
        if !b.jobs.iter().any(|j| j.name == n) {
            return Err(not_found(format!("backup job {n}")));
        }
        b.jobs.retain(|j| j.name != n);
        let tid = task_id_of(&n);
        sched.tasks.retain(|t| t.id != tid);
        save(p, &o, &b, &sched)
    })
    .await?;
    audit(&owner, &format!("job {name} removed (the repository stays)"));
    Ok(Json(StagedChanges { staged: 2 }))
}

// ---- test, keys ----

async fn test(
    State(st): State<RsState>,
    h: HeaderMap,
    body: Result<Json<TestRequest>, JsonRejection>,
) -> Res<Json<TestResult>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let req = json_body(body)?;
    let (o, s2) = (owner.clone(), s.clone());
    let (spec, dest) = blocking(&s.prot, move |_| {
        // the destination checked like a job's
        let shape = BackupJob {
            name: "test".into(),
            description: String::new(),
            sources: vec![BackupSource::Config],
            destination: req.destination.clone(),
            retention: Retention::SMART,
            maintenance: Maintenance::default(),
            excludes: vec![],
            use_snapshot: false,
            stop_stacks: vec![],
            bandwidth_kib: 0,
            sealed: Some("-".into()),
            ssh_public_key: String::new(),
        };
        let dest = normalize_job(shape).map_err(invalid)?.destination;
        let mut sec = JobSecrets::default();
        if let Some(name) = &req.job {
            let (b, _, _): (BackupConfig, BackupConfig, _) = docs(&s2.prot.ops.config, &o, BACKUP_KEY)?;
            // the stored secrets fill the empty fields only for the very same destination
            // (review L3): never sent to another server
            if let Some(j) = b.jobs.iter().find(|j| &j.name == name) {
                let stored = unseal_job(&s2, j)?;
                if j.destination == dest {
                    sec = stored;
                } else if let (Destination::Sftp { .. }, Destination::Sftp { .. }) = (&j.destination, &dest) {
                    // the job's own key pair, to a server the user names
                    sec.ssh_private_key = stored.ssh_private_key;
                }
            }
        }
        if !req.secrets.password.is_empty() && req_public_key(&req).is_none() {
            sec.password = req.secrets.password.clone();
        }
        if !req.secrets.rclone_config.is_empty() {
            sec.rclone_config = req.secrets.rclone_config.clone();
        }
        if !req.repository_password.is_empty() {
            sec.repository_password = req.repository_password.clone();
        }
        if let Destination::Sftp { .. } = dest {
            let k = s2.keys.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(pk) = req_public_key(&req)
                && let Some((private, _)) = k.get(pk)
            {
                sec.ssh_private_key = private.clone();
            }
        }
        if let Destination::Local { path } = &dest
            && !s2.dry
        {
            check_local_destination(path, &mountinfo()).map_err(invalid)?;
        }
        Ok((repo_spec(&dest, &sec, 0), dest))
    })
    .await?;
    let wreq = WorkerRequest {
        repo: spec,
        op: WorkerOp::Test,
        cache_dir: None,
    };
    let limits = Limits {
        timeout: Some(READ_TIMEOUT),
        ..Default::default()
    };
    let r = run_worker(&s.launch, "test", &wreq, &CancellationToken::new(), &limits, |_| {}).await;
    Ok(Json(match r {
        Ok(WorkerEvent::Test {
            outcome,
            snapshots,
            host_key,
        }) => {
            let mut message = String::new();
            if let (Destination::Sftp { host, port, .. }, Some(k)) = (&dest, host_key) {
                let fp = tokio::task::spawn_blocking({
                    let k = k.clone();
                    move || fingerprints(&k)
                })
                .await
                .unwrap_or_default();
                message = format!(
                    "server host key {fp}: compare it with the server's own; saving the job pins it"
                );
                s.host_keys
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(format!("{host}:{port}"), (k, Instant::now()));
            }
            TestResult {
                outcome,
                message,
                snapshots,
            }
        }
        Ok(_) => TestResult {
            outcome: TestOutcome::Failed,
            message: "unexpected answer".into(),
            snapshots: None,
        },
        Err(e) => TestResult {
            outcome: TestOutcome::Failed,
            message: e,
            snapshots: None,
        },
    }))
}

/// `/proc/self/mountinfo`.
fn mountinfo() -> String {
    std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default()
}

/// The mount point holding `path` (the longest mount point that is a prefix of it).
fn mount_point_of(path: &str, mountinfo: &str) -> String {
    rynas_storage::mounts::parse_mountinfo(mountinfo)
        .into_iter()
        .map(|m| m.mountpoint)
        .filter(|mp| mp == "/" || path == mp || path.starts_with(&format!("{mp}/")))
        .max_by_key(String::len)
        .unwrap_or_else(|| "/".into())
}

/// Logged by each run of a job whose local destination is in the system volume.
const SAME_DISK_WARNING: &str = "the destination is on the system disk: the snapshots protect against deleted or encrypted files, not against a failure of that disk; keep another copy elsewhere";

/// A local destination must be on a mounted disk or volume, never elsewhere on the system
/// disk (an unplugged USB disk leaves an empty folder there): review M6. The system volume's
/// folder is a volume chosen on purpose (D-ROOTVOL): allowed, with [`SAME_DISK_WARNING`].
fn check_local_destination(path: &str, mountinfo: &str) -> Result<(), String> {
    if crate::places::in_system_dir(path) && crate::places::root_is_disk(mountinfo) {
        return Ok(());
    }
    if mount_point_of(path, mountinfo) == "/" {
        return Err(format!(
            "{path} is on the system disk: is the backup disk mounted? (a local destination must be on a mounted disk or volume)"
        ));
    }
    Ok(())
}

/// A test of an SFTP destination names its key by the public key, in `secrets.password`
/// (never a secret there: the public half).
fn req_public_key(req: &TestRequest) -> Option<&str> {
    let k = req.secrets.password.trim();
    k.starts_with("ssh-").then_some(k)
}

async fn ssh_key(State(st): State<RsState>, h: HeaderMap) -> Res<Json<SshKeyCreated>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let (private, public) = tokio::task::spawn_blocking(generate_ssh_key)
        .await
        .map_err(Error::internal)?
        .map_err(Error::internal)?;
    let id: String = public
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .chars()
        .rev()
        .take(12)
        .collect();
    s.keys
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(public.clone(), (private, Instant::now()));
    audit(&owner, "SSH key generated");
    Ok(Json(SshKeyCreated {
        public_key: public,
        key_id: id,
    }))
}

/// A new ed25519 key pair with `ssh-keygen` (private OpenSSH text, public line).
fn generate_ssh_key() -> Result<(String, String), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("key");
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let out = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C"])
        .arg(format!("rynas-backup@{}", host.trim()))
        .arg("-f")
        .arg(&path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .map_err(|e| format!("ssh-keygen: {e} (is openssh-client installed?)"))?;
    if !out.status.success() {
        return Err(format!(
            "ssh-keygen: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let private = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let public = std::fs::read_to_string(path.with_extension("pub")).map_err(|e| e.to_string())?;
    Ok((private, public.trim().to_string()))
}

// ---- running ----

/// The applied job and its secrets (only applied jobs run).
fn applied_job(s: &ResticSvc, name: &str) -> Res<(BackupJob, JobSecrets)> {
    let b: BackupConfig = from_value(s.prot.ops.config.applied(BACKUP_KEY)?)?;
    let job = b
        .jobs
        .into_iter()
        .find(|j| j.name == name)
        .ok_or_else(|| not_found(format!("applied backup job {name}")))?;
    let sec = unseal_job(s, &job)?;
    Ok((job, sec))
}

/// The reservation of a repository; dropping it frees the repository if it is still ours.
struct Busy {
    svc: Arc<ResticSvc>,
    key: String,
    token: u64,
}

impl Drop for Busy {
    fn drop(&mut self) {
        let mut r = self.svc.running.lock().unwrap_or_else(|p| p.into_inner());
        release(&mut r, &self.key, self.token);
    }
}

/// Takes `key` for `job` unless another operation holds it (checked and taken under one
/// lock: two requests can never both get it).
fn try_reserve(r: &mut HashMap<String, Slot>, key: &str, job: &str, token: u64) -> Result<(), String> {
    if let Some(slot) = r.get(key) {
        let who = if slot.job == job {
            format!("backup job {job} is busy")
        } else {
            format!("the repository of backup job {job} is in use by job {}", slot.job)
        };
        let task = if slot.task.is_empty() { String::new() } else { format!(" (task {})", slot.task) };
        return Err(format!("{who}{task}"));
    }
    r.insert(
        key.to_string(),
        Slot {
            job: job.to_string(),
            task: String::new(),
            token,
        },
    );
    Ok(())
}

/// Frees `key` only for the reservation that took it.
fn release(r: &mut HashMap<String, Slot>, key: &str, token: u64) {
    if r.get(key).is_some_and(|s| s.token == token) {
        r.remove(key);
    }
}

/// Reserves the repository of `job` (its normalized key: two jobs on one repository share
/// it) for one worker operation, under the lock (review M4, L1).
fn reserve(s: &Arc<ResticSvc>, job: &BackupJob) -> Res<Busy> {
    let key = repo_key(&job.destination);
    let token = TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut r = s.running.lock().unwrap_or_else(|p| p.into_inner());
    try_reserve(&mut r, &key, &job.name, token).map_err(|e| conflict(error_codes::BUSY, e))?;
    Ok(Busy {
        svc: s.clone(),
        key,
        token,
    })
}

/// Starts a task that holds the job's repository for its whole run, and one of the
/// [`MAX_WORKERS`] worker slots while it works.
fn spawn_locked<F, Fut>(s: &Arc<ResticSvc>, kind: &str, who: &str, job: &BackupJob, f: F) -> Res<String>
where
    F: FnOnce(TaskCtx) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let busy = reserve(s, job)?;
    let (key, token) = (busy.key.clone(), busy.token);
    let workers = s.workers.clone();
    // on an error the closure (and the reservation in it) is dropped: the slot is freed
    let id = s.prot.ops.tasks.spawn(kind, who, move |ctx| async move {
        let _busy = busy;
        let _permit = match workers.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                ctx.log("waiting for another backup operation to finish");
                tokio::select! {
                    p = workers.acquire_owned() => p.map_err(|e| e.to_string())?,
                    _ = ctx.token().cancelled() => return Err("cancelled".into()),
                }
            }
        };
        f(ctx).await
    })?;
    let mut r = s.running.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(slot) = r.get_mut(&key)
        && slot.token == token
    {
        slot.task = id.clone();
    }
    Ok(id)
}

/// Worker events → the task's log and progress.
fn follow(ctx: &TaskCtx) -> impl FnMut(&WorkerEvent) + '_ {
    let mut last_step = String::new();
    move |ev| match ev {
        WorkerEvent::Step { title } if *title != last_step => {
            ctx.log(title);
            last_step = title.clone();
        }
        WorkerEvent::Log { text } => ctx.log(text),
        WorkerEvent::Progress { done, total, .. } if *total > 0 => {
            ctx.progress(((done.saturating_mul(100)) / total).min(99) as u8);
        }
        WorkerEvent::BackedUp { snapshot } => ctx.log(&format!(
            "snapshot {} of {}: {} files, {} read, {} new",
            short(&snapshot.id),
            snapshot.paths.join(", "),
            snapshot.files.unwrap_or(0),
            bytes(snapshot.bytes.unwrap_or(0)),
            bytes(snapshot.added_bytes.unwrap_or(0)),
        )),
        _ => {}
    }
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

async fn op(
    s: &ResticSvc,
    job: &BackupJob,
    spec: &RepoSpec,
    op: WorkerOp,
    ctx: &TaskCtx,
) -> Result<WorkerEvent, String> {
    op_in(s, job, spec, op, ctx, None).await
}

/// [`op`] in the unit `unit` (recorded by the caller before the start).
async fn op_in(
    s: &ResticSvc,
    job: &BackupJob,
    spec: &RepoSpec,
    op: WorkerOp,
    ctx: &TaskCtx,
    unit: Option<String>,
) -> Result<WorkerEvent, String> {
    let req = s.request(&job.name, spec.clone(), op);
    let limits = Limits {
        stall: Some(STALL),
        unit,
        ..Default::default()
    };
    let mut changed = Vec::new();
    let mut f = follow(ctx);
    let r = run_worker(&s.launch, &job.name, &req, ctx.token(), &limits, |ev| {
        if let WorkerEvent::SecretChanged { name, content } = ev {
            changed.push((name.clone(), content.clone()));
        }
        f(ev)
    })
    .await;
    store_changed(s, job, &changed, Some(ctx));
    r
}

/// The job's snapshots as this NAS sees them: its tag and host (review M5, M8).
fn scope_of(job: &str) -> SnapshotScope {
    SnapshotScope {
        tag: job_tag(job),
        host: hostname(),
    }
}

pub fn spawn_run(who: &str, job: &str) -> Res<String> {
    let s = SVC
        .get()
        .cloned()
        .ok_or_else(|| Error::internal("backups are not enabled in this rynasd"))?;
    let (j, sec) = applied_job(&s, job)?;
    let s2 = s.clone();
    spawn_locked(&s, "backup.run", who, &j.clone(), move |ctx| async move {
        let mut warnings = Vec::new();
        let res = run_job(&s2, &j, &sec, &ctx, &mut warnings).await;
        // the run's own fields were set as it went
        let mut cur = status::read(&s2.prot.state_dir, &j.name);
        cur.last_end_ms = Some(rynas_tasks::now_ms());
        cur.last_warnings = warnings;
        match &res {
            Ok(()) => {
                cur.last_ok_ms = cur.last_end_ms;
                cur.last_error = None;
            }
            Err(e) => {
                let e = rynas_api_types::backup::redact_secrets(e, &sec.values());
                cur.last_error = Some(e.chars().take(500).collect());
            }
        }
        let _ = status::write(&s2.prot.state_dir, &j.name, &cur);
        res.map_err(|e| rynas_api_types::backup::redact_secrets(&e, &sec.values()))
    })
}

/// A temporary snapshot to delete at the end.
struct TempSnap {
    parent: PathBuf,
    name: String,
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|_| "rynas".into())
}

/// What is read for the folder `path`: an encrypted folder (D-CRYPT E1) is backed up as its
/// ciphertext directory `<place>/.rynas-crypt/<name>` (with `gocryptfs.conf` and the key
/// check), so the backup stays encrypted whether the folder is locked or not; restoring needs
/// the password or the recovery key. Its mount point is never read: locked it is an empty
/// folder, and an empty snapshot would let the retention expire the real ones. Without its
/// ciphertext the source fails.
fn backup_path(path: &str, encrypted: bool) -> Result<String, String> {
    use rynas_services::crypt;
    let cipher = crypt::cipher_dir(path);
    let has_conf = |c: &str| FsPath::new(c).join(crypt::CONF).is_file();
    if !encrypted && !cipher.as_deref().is_some_and(has_conf) {
        return Ok(path.to_string());
    }
    match cipher {
        Some(c) if has_conf(&c) => Ok(c),
        Some(c) => Err(format!(
            "{path} is an encrypted folder but its encrypted data ({c}/{}) is missing: not backed up",
            crypt::CONF
        )),
        None => Err(format!("{path} cannot be an encrypted folder: not backed up")),
    }
}

/// The folder of each source (shares resolved from the applied configuration).
fn source_paths(s: &ResticSvc, job: &BackupJob) -> Result<Vec<(String, Option<BackupSource>)>, String> {
    let shares: SharesConfig = from_value(s.prot.ops.config.applied(SHARES_KEY).map_err(|e| e.to_string())?)
        .map_err(|e| e.2)?;
    let mut out = Vec::new();
    for src in &job.sources {
        match src {
            BackupSource::Share { name } => {
                let sh = shares
                    .shares
                    .iter()
                    .find(|x| &x.name == name)
                    .ok_or_else(|| format!("share {name} is not in the applied configuration"))?;
                out.push((backup_path(&sh.path, sh.encrypted)?, None));
            }
            BackupSource::Folder { path } => out.push((backup_path(path, false)?, None)),
            BackupSource::Config => out.push((String::new(), Some(BackupSource::Config))),
        }
    }
    Ok(out)
}

/// A read-only snapshot of `path` when it is a btrfs subvolume:
/// `<parent>/.rynas-backup/<job>-<n>`.
fn snapshot_of(path: &str, job: &str, n: usize) -> std::io::Result<Option<TempSnap>> {
    use rynas_btrfs::ioctl;
    let src = ioctl::open_dir(FsPath::new(path))?;
    if !ioctl::is_btrfs(&src) || !ioctl::is_subvolume(&src) {
        return Ok(None);
    }
    let parent = FsPath::new(path)
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent"))?
        .join(".rynas-backup");
    std::fs::create_dir_all(&parent)?;
    std::fs::set_permissions(&parent, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    let name = format!("{job}-{n}");
    let dir = ioctl::open_dir(&parent)?;
    // a leftover of an interrupted run
    if parent.join(&name).exists() {
        let _ = ioctl::destroy_subvolume(&dir, &name);
    }
    ioctl::create_snapshot(&src, &dir, &name, true)?;
    Ok(Some(TempSnap { parent, name }))
}

fn drop_snapshots(snaps: &[TempSnap], ctx: &TaskCtx) {
    for t in snaps {
        let r = rynas_btrfs::ioctl::open_dir(&t.parent)
            .and_then(|d| rynas_btrfs::ioctl::destroy_subvolume(&d, &t.name));
        match r {
            Ok(()) => {
                let _ = std::fs::remove_dir(&t.parent);
            }
            Err(e) => ctx.log(&format!(
                "warning: temporary snapshot {}/{} not deleted: {e}",
                t.parent.display(),
                t.name
            )),
        }
    }
}

/// Containers of `stack` that are running (`docker ps` by compose project).
async fn stack_containers(stack: &str) -> Vec<String> {
    let out = rynas_tasks::Cmd::new("docker")
        .args(["ps", "-q", "--filter"])
        .arg(format!("label=com.docker.compose.project={stack}"))
        .timeout(Duration::from_secs(30))
        .run(None, None)
        .await;
    match out {
        Ok(o) if o.success() => o.stdout.split_whitespace().map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

async fn docker(args: &[&str], ids: &[String], ctx: &TaskCtx) {
    if ids.is_empty() {
        return;
    }
    let r = rynas_tasks::Cmd::new("docker")
        .args(args)
        .args(ids)
        .timeout(Duration::from_secs(300))
        .run(None, None)
        .await;
    if !matches!(&r, Ok(o) if o.success()) {
        ctx.log(&format!("warning: docker {} failed", args.join(" ")));
    }
}

async fn run_job(
    s: &Arc<ResticSvc>,
    job: &BackupJob,
    sec: &JobSecrets,
    ctx: &TaskCtx,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let spec = repo_spec(&job.destination, sec, job.bandwidth_kib);
    ctx.log(&format!(
        "backup job {} to {} ({})",
        job.name,
        job.destination.summary(),
        job.destination.kind()
    ));
    if let Destination::Local { path } = &job.destination
        && !s.dry
    {
        // never onto the system disk when the USB disk is not plugged in (review M6); the
        // folder must exist for the worker's sandbox to make it writable
        check_local_destination(path, &mountinfo())?;
        if crate::places::in_system_dir(path) {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || {
                crate::places::ensure_system_dir(FsPath::new("/"), true, &|m| c.log(m))
            })
            .await
            .map_err(|e| e.to_string())??;
            let w = SAME_DISK_WARNING.to_string();
            ctx.log(&format!("warning: {w}"));
            warnings.push(w);
        }
        std::fs::create_dir_all(path).map_err(|e| format!("{path}: {e}"))?;
    }
    if let Destination::Sftp { .. } = &job.destination
        && sec.ssh_known_hosts.trim().is_empty()
    {
        let w = "the SFTP server's host key was not pinned by a test: trusted on first use".to_string();
        ctx.log(&format!("warning: {w}"));
        warnings.push(w);
    }
    if let WorkerEvent::Initialized { created: true } = op(s, job, &spec, WorkerOp::Init, ctx).await? {
        ctx.log("repository created");
    }
    let state_dir = s.prot.state_dir.clone();
    let paths = source_paths(s, job)?;
    // the backup reads the volumes of its sources: their deduplication gives way (D-DEDUP)
    let fs_paths: Vec<&str> = paths.iter().filter(|(_, sp)| sp.is_none()).map(|(p, _)| p.as_str()).collect();
    let _fs = crate::fslock::FsLocks::global().heavy_paths(&fs_paths, "backup.run").await;
    for (p, _) in &paths {
        if p.contains(&format!("/{}/", rynas_services::crypt::CRYPT_DIR)) {
            ctx.log(&format!(
                "{p}: the encrypted data of an encrypted folder, backed up as it is (restoring needs the folder's password or recovery key; exclude patterns do not match its encrypted names)"
            ));
        }
    }
    // the configuration export, encrypted with the repository password
    let cfg_dir = state_dir.join("backup").join(format!("config-{}", job.name));
    let mut sources = Vec::new();
    let mut snaps = Vec::new();
    let mut stopped: Vec<String> = Vec::new();
    record_run(&state_dir, &job.name, |r| {
        *r = RunState {
            started_ms: rynas_tasks::now_ms(),
            ..Default::default()
        }
    });
    if !s.dry {
        for st in &job.stop_stacks {
            // `docker ps` lists the newest first: stopped in that order (dependents before
            // what they depend on), started again oldest first, as compose created them
            let ids = stack_containers(st).await;
            if !ids.is_empty() {
                let mut start_order = ids.clone();
                start_order.reverse();
                // written before the stop: a rynasd that dies now starts them at its start
                record_run(&state_dir, &job.name, |r| {
                    let mut all = start_order.clone();
                    all.append(&mut r.stopped_containers);
                    r.stopped_containers = all;
                });
                ctx.log(&format!("stopping stack {st}"));
                docker(&["stop"], &ids, ctx).await;
                let mut all = start_order;
                all.append(&mut stopped);
                stopped = all;
            }
        }
    }
    let mut n = 0usize;
    let prepared: Result<(), String> = async {
        for (path, special) in &paths {
            if special.is_some() {
                let _ = std::fs::remove_dir_all(&cfg_dir);
                std::fs::create_dir_all(&cfg_dir).map_err(|e| e.to_string())?;
                std::fs::set_permissions(&cfg_dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                    .map_err(|e| e.to_string())?;
                let (c, sd, out, pw) = (
                    s.prot.ops.config.clone(),
                    state_dir.clone(),
                    cfg_dir.join("rynas.rynasbak"),
                    sec.repository_password.clone(),
                );
                tokio::task::spawn_blocking(move || {
                    crate::backup::export(&c, &sd, &out, &hostname(), Some(&pw))
                })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("configuration export: {e}"))?;
                sources.push(WorkerSource {
                    read: cfg_dir.display().to_string(),
                    as_path: "/rynas-config".into(),
                    excludes: vec![],
                });
                continue;
            }
            if !(job.use_snapshot && !s.dry) {
                sources.push(WorkerSource {
                    read: path.clone(),
                    as_path: path.clone(),
                    excludes: job.excludes.clone(),
                });
                continue;
            }
            // the source and every subvolume inside it (a snapshot holds a nested
            // subvolume as an empty folder: review M3), each read from its own snapshot
            let p = path.clone();
            let (nested, truncated) = tokio::task::spawn_blocking(move || {
                // only a source that is a subvolume is snapshotted: one that is not is read
                // live, nested subvolumes included
                let sub = rynas_btrfs::ioctl::open_dir(FsPath::new(&p)).is_ok_and(|d| rynas_btrfs::ioctl::is_subvolume(&d));
                if sub { child_subvolumes(FsPath::new(&p), 100_000) } else { (Vec::new(), false) }
            })
            .await
            .map_err(|e| e.to_string())?;
            if truncated {
                let w = format!("{path}: too many folders to look for nested subvolumes in all of them");
                ctx.log(&format!("warning: {w}"));
                warnings.push(w);
            }
            for sub in std::iter::once(path.clone()).chain(nested.iter().map(|p| p.display().to_string())) {
                if sub != *path {
                    let w = format!("{sub}: a subvolume inside {path}, backed up as a source of its own");
                    ctx.log(&w);
                    warnings.push(w);
                }
                let mut read = sub.clone();
                if let Some(planned) = planned_snapshot(&sub, &job.name, n) {
                    // recorded before it exists: rynasd deletes it at its start if it dies
                    record_run(&state_dir, &job.name, |r| r.temp_snapshots.push(planned.display().to_string()));
                }
                let (p, j) = (sub.clone(), job.name.clone());
                let k = n;
                n += 1;
                match tokio::task::spawn_blocking(move || snapshot_of(&p, &j, k)).await {
                    Ok(Ok(Some(t))) => {
                        read = t.parent.join(&t.name).display().to_string();
                        ctx.log(&format!("read-only snapshot of {sub}"));
                        snaps.push(t);
                    }
                    Ok(Ok(None)) => ctx.log(&format!("{sub}: not a btrfs subvolume, read as it is")),
                    Ok(Err(e)) => ctx.log(&format!("{sub}: no snapshot ({e}), read as it is")),
                    Err(e) => return Err(e.to_string()),
                }
                let excludes = anchored_to(&job.excludes, &sub, &read);
                sources.push(WorkerSource {
                    read,
                    as_path: sub,
                    excludes,
                });
            }
        }
        Ok(())
    }
    .await;
    // the stacks run again as soon as their data is frozen in snapshots; without
    // snapshots they wait for the backup
    let live = sources.iter().filter(|x| x.as_path != "/rynas-config").count();
    let restart_early = prepared.is_ok() && job.use_snapshot && !snaps.is_empty() && snaps.len() == live;
    if restart_early && !stopped.is_empty() {
        ctx.log("starting the stacks again");
        docker(&["start"], &stopped, ctx).await;
        stopped.clear();
        record_run(&state_dir, &job.name, |r| r.stopped_containers.clear());
    }
    let result: Result<(), String> = async {
        prepared?;
        let backup = WorkerOp::Backup {
            sources: sources.clone(),
            tags: vec!["rynas".into(), job_tag(&job.name)],
            host: hostname(),
            excludes: vec![],
        };
        let unit = matches!(s.launch, Launch::Unit { .. }).then(|| rynas_backup::runner::unit_name(&job.name));
        record_run(&state_dir, &job.name, |r| r.unit = unit.clone().map(|u| format!("{u}.service")));
        let (mut read_b, mut added_b) = (0, 0);
        let mut seen = std::collections::HashSet::new();
        let mut f = follow(ctx);
        let mut changed = Vec::new();
        let limits = Limits {
            stall: Some(STALL),
            unit,
            ..Default::default()
        };
        let r = run_worker(
            &s.launch,
            &job.name,
            &s.request(&job.name, spec.clone(), backup),
            ctx.token(),
            &limits,
            |ev: &WorkerEvent| {
                match ev {
                    // one event per source; the final result repeats the last one
                    WorkerEvent::BackedUp { snapshot } => {
                        if !seen.insert(snapshot.id.clone()) {
                            return;
                        }
                        read_b += snapshot.bytes.unwrap_or(0);
                        added_b += snapshot.added_bytes.unwrap_or(0);
                    }
                    WorkerEvent::SecretChanged { name, content } => changed.push((name.clone(), content.clone())),
                    _ => {}
                }
                f(ev)
            },
        )
        .await;
        store_changed(s, job, &changed, Some(ctx));
        r?;
        let mut st = status::read(&state_dir, &job.name);
        st.source_bytes = Some(read_b);
        st.added_bytes = Some(added_b);
        status::write(&state_dir, &job.name, &st).map_err(|e| e.to_string())?;
        Ok(())
    }
    .await;
    if !stopped.is_empty() {
        ctx.log("starting the stacks again");
        docker(&["start"], &stopped, ctx).await;
    }
    drop_snapshots(&snaps, ctx);
    let _ = std::fs::remove_dir_all(&cfg_dir);
    clear_run(&state_dir, &job.name);
    result?;
    maintenance(s, job, &spec, ctx).await
}

/// Changes the run state in the job's status (written at once).
fn record_run(state_dir: &FsPath, job: &str, f: impl FnOnce(&mut RunState)) {
    let mut st = status::read(state_dir, job);
    let mut r = st.run.take().unwrap_or_default();
    f(&mut r);
    st.run = Some(r);
    if let Err(e) = status::write(state_dir, job, &st) {
        tracing::warn!("backup {job}: run state not written: {e}");
    }
}

fn clear_run(state_dir: &FsPath, job: &str) {
    let mut st = status::read(state_dir, job);
    if st.run.take().is_some() {
        let _ = status::write(state_dir, job, &st);
    }
}

/// Where [`snapshot_of`] puts the snapshot of `path`.
fn planned_snapshot(path: &str, job: &str, n: usize) -> Option<PathBuf> {
    Some(FsPath::new(path).parent()?.join(".rynas-backup").join(format!("{job}-{n}")))
}

/// The job's exclude globs for one source read from `read` instead of its real path
/// `as_path`: an anchored glob below `as_path` is rewritten to `read` (review L5).
fn anchored_to(excludes: &[String], as_path: &str, read: &str) -> Vec<String> {
    excludes
        .iter()
        .map(|g| {
            if read != as_path
                && let Some(rest) = g.strip_prefix(as_path)
                && (rest.is_empty() || rest.starts_with('/'))
            {
                format!("{read}{rest}")
            } else {
                g.clone()
            }
        })
        .collect()
}

/// Subvolumes inside `root` (not below another one), found by a walk of its folders that
/// stops at other filesystems and subvolumes (each has its own device number on btrfs).
/// At most `limit` folders are read (`true`: the walk stopped there).
fn child_subvolumes(root: &FsPath, limit: usize) -> (Vec<PathBuf>, bool) {
    use std::os::unix::fs::MetadataExt;
    const SKIP: [&str; 4] = [".rynas-backup", ".snapshots", "#recycle", "@eaDir"];
    let Ok(m) = std::fs::symlink_metadata(root) else {
        return (Vec::new(), false);
    };
    let (mut out, mut stack, mut seen) = (Vec::new(), vec![(root.to_path_buf(), m.dev())], 0usize);
    while let Some((dir, dev)) = stack.pop() {
        seen += 1;
        if seen > limit {
            return (out, true);
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if SKIP.iter().any(|s| e.file_name() == *s) {
                continue;
            }
            let p = e.path();
            let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
            if m.dev() == dev {
                stack.push((p, dev));
            } else if m.ino() == 256 {
                // the top folder of a btrfs subvolume (and the ones inside it)
                out.push(p.clone());
                stack.push((p, m.dev()));
            }
            // another filesystem mounted inside: left out (restic reads it live anyway
            // without a snapshot, and a snapshot never holds it)
        }
    }
    out.sort();
    (out, false)
}

async fn maintenance(s: &ResticSvc, job: &BackupJob, spec: &RepoSpec, ctx: &TaskCtx) -> Result<(), String> {
    let state_dir = &s.prot.state_dir;
    let now = rynas_tasks::now_ms();
    if let WorkerEvent::Forgot { keep, remove } = op(
        s,
        job,
        spec,
        WorkerOp::Forget {
            scope: scope_of(&job.name),
            retention: job.retention,
            dry_run: false,
        },
        ctx,
    )
    .await?
    {
        ctx.log(&format!("retention: {} kept, {} removed", keep.len(), remove.len()));
        let mut st = status::read(state_dir, &job.name);
        st.snapshots = keep.len() as u32;
        let _ = status::write(state_dir, &job.name, &st);
    }
    let st = status::read(state_dir, &job.name);
    if status::due(st.last_prune_ms, job.maintenance.prune_every_days, now) {
        if let WorkerEvent::Pruned { removed_packs, freed_bytes } = op(s, job, spec, WorkerOp::Prune, ctx).await? {
            ctx.log(&format!("prune: {removed_packs} packs, {} to free", bytes(freed_bytes)));
        }
        let mut st = status::read(state_dir, &job.name);
        st.last_prune_ms = Some(now);
        let _ = status::write(state_dir, &job.name, &st);
    }
    let st = status::read(state_dir, &job.name);
    if status::due(st.last_check_ms, job.maintenance.check_every_days, now) {
        check_into_status(s, job, spec, job.maintenance.check_read_percent, ctx).await?;
    }
    if let Ok(WorkerEvent::Stats { bytes: b, .. }) = op(s, job, spec, WorkerOp::Stats, ctx).await {
        let mut st = status::read(state_dir, &job.name);
        st.repository_bytes = Some(b);
        let _ = status::write(state_dir, &job.name, &st);
        ctx.log(&format!("repository size {}", bytes(b)));
    }
    Ok(())
}

async fn check_into_status(
    s: &ResticSvc,
    job: &BackupJob,
    spec: &RepoSpec,
    read_percent: u8,
    ctx: &TaskCtx,
) -> Result<(), String> {
    let r = op(s, job, spec, WorkerOp::Check { read_percent, slice: None }, ctx).await?;
    let state_dir = &s.prot.state_dir;
    let mut st = status::read(state_dir, &job.name);
    st.last_check_ms = Some(rynas_tasks::now_ms());
    let res = match r {
        WorkerEvent::Checked { errors, warnings } => {
            for w in &warnings {
                ctx.log(&format!("check warning: {w}"));
            }
            st.last_check_ok = Some(errors.is_empty());
            if errors.is_empty() {
                ctx.log(&format!("check: no errors ({read_percent} % of the data read)"));
                Ok(())
            } else {
                for e in &errors {
                    ctx.log(&format!("check error: {e}"));
                }
                Err(format!("the repository check found {} errors", errors.len()))
            }
        }
        _ => Err("unexpected answer to the check".into()),
    };
    let _ = status::write(state_dir, &job.name, &st);
    res
}

fn started(id: String) -> Response {
    (StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response()
}

async fn run_now(State(st): State<RsState>, h: HeaderMap, Path(name): Path<String>) -> Res<Response> {
    let (_s, owner) = (svc(&st)?, user(&h)?);
    check_name(&name)?;
    let id = spawn_run(&owner, &name)?;
    audit(&owner, &format!("job {name} started"));
    Ok(started(id))
}

async fn check_now(
    State(st): State<RsState>,
    h: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<CheckRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    check_name(&name)?;
    let req = json_body(body)?;
    if req.read_percent > 100 {
        return Err(invalid("read 0 to 100 % of the data"));
    }
    let (job, sec) = applied_job(&s, &name)?;
    let s2 = s.clone();
    let id = spawn_locked(&s, "backup.check", &owner, &job.clone(), move |ctx| async move {
        let spec = repo_spec(&job.destination, &sec, job.bandwidth_kib);
        check_into_status(&s2, &job, &spec, req.read_percent, &ctx).await
    })?;
    audit(&owner, &format!("check of {name} started"));
    Ok(started(id))
}

async fn read_op(s: &Arc<ResticSvc>, name: &str, op: WorkerOp) -> Res<WorkerEvent> {
    let (job, sec) = {
        let (s2, n) = (s.clone(), name.to_string());
        blocking(&s.prot, move |_| applied_job(&s2, &n)).await?
    };
    let req = s.request(name, repo_spec(&job.destination, &sec, job.bandwidth_kib), op);
    let limits = Limits {
        timeout: Some(READ_TIMEOUT),
        ..Default::default()
    };
    let mut changed = Vec::new();
    let r = run_worker(&s.launch, name, &req, &CancellationToken::new(), &limits, |ev| {
        if let WorkerEvent::SecretChanged { name, content } = ev {
            changed.push((name.clone(), content.clone()));
        }
    })
    .await;
    store_changed(s, &job, &changed, None);
    r.map_err(|e| {
        if e.starts_with("the file is too large") {
            conflict(error_codes::CONFLICT, e)
        } else {
            Error::new(StatusCode::BAD_GATEWAY, error_codes::BACKEND_UNAVAILABLE, e)
        }
    })
}

#[derive(Deserialize, Default)]
struct SnapshotsQuery {
    /// The job's snapshots written by any NAS, not only this one.
    #[serde(default)]
    all_hosts: bool,
}

async fn snapshots(
    State(st): State<RsState>,
    Path(name): Path<String>,
    Query(q): Query<SnapshotsQuery>,
) -> Res<Json<Vec<SnapshotEntry>>> {
    let s = svc(&st)?;
    check_name(&name)?;
    let mut scope = scope_of(&name);
    if q.all_hosts {
        scope.host.clear();
    }
    match read_op(&s, &name, WorkerOp::Snapshots { scope: Some(scope) }).await? {
        WorkerEvent::Snapshots { list } => Ok(Json(list)),
        _ => Err(Error::internal("unexpected answer")),
    }
}

#[derive(Deserialize)]
struct BrowseQuery {
    snapshot: String,
    #[serde(default)]
    path: String,
}

fn check_snapshot_id(id: &str) -> Res<()> {
    if id == "latest" || ((1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())) {
        Ok(())
    } else {
        Err(invalid("invalid snapshot id"))
    }
}

fn check_inner_path(p: &str) -> Res<()> {
    if p.len() > 4096 || p.chars().any(char::is_control) || p.split('/').any(|c| c == "..") {
        return Err(invalid("invalid path"));
    }
    Ok(())
}

async fn browse(
    State(st): State<RsState>,
    Path(name): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> Res<Json<Vec<NodeEntry>>> {
    let s = svc(&st)?;
    check_name(&name)?;
    check_snapshot_id(&q.snapshot)?;
    check_inner_path(&q.path)?;
    match read_op(
        &s,
        &name,
        WorkerOp::Ls {
            snapshot: q.snapshot,
            path: q.path,
            scope: Some(scope_of(&name)),
        },
    )
    .await?
    {
        WorkerEvent::Entries { mut list } => {
            list.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
            Ok(Json(list))
        }
        _ => Err(Error::internal("unexpected answer")),
    }
}

async fn restore(
    State(st): State<RsState>,
    h: HeaderMap,
    Path(name): Path<String>,
    Query(q): Query<RestoreQuery>,
    body: Result<Json<RestoreRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    check_name(&name)?;
    let req = json_body(body)?;
    check_snapshot_id(&req.snapshot)?;
    for p in &req.paths {
        check_inner_path(p)?;
    }
    let roots = restore_roots(&mountinfo());
    let target = match &req.target {
        RestoreTarget::Original => {
            if !q.confirm {
                return Err(conflict(
                    error_codes::CONFIRMATION_REQUIRED,
                    "restoring to the original place replaces the files there: confirm with ?confirm=true",
                ));
            }
            None
        }
        RestoreTarget::Folder { path } => {
            let p = path.trim().trim_end_matches('/').to_string();
            Some(check_target(&p, &roots).map_err(invalid)?)
        }
    };
    let (job, sec) = applied_job(&s, &name)?;
    let s2 = s.clone();
    let (snap, paths) = (req.snapshot.clone(), req.paths.clone());
    let id = spawn_locked(&s, "backup.restore", &owner, &job.clone(), move |ctx| async move {
        let spec = repo_spec(&job.destination, &sec, job.bandwidth_kib);
        let scope = scope_of(&job.name);
        if let Some(t) = target {
            // into the system volume: its folder made ready and checked first (D-ROOTVOL),
            // never a plain folder made in its place
            let sys = rynas_api_types::storage::SYSTEM_VOLUME_DIR;
            if (t == sys || crate::places::in_system_dir(&t)) && !s2.dry {
                let c = ctx.clone();
                tokio::task::spawn_blocking(move || {
                    crate::places::ensure_system_dir(FsPath::new("/"), true, &|m| c.log(m))
                })
                .await
                .map_err(|e| e.to_string())??;
            }
            std::fs::create_dir_all(&t).map_err(|e| format!("{t}: {e}"))?;
            let r = op(
                &s2,
                &job,
                &spec,
                WorkerOp::Restore {
                    snapshot: snap,
                    paths,
                    target: Some(t),
                    scope: Some(scope),
                },
                &ctx,
            )
            .await?;
            if let WorkerEvent::Restored { files, bytes: b } = r {
                ctx.log(&format!("restored {files} files, {}", bytes(b)));
            }
            return Ok(());
        }
        // the original place: the snapshot resolved once (its paths when none are given),
        // each path restored into a fresh staging folder next to it, then moved in place
        // (review M2): a failed or cancelled restore leaves the originals as they were
        let WorkerEvent::Snapshots { list } = op(&s2, &job, &spec, WorkerOp::Snapshots { scope: Some(scope.clone()) }, &ctx).await? else {
            return Err("unexpected answer".into());
        };
        let entry = if snap == "latest" {
            list.first()
        } else {
            list.iter().find(|e| e.id.starts_with(&snap))
        }
        .ok_or_else(|| format!("no snapshot {snap} of this job on this NAS"))?;
        let paths = if paths.is_empty() { entry.paths.clone() } else { paths };
        let roots = restore_roots(&mountinfo());
        let mut moved = 0u64;
        for (n, p) in paths.iter().enumerate() {
            let p = p.trim_end_matches('/').to_string();
            check_target(&p, &roots).map_err(|e| format!("{p}: not restored to its original place: {e}"))?;
            let staging = staging_for(FsPath::new(&p), n).map_err(|e| format!("{p}: staging folder: {e}"))?;
            let r = op(
                &s2,
                &job,
                &spec,
                WorkerOp::Restore {
                    snapshot: entry.id.clone(),
                    paths: vec![p.clone()],
                    target: Some(staging.display().to_string()),
                    scope: Some(scope.clone()),
                },
                &ctx,
            )
            .await;
            let res = match r {
                Ok(_) => {
                    let (src, dst) = (staging.join(p.trim_start_matches('/')), PathBuf::from(&p));
                    tokio::task::spawn_blocking(move || {
                        let mut m = Moved::default();
                        merge_move(&src, &dst, &mut m).map(|_| m)
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| format!("{p}: moving the restored files in place: {e}")))
                }
                Err(e) => Err(e),
            };
            let _ = std::fs::remove_dir_all(&staging);
            let m = res?;
            for w in &m.skipped {
                ctx.log(&format!("warning: {w}"));
            }
            moved += m.files;
        }
        ctx.log(&format!("restored {moved} files to their original place"));
        Ok(())
    })?;
    audit(&owner, &format!("restore from {name} started"));
    Ok(started(id))
}

#[derive(Deserialize, Default)]
struct RestoreQuery {
    /// Required for a restore to the original place (it replaces files).
    #[serde(default)]
    confirm: bool,
}

/// The folders a restore may write below: the rynas volumes (`/<label>`, btrfs top levels),
/// the system volume's folder (D-ROOTVOL) and the disks mounted below `/media` (as the file
/// manager and the other copy-out features); never the rest of the system disk.
fn restore_roots(mountinfo: &str) -> Vec<String> {
    let mut v: Vec<String> = crate::places::places_in(mountinfo)
        .into_iter()
        .map(|(_, mp)| mp)
        .collect();
    v.extend(
        rynas_storage::mounts::parse_mountinfo(mountinfo)
            .into_iter()
            .map(|m| m.mountpoint)
            .filter(|mp| mp.starts_with("/media/") && mp.matches('/').count() >= 2),
    );
    v.sort();
    v.dedup();
    v
}

/// A restore target (a folder to restore into, or an original path): strictly below one of
/// `roots`, normalized, no `"`, `\` or control characters, never inside rynas' own
/// temporary folders, and no symlink in any existing component (review M2).
fn check_target(p: &str, roots: &[String]) -> Result<String, String> {
    if !rynas_backup::domain::clean_abs(p) || p == "/" || p.contains(['"', '\\']) {
        return Err("an absolute folder inside a volume or a mounted disk, such as /volume1/restore".into());
    }
    let Some(root) = roots
        .iter()
        .filter(|r| p.starts_with(&format!("{}/", r.trim_end_matches('/'))))
        .max_by_key(|r| r.len())
    else {
        return Err("only inside a volume or a mounted disk (such as /volume1/restore or /media/usb1/restore)".into());
    };
    let rest = &p[root.trim_end_matches('/').len() + 1..];
    if rest.split('/').any(|c| c == ".rynas-backup" || c.starts_with(".rynas-restore-")) {
        return Err("not inside rynas' temporary folders".into());
    }
    // the root itself and each existing component: real folders, never symlinks
    let mut cur = PathBuf::from(root);
    for c in std::iter::once("").chain(rest.split('/')) {
        if !c.is_empty() {
            cur.push(c);
        }
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("{} is a symbolic link", cur.display()));
            }
            Ok(m) if !m.is_dir() && cur != FsPath::new(p) => {
                return Err(format!("{} is not a folder", cur.display()));
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(p.to_string())
}

/// A fresh staging folder for restoring `p` in place: inside `p` when it is a folder (the
/// same subvolume as what it replaces, so the move is a rename), else next to it.
fn staging_for(p: &FsPath, n: usize) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let name = format!(".rynas-restore-{}-{n}", rynas_tasks::now_ms());
    let is_dir = std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    let mut base = if is_dir { p.to_path_buf() } else { p.parent().unwrap_or(p).to_path_buf() };
    while !base.is_dir() {
        base = match base.parent() {
            Some(b) => b.to_path_buf(),
            None => return Err(std::io::Error::other("no existing parent")),
        };
    }
    let dir = base.join(name);
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

/// What [`merge_move`] did.
#[derive(Default, Debug)]
struct Moved {
    files: u64,
    skipped: Vec<String>,
}

/// Moves the restored tree `src` onto `dst`: files replace files (rename, atomic), folders
/// merge, nothing is followed through a symlink; across filesystems (a nested subvolume)
/// files are copied. A folder in the way of a file is left alone and reported.
fn merge_move(src: &FsPath, dst: &FsPath, m: &mut Moved) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let sm = std::fs::symlink_metadata(src)?;
    let dm = std::fs::symlink_metadata(dst).ok();
    if sm.is_dir() {
        match dm {
            Some(d) if d.is_dir() => {
                for e in std::fs::read_dir(src)? {
                    let e = e?;
                    merge_move(&e.path(), &dst.join(e.file_name()), m)?;
                }
                // the folder's own owner and mode, as in the snapshot
                let _ = std::os::unix::fs::lchown(dst, Some(sm.uid()), Some(sm.gid()));
                let _ = std::fs::set_permissions(dst, sm.permissions());
                let _ = std::fs::remove_dir(src);
                Ok(())
            }
            other => {
                if other.is_some() {
                    // a file or symlink where the folder was: replaced
                    std::fs::remove_file(dst)?;
                }
                match std::fs::rename(src, dst) {
                    Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                        std::fs::create_dir(dst)?;
                        for e in std::fs::read_dir(src)? {
                            let e = e?;
                            merge_move(&e.path(), &dst.join(e.file_name()), m)?;
                        }
                        let _ = std::os::unix::fs::lchown(dst, Some(sm.uid()), Some(sm.gid()));
                        let _ = std::fs::set_permissions(dst, sm.permissions());
                        Ok(())
                    }
                    Err(e) => Err(e),
                    Ok(()) => {
                        m.files += count_files(dst);
                        Ok(())
                    }
                }
            }
        }
    } else {
        if dm.as_ref().is_some_and(|d| d.is_dir()) {
            m.skipped.push(format!("{}: a folder is there now, the restored file was not put in its place", dst.display()));
            return Ok(());
        }
        match std::fs::rename(src, dst) {
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                copy_across(src, dst, &sm)?;
                m.files += 1;
                Ok(())
            }
            Err(e) => Err(e),
            Ok(()) => {
                m.files += 1;
                Ok(())
            }
        }
    }
}

fn count_files(p: &FsPath) -> u64 {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| count_files(&e.path()))
            .sum(),
        Ok(_) => 1,
        Err(_) => 0,
    }
}

/// A file or symlink copied next to `dst` and renamed over it (another filesystem).
fn copy_across(src: &FsPath, dst: &FsPath, sm: &std::fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let tmp = dst.with_file_name(format!(
        ".{}.rynas-restore",
        dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    let _ = std::fs::remove_file(&tmp);
    if sm.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, &tmp)?;
    } else if sm.is_file() {
        std::fs::copy(src, &tmp)?;
        let f = std::fs::File::options().write(true).open(&tmp)?;
        if let Ok(t) = sm.modified() {
            let _ = f.set_modified(t);
        }
    } else {
        // devices, fifos, sockets: left in the staging folder (removed after)
        return Ok(());
    }
    let _ = std::os::unix::fs::lchown(&tmp, Some(sm.uid()), Some(sm.gid()));
    std::fs::rename(&tmp, dst)
}

async fn download(
    State(st): State<RsState>,
    Path(name): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> Res<Response> {
    let s = svc(&st)?;
    check_name(&name)?;
    check_snapshot_id(&q.snapshot)?;
    check_inner_path(&q.path)?;
    // a folder both rynasd and the worker's unit see (each has a private /tmp): below
    // rynasd's state, 0700 root, one fresh folder per download, removed after (review H1)
    let base = dl_dir(&s.prot.state_dir);
    let dir = {
        let b = base.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<tempfile::TempDir> {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&b)?;
            tempfile::Builder::new().prefix("dl-").tempdir_in(&b)
        })
        .await
        .map_err(Error::internal)?
        .map_err(Error::internal)?
    };
    let out = dir.path().join("file");
    match read_op(
        &s,
        &name,
        WorkerOp::Dump {
            snapshot: q.snapshot.clone(),
            path: q.path.clone(),
            out: out.display().to_string(),
            // refused by the worker before a byte is written
            max_bytes: MAX_DOWNLOAD,
            scope: Some(scope_of(&name)),
        },
    )
    .await?
    {
        WorkerEvent::Dumped { bytes } if bytes > MAX_DOWNLOAD => {
            return Err(conflict(
                error_codes::CONFLICT,
                "the file is too large to download: restore it to a folder",
            ));
        }
        WorkerEvent::Dumped { .. } => {}
        _ => return Err(Error::internal("unexpected answer")),
    }
    let data = {
        let o = out.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            use std::io::Read;
            use std::os::unix::fs::OpenOptionsExt;
            let f = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&o)?;
            let mut buf = Vec::new();
            f.take(MAX_DOWNLOAD + 1).read_to_end(&mut buf)?;
            Ok(buf)
        })
        .await
        .map_err(Error::internal)?
        .map_err(Error::internal)?
    };
    drop(dir);
    if data.len() as u64 > MAX_DOWNLOAD {
        return Err(conflict(
            error_codes::CONFLICT,
            "the file is too large to download: restore it to a folder",
        ));
    }
    let mut r = data.into_response();
    let hd = r.headers_mut();
    hd.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    if let Ok(v) = HeaderValue::from_str(&content_disposition(q.path.rsplit('/').next().unwrap_or("file"))) {
        hd.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(r)
}

/// `<state>/backup/dl`: downloads in flight (emptied at rynasd's start).
fn dl_dir(state_dir: &FsPath) -> PathBuf {
    status::dir(state_dir).join("dl")
}

/// `attachment` with an ASCII `filename` and, for other names, `filename*` (RFC 6266/5987).
fn content_disposition(name: &str) -> String {
    let name = if name.is_empty() { "file" } else { name };
    let ascii: String = name
        .chars()
        .map(|c| if c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\' { c } else { '_' })
        .collect();
    if ascii == name {
        return format!("attachment; filename=\"{ascii}\"");
    }
    let mut enc = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            enc.push(b as char);
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{enc}")
}

async fn preview_retention(
    State(st): State<RsState>,
    Path(name): Path<String>,
) -> Res<Json<RetentionPreview>> {
    let s = svc(&st)?;
    check_name(&name)?;
    let (job, _) = {
        let (s2, n) = (s.clone(), name.clone());
        blocking(&s.prot, move |_| applied_job(&s2, &n)).await?
    };
    match read_op(
        &s,
        &name,
        WorkerOp::Forget {
            scope: scope_of(&name),
            retention: job.retention,
            dry_run: true,
        },
    )
    .await?
    {
        WorkerEvent::Forgot { keep, remove } => Ok(Json(RetentionPreview { keep, remove })),
        _ => Err(Error::internal("unexpected answer")),
    }
}

// ---- recovery at rynasd's start (review H2) ----

/// What recovery does to the system (a fake in the tests).
pub trait RecoverHost: Send + Sync {
    /// Stops every unit whose name starts with `prefix`; the names stopped.
    fn stop_units(&self, prefix: &str) -> Vec<String>;
    fn start_containers(&self, ids: &[String]) -> Result<(), String>;
    /// Deletes the read-only snapshot `path` (a btrfs subvolume).
    fn destroy_snapshot(&self, path: &FsPath) -> Result<(), String>;
}

/// The real system: `systemctl`, `docker`, btrfs ioctls.
pub struct HostRecover;

impl RecoverHost for HostRecover {
    fn stop_units(&self, prefix: &str) -> Vec<String> {
        let out = std::process::Command::new("systemctl")
            .args(["list-units", "--all", "--plain", "--no-legend", "--type=service"])
            .arg(format!("{prefix}*"))
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .output();
        let Ok(out) = out else { return Vec::new() };
        let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter(|u| u.starts_with(prefix) && u.ends_with(".service"))
            .map(str::to_string)
            .collect();
        for u in &names {
            let _ = std::process::Command::new("systemctl")
                .args(["stop", u])
                .env_clear()
                .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
                .status();
        }
        names
    }

    fn start_containers(&self, ids: &[String]) -> Result<(), String> {
        let st = std::process::Command::new("docker")
            .arg("start")
            .args(ids)
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .output()
            .map_err(|e| e.to_string())?;
        if st.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&st.stderr).trim().to_string())
        }
    }

    fn destroy_snapshot(&self, path: &FsPath) -> Result<(), String> {
        let (parent, name) = (path.parent().ok_or("no parent")?, path.file_name().ok_or("no name")?);
        let d = rynas_btrfs::ioctl::open_dir(parent).map_err(|e| e.to_string())?;
        rynas_btrfs::ioctl::destroy_subvolume(&d, &name.to_string_lossy()).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir(parent);
        Ok(())
    }
}

/// Whether `path` is one of `job`'s temporary snapshots (`…/.rynas-backup/<job>-<n>`):
/// recovery never deletes anything else.
fn is_temp_snapshot(path: &FsPath, job: &str) -> bool {
    let parent_ok = path
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == ".rynas-backup");
    let name_ok = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix(job)?.strip_prefix('-'))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    path.is_absolute() && parent_ok && name_ok
}

/// Puts back what runs cut short by a rynasd stop left: worker units stopped, Docker
/// containers started, temporary snapshots deleted, downloads in flight removed. The lines
/// say what was done (for the log).
pub fn recover_with(state_dir: &FsPath, host: &dyn RecoverHost) -> Vec<String> {
    let mut log = Vec::new();
    for u in host.stop_units(rynas_backup::runner::UNIT_PREFIX) {
        log.push(format!("leftover worker unit {u} stopped"));
    }
    let _ = std::fs::remove_dir_all(dl_dir(state_dir));
    let dir = status::dir(state_dir);
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let Some(job) = n.strip_suffix(".json") else { continue };
        if !rynas_backup::valid_job_name(job) {
            continue;
        }
        let mut st = status::read(state_dir, job);
        let Some(run) = st.run.take() else { continue };
        if !run.stopped_containers.is_empty() {
            match host.start_containers(&run.stopped_containers) {
                Ok(()) => log.push(format!("{job}: {} stopped containers started again", run.stopped_containers.len())),
                Err(e) => log.push(format!("{job}: starting the stopped containers failed: {e}")),
            }
        }
        for p in &run.temp_snapshots {
            let p = FsPath::new(p);
            if !is_temp_snapshot(p, job) {
                log.push(format!("{job}: {} is not a temporary snapshot of the job, left alone", p.display()));
                continue;
            }
            if std::fs::symlink_metadata(p).is_err() {
                continue;
            }
            match host.destroy_snapshot(p) {
                Ok(()) => log.push(format!("{job}: temporary snapshot {} deleted", p.display())),
                Err(e) => log.push(format!("{job}: temporary snapshot {} not deleted: {e}", p.display())),
            }
        }
        let _ = std::fs::remove_dir_all(dir.join(format!("config-{job}")));
        if st.last_end_ms.is_none_or(|t| t < run.started_ms) {
            st.last_end_ms = Some(rynas_tasks::now_ms());
            st.last_error = Some("interrupted: rynasd stopped during the run".into());
        }
        if let Err(e) = status::write(state_dir, job, &st) {
            log.push(format!("{job}: status not written: {e}"));
        }
    }
    log
}

/// At rynasd's start: [`recover_with`] on the real system (not in `--dry-system`, which
/// never stops containers nor takes snapshots), then the files of removed jobs.
pub async fn recover(s: Arc<ResticSvc>) {
    let s2 = s.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if !s2.dry {
            for l in recover_with(&s2.prot.state_dir, &HostRecover) {
                tracing::warn!(target: "rynas::audit", "backup recovery: {l}");
            }
        } else {
            let _ = std::fs::remove_dir_all(dl_dir(&s2.prot.state_dir));
        }
        sweep(&s2);
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert!(check_snapshot_id("latest").is_ok());
        assert!(check_snapshot_id("0123abcd").is_ok());
        assert!(check_snapshot_id("../x").is_err());
        assert!(check_inner_path("/a/../b").is_err());
        assert!(check_inner_path("/volume1/docs/a b.txt").is_ok());
        let r = TestRequest {
            destination: Destination::Local { path: "/x/y".into() },
            secrets: DestinationSecrets {
                password: "ssh-ed25519 AAAA x".into(),
                rclone_config: String::new(),
            },
            job: None,
            repository_password: String::new(),
        };
        assert_eq!(req_public_key(&r), Some("ssh-ed25519 AAAA x"));
    }

    #[test]
    fn encrypted_folders_are_backed_up_as_ciphertext() {
        let d = tempfile::tempdir().unwrap();
        let place = d.path().to_string_lossy().into_owned();
        let mount = format!("{place}/secret");
        let cipher = format!("{place}/.rynas-crypt/secret");
        std::fs::create_dir_all(&mount).unwrap();
        std::fs::create_dir_all(&cipher).unwrap();
        // a plain folder is read as it is
        assert_eq!(backup_path(&format!("{place}/plain"), false).unwrap(), format!("{place}/plain"));
        // an encrypted share without its ciphertext fails: never an empty snapshot
        let e = backup_path(&mount, true).unwrap_err();
        assert!(e.contains("missing"), "{e}");
        std::fs::write(format!("{cipher}/gocryptfs.conf"), "{}").unwrap();
        assert_eq!(backup_path(&mount, true).unwrap(), cipher);
        // a folder source on the mount point of an encrypted folder too
        assert_eq!(backup_path(&mount, false).unwrap(), cipher);
    }

    #[test]
    fn repository_lock_per_key_and_own_release() {
        let mut r = HashMap::new();
        // two jobs on one repository: the second is refused
        try_reserve(&mut r, "s3:/b/nas1", "docs", 1).unwrap();
        let e = try_reserve(&mut r, "s3:/b/nas1", "photos", 2).unwrap_err();
        assert!(e.contains("in use by job docs"), "{e}");
        assert!(try_reserve(&mut r, "s3:/b/nas1", "docs", 3).unwrap_err().contains("is busy"));
        // another repository is free
        try_reserve(&mut r, "s3:/b/nas2", "photos", 4).unwrap();
        // a refused request never frees the holder's slot
        release(&mut r, "s3:/b/nas1", 2);
        assert!(r.contains_key("s3:/b/nas1"));
        release(&mut r, "s3:/b/nas1", 1);
        assert!(!r.contains_key("s3:/b/nas1"));
    }

    struct FakeHost {
        calls: Mutex<Vec<String>>,
    }

    impl RecoverHost for FakeHost {
        fn stop_units(&self, prefix: &str) -> Vec<String> {
            self.calls.lock().unwrap().push(format!("stop {prefix}*"));
            vec![format!("{prefix}docs-1-0.service")]
        }
        fn start_containers(&self, ids: &[String]) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("start {}", ids.join(" ")));
            Ok(())
        }
        fn destroy_snapshot(&self, path: &FsPath) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("destroy {}", path.display()));
            std::fs::remove_dir(path).map_err(|e| e.to_string())
        }
    }

    #[test]
    fn recovery_after_a_stop_mid_run() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path().join("state");
        let snap = t.path().join("volume1/.rynas-backup/docs-0");
        std::fs::create_dir_all(&snap).unwrap();
        let foreign = t.path().join("volume1/docs");
        std::fs::create_dir_all(&foreign).unwrap();
        let st = BackupStatus {
            last_ok_ms: Some(1),
            run: Some(RunState {
                started_ms: 10,
                unit: Some("rynas-backup-docs-1-0.service".into()),
                stopped_containers: vec!["db".into(), "app".into()],
                temp_snapshots: vec![
                    snap.display().to_string(),
                    // recorded but never created
                    t.path().join("volume1/.rynas-backup/docs-1").display().to_string(),
                    // never deleted: not a temporary snapshot of the job
                    foreign.display().to_string(),
                ],
            }),
            ..Default::default()
        };
        status::write(&state, "docs", &st).unwrap();
        status::write(&state, "idle", &BackupStatus::default()).unwrap();
        std::fs::create_dir_all(dl_dir(&state).join("dl-x")).unwrap();
        let host = FakeHost { calls: Mutex::new(Vec::new()) };
        let log = recover_with(&state, &host);
        let calls = host.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                "stop rynas-backup-*".to_string(),
                "start db app".into(),
                format!("destroy {}", snap.display()),
            ]
        );
        assert!(!snap.exists() && foreign.exists());
        assert!(log.iter().any(|l| l.contains("left alone")), "{log:?}");
        assert!(!dl_dir(&state).exists());
        let after = status::read(&state, "docs");
        assert!(after.run.is_none());
        assert_eq!(after.last_ok_ms, Some(1));
        assert!(after.last_error.unwrap().contains("interrupted"));
        // nothing left to do the second time
        let host = FakeHost { calls: Mutex::new(Vec::new()) };
        recover_with(&state, &host);
        assert_eq!(host.calls.lock().unwrap().len(), 1);
        assert!(is_temp_snapshot(FsPath::new("/v/.rynas-backup/docs-12"), "docs"));
        assert!(!is_temp_snapshot(FsPath::new("/v/.rynas-backup/docs-a"), "docs"));
        assert!(!is_temp_snapshot(FsPath::new("/v/x/docs-1"), "docs"));
        assert!(!is_temp_snapshot(FsPath::new("/v/.rynas-backup/docsx-1"), "docs"));
    }

    #[test]
    fn restore_targets_and_in_place_move() {
        let t = tempfile::tempdir().unwrap();
        let vol = t.path().join("volume1");
        std::fs::create_dir_all(vol.join("docs/sub")).unwrap();
        std::fs::create_dir_all(t.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(t.path().join("elsewhere"), vol.join("docs/link")).unwrap();
        let roots = vec![vol.display().to_string()];
        let v = |p: &str| format!("{}/{p}", vol.display());
        assert!(check_target(&v("restore/new"), &roots).is_ok());
        assert!(check_target(&v("docs/sub"), &roots).is_ok());
        // a symlink in any component, the root itself, outside the roots, system paths
        assert!(check_target(&v("docs/link/x"), &roots).unwrap_err().contains("symbolic link"));
        assert!(check_target(&vol.display().to_string(), &roots).is_err());
        assert!(check_target("/rynas-config", &roots).is_err());
        assert!(check_target("/etc/x", &roots).is_err());
        assert!(check_target(&v("a/../b"), &roots).is_err());
        assert!(check_target(&v(".rynas-backup/docs-0"), &roots).is_err());
        assert!(check_target(&v("x\"y"), &roots).is_err());
        // in place: files replaced, folders merged, other files kept
        let dst = vol.join("docs");
        std::fs::write(dst.join("a.txt"), "new local").unwrap();
        std::fs::write(dst.join("keep.txt"), "kept").unwrap();
        std::fs::create_dir(dst.join("was-a-file")).unwrap();
        let staging = staging_for(&dst, 0).unwrap();
        assert!(staging.starts_with(&dst));
        let src = staging.join(dst.strip_prefix("/").unwrap());
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "restored").unwrap();
        std::fs::write(src.join("sub/b.txt"), "b").unwrap();
        std::fs::write(src.join("was-a-file"), "f").unwrap();
        let mut m = Moved::default();
        merge_move(&src, &dst, &mut m).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "restored");
        assert_eq!(std::fs::read_to_string(dst.join("sub/b.txt")).unwrap(), "b");
        assert_eq!(std::fs::read_to_string(dst.join("keep.txt")).unwrap(), "kept");
        assert!(dst.join("was-a-file").is_dir());
        assert_eq!(m.files, 2);
        assert_eq!(m.skipped.len(), 1);
        // a missing original: the staging folder goes next to it
        let gone = vol.join("docs/gone/deeper");
        let st2 = staging_for(&gone, 1).unwrap();
        assert_eq!(st2.parent().unwrap(), dst);
    }

    #[test]
    fn local_destination_mount_and_excludes_and_names() {
        let mi = "22 1 0:21 / / rw - btrfs /dev/md0 rw\n\
                  30 22 0:40 / /volume1 rw - btrfs /dev/sdb1 rw\n\
                  31 22 8:33 / /media/usb1 rw - ext4 /dev/sdc1 rw\n";
        assert_eq!(mount_point_of("/media/usb1/backup", mi), "/media/usb1");
        assert_eq!(mount_point_of("/media/usb2/backup", mi), "/");
        assert!(check_local_destination("/media/usb1/b", mi).is_ok());
        assert!(check_local_destination("/media/usb2/b", mi).unwrap_err().contains("system disk"));
        // the system volume's folder is a destination chosen on purpose (D-ROOTVOL); the
        // folder itself and the rest of the system disk are not
        assert!(check_local_destination("/srv/rynas/system/backup", mi).is_ok());
        assert!(check_local_destination("/srv/rynas/system", mi).is_err());
        assert!(check_local_destination("/srv/backup", mi).is_err());
        assert_eq!(
            restore_roots(mi),
            vec!["/media/usb1".to_string(), "/srv/rynas/system".into(), "/volume1".into()]
        );
        // anchored excludes follow the snapshot path
        let ex = vec!["*.tmp".to_string(), "/volume1/docs/cache".into(), "/volume1/docsx/a".into()];
        assert_eq!(
            anchored_to(&ex, "/volume1/docs", "/volume1/.rynas-backup/docs-0"),
            vec![
                "*.tmp".to_string(),
                "/volume1/.rynas-backup/docs-0/cache".into(),
                "/volume1/docsx/a".into()
            ]
        );
        assert_eq!(anchored_to(&ex, "/volume1/docs", "/volume1/docs"), ex);
        assert_eq!(content_disposition("a b.txt"), "attachment; filename=\"a b.txt\"");
        assert_eq!(
            content_disposition("perché.txt"),
            "attachment; filename=\"perch_.txt\"; filename*=UTF-8''perch%C3%A9.txt"
        );
        // no nested subvolume outside btrfs
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("a/b")).unwrap();
        assert_eq!(child_subvolumes(t.path(), 10), (vec![], false));
        assert!(child_subvolumes(t.path(), 1).1);
    }

    #[test]
    fn kept_and_changed_secrets() {
        let job = |d: Destination, pk: &str| BackupJob {
            name: "j".into(),
            description: String::new(),
            sources: vec![BackupSource::Config],
            destination: d,
            retention: Retention::SMART,
            maintenance: Maintenance::default(),
            excludes: vec![],
            use_snapshot: true,
            stop_stacks: vec![],
            bandwidth_kib: 0,
            sealed: Some("x".into()),
            ssh_public_key: pk.into(),
        };
        let sec = JobSecrets {
            repository_password: "repo".into(),
            password: "s3secret".into(),
            rclone_config: "[r]".into(),
            ssh_private_key: "KEY".into(),
            ssh_known_hosts: "nas2 ssh-ed25519 A".into(),
        };
        let s3 = |b: &str| Destination::S3 {
            endpoint: String::new(),
            region: String::new(),
            bucket: b.into(),
            prefix: String::new(),
            access_key_id: "AK".into(),
        };
        // same destination: everything kept
        let k = kept_secrets(&job(s3("b"), ""), &job(s3("b"), ""), sec.clone());
        assert_eq!(k.password, "s3secret");
        // another bucket: the secret key is not sent there
        let k = kept_secrets(&job(s3("b"), ""), &job(s3("other"), ""), sec.clone());
        assert!(k.password.is_empty());
        assert_eq!(k.repository_password, "repo");
        let sftp = |h: &str| Destination::Sftp {
            host: h.into(),
            port: 22,
            user: "u".into(),
            path: "/p".into(),
        };
        let k = kept_secrets(&job(sftp("nas2"), "pk"), &job(sftp("NAS2"), "pk"), sec.clone());
        assert_eq!((k.ssh_private_key.as_str(), k.ssh_known_hosts.as_str()), ("KEY", "nas2 ssh-ed25519 A"));
        let k = kept_secrets(&job(sftp("nas2"), "pk"), &job(sftp("nas3"), "pk"), sec.clone());
        assert!(k.ssh_known_hosts.is_empty() && k.ssh_private_key == "KEY");
        // a refreshed rclone token applies only while the job still holds the old one
        let mut ov = std::collections::BTreeMap::new();
        ov.insert("rclone_config".to_string(), "[r] new token".to_string());
        ov.insert("rclone_config.base".to_string(), "[r]".to_string());
        assert_eq!(apply_overlay(sec.clone(), &ov).rclone_config, "[r] new token");
        let mut saved = sec.clone();
        saved.rclone_config = "[r] typed by the user".into();
        assert_eq!(apply_overlay(saved, &ov).rclone_config, "[r] typed by the user");
        // redaction of the run's error: every secret, however short
        let e = rynas_api_types::backup::redact_secrets("failed: s3secret repo", &sec.values());
        assert_eq!(e, "failed: *** ***");
    }
}
