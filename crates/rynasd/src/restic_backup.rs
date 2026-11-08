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
//! - One worker operation per job at a time (the per-repository maintenance lock of
//!   D-BACKUP: never two prunes on one repository); reads (snapshots, browse) may run
//!   alongside, as restic allows.

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
use rynas_backup::{JobSecrets, Launch, job_tag, normalize_job, repo_spec, run_worker, status, task_id_of};
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

/// The backup service.
pub struct ResticSvc {
    pub prot: Arc<Protection>,
    pub launch: Launch,
    /// `--dry-system`: no btrfs snapshots, no Docker stops.
    pub dry: bool,
    /// `<state>/notify`: rynasd's age key.
    pub secret_dir: PathBuf,
    /// job → UPID of its running worker operation.
    running: Mutex<HashMap<String, String>>,
    /// public key line → (private key, created).
    keys: Mutex<HashMap<String, (String, Instant)>>,
}

pub type RsState = Option<Arc<ResticSvc>>;

/// The service, for the scheduler's internal action (`rynasctl job run backup-<job>`).
pub static SVC: OnceLock<Arc<ResticSvc>> = OnceLock::new();

impl ResticSvc {
    pub fn new(prot: Arc<Protection>, launch: Launch, dry: bool, secret_dir: PathBuf) -> Arc<Self> {
        Arc::new(ResticSvc {
            prot,
            launch,
            dry,
            secret_dir,
            running: Mutex::new(HashMap::new()),
            keys: Mutex::new(HashMap::new()),
        })
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

/// Merges a request's secrets into the stored ones; `creating`: a repository password is
/// generated when none is given (returned, to show once).
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
        Destination::Sftp { .. } => {
            if let Some(k) = take_key(s, &job.ssh_public_key) {
                sec.ssh_private_key = k;
            }
            if sec.ssh_private_key.is_empty() {
                return Err(invalid("SFTP: generate a key pair first"));
            }
        }
        _ => sec.ssh_private_key.clear(),
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
    let running = s.running.lock().unwrap_or_else(|p| p.into_inner()).clone();
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
            running_task: running.get(&j.name).cloned(),
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
    Ok(Json(blocking(&s.prot, move |_| job_views(&s2, &owner, &rt)).await?))
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
        let prev = unseal(&s2, &old)?;
        // a new destination does not inherit the old one's secrets
        let prev = if old.destination.kind() == job.destination.kind() {
            prev
        } else {
            JobSecrets {
                repository_password: prev.repository_password,
                ..Default::default()
            }
        };
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
    let spec = blocking(&s.prot, move |_| {
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
            if let Some(j) = b.jobs.iter().find(|j| &j.name == name) {
                sec = unseal(&s2, j)?;
            }
        }
        if !req.secrets.password.is_empty() {
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
        Ok(repo_spec(&dest, &sec, 0))
    })
    .await?;
    let wreq = WorkerRequest {
        repo: spec,
        op: WorkerOp::Test,
    };
    let r = run_worker(&s.launch, "test", &wreq, &CancellationToken::new(), Some(READ_TIMEOUT), |_| {}).await;
    Ok(Json(match r {
        Ok(WorkerEvent::Test { outcome, snapshots }) => TestResult {
            outcome,
            message: String::new(),
            snapshots,
        },
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
    let sec = unseal(s, &job)?;
    Ok((job, sec))
}

/// Reserves the job for one worker operation; the guard frees it.
struct Busy {
    svc: Arc<ResticSvc>,
    job: String,
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.svc
            .running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.job);
    }
}

/// Starts a task that holds the job's lock for its whole run.
fn spawn_locked<F, Fut>(s: &Arc<ResticSvc>, kind: &str, who: &str, job: &str, f: F) -> Res<String>
where
    F: FnOnce(TaskCtx) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    {
        let r = s.running.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(id) = r.get(job) {
            return Err(conflict(
                error_codes::BUSY,
                format!("backup job {job} is busy (task {id})"),
            ));
        }
    }
    let busy = Busy {
        svc: s.clone(),
        job: job.to_string(),
    };
    let s2 = s.clone();
    let j = job.to_string();
    let id = s.prot.ops.tasks.spawn(kind, who, move |ctx| async move {
        let _busy = busy;
        f(ctx).await
    })?;
    s2.running
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(j, id.clone());
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
    job: &str,
    spec: &RepoSpec,
    op: WorkerOp,
    ctx: &TaskCtx,
) -> Result<WorkerEvent, String> {
    let req = WorkerRequest {
        repo: spec.clone(),
        op,
    };
    run_worker(&s.launch, job, &req, ctx.token(), None, follow(ctx)).await
}

/// The scheduler's internal action and "Back up now".
pub fn spawn_run(who: &str, job: &str) -> Res<String> {
    let s = SVC
        .get()
        .cloned()
        .ok_or_else(|| Error::internal("backups are not enabled in this rynasd"))?;
    let (j, sec) = applied_job(&s, job)?;
    let s2 = s.clone();
    spawn_locked(&s, "backup.run", who, job, move |ctx| async move {
        let res = run_job(&s2, &j, &sec, &ctx).await;
        let mut st = status::read(&s2.prot.state_dir, &j.name);
        st.last_end_ms = Some(rynas_tasks::now_ms());
        match &res {
            Ok(()) => {
                st.last_ok_ms = st.last_end_ms;
                st.last_error = None;
            }
            Err(e) => st.last_error = Some(e.chars().take(500).collect()),
        }
        // the run's own fields were set as it went
        let mut cur = status::read(&s2.prot.state_dir, &j.name);
        cur.last_end_ms = st.last_end_ms;
        cur.last_ok_ms = st.last_ok_ms;
        cur.last_error = st.last_error;
        let _ = status::write(&s2.prot.state_dir, &j.name, &cur);
        res
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
                out.push((sh.path.clone(), None));
            }
            BackupSource::Folder { path } => out.push((path.clone(), None)),
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

async fn run_job(s: &Arc<ResticSvc>, job: &BackupJob, sec: &JobSecrets, ctx: &TaskCtx) -> Result<(), String> {
    let spec = repo_spec(&job.destination, sec, job.bandwidth_kib);
    ctx.log(&format!(
        "backup job {} to {} ({})",
        job.name,
        job.destination.summary(),
        job.destination.kind()
    ));
    if let WorkerEvent::Initialized { created: true } = op(s, &job.name, &spec, WorkerOp::Init, ctx).await? {
        ctx.log("repository created");
    }
    let state_dir = s.prot.state_dir.clone();
    let paths = source_paths(s, job)?;
    // the configuration export, encrypted with the repository password
    let cfg_dir = state_dir.join("backup").join(format!("config-{}", job.name));
    let mut sources = Vec::new();
    let mut snaps = Vec::new();
    let mut stopped: Vec<String> = Vec::new();
    if !s.dry {
        for st in &job.stop_stacks {
            let ids = stack_containers(st).await;
            if !ids.is_empty() {
                ctx.log(&format!("stopping stack {st}"));
                docker(&["stop"], &ids, ctx).await;
                stopped.extend(ids);
            }
        }
    }
    let prepared: Result<(), String> = async {
        for (n, (path, special)) in paths.iter().enumerate() {
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
                });
                continue;
            }
            let mut read = path.clone();
            if job.use_snapshot && !s.dry {
                let (p, j) = (path.clone(), job.name.clone());
                match tokio::task::spawn_blocking(move || snapshot_of(&p, &j, n)).await {
                    Ok(Ok(Some(t))) => {
                        read = t.parent.join(&t.name).display().to_string();
                        ctx.log(&format!("read-only snapshot of {path}"));
                        snaps.push(t);
                    }
                    Ok(Ok(None)) => ctx.log(&format!("{path}: not a btrfs subvolume, read as it is")),
                    Ok(Err(e)) => ctx.log(&format!("{path}: no snapshot ({e}), read as it is")),
                    Err(e) => return Err(e.to_string()),
                }
            }
            sources.push(WorkerSource {
                read,
                as_path: path.clone(),
            });
        }
        Ok(())
    }
    .await;
    // the stacks run again as soon as their data is frozen in a snapshot; without
    // snapshots they wait for the backup
    let restart_early = job.use_snapshot && !snaps.is_empty() && snaps.len() == paths.iter().filter(|p| p.1.is_none()).count();
    if restart_early && !stopped.is_empty() {
        ctx.log("starting the stacks again");
        docker(&["start"], &stopped, ctx).await;
        stopped.clear();
    }
    let result: Result<(), String> = async {
        prepared?;
        let backup = WorkerOp::Backup {
            sources: sources.clone(),
            tags: vec!["rynas".into(), job_tag(&job.name)],
            host: hostname(),
            excludes: job.excludes.clone(),
        };
        let mut st = status::read(&state_dir, &job.name);
        let (mut read_b, mut added_b) = (0, 0);
        let mut seen = std::collections::HashSet::new();
        let mut f = follow(ctx);
        let r = run_worker(
            &s.launch,
            &job.name,
            &WorkerRequest { repo: spec.clone(), op: backup },
            ctx.token(),
            None,
            {
                |ev: &WorkerEvent| {
                    // one event per source; the final result repeats the last one
                    if let WorkerEvent::BackedUp { snapshot } = ev {
                        if !seen.insert(snapshot.id.clone()) {
                            return;
                        }
                        read_b += snapshot.bytes.unwrap_or(0);
                        added_b += snapshot.added_bytes.unwrap_or(0);
                    }
                    f(ev)
                }
            },
        )
        .await;
        r?;
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
    result?;
    maintenance(s, job, &spec, ctx).await
}

/// Retention, then prune and check when due, then the repository size.
async fn maintenance(s: &ResticSvc, job: &BackupJob, spec: &RepoSpec, ctx: &TaskCtx) -> Result<(), String> {
    let state_dir = &s.prot.state_dir;
    let now = rynas_tasks::now_ms();
    if let WorkerEvent::Forgot { keep, remove } = op(
        s,
        &job.name,
        spec,
        WorkerOp::Forget {
            tag: job_tag(&job.name),
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
        if let WorkerEvent::Pruned { removed_packs, freed_bytes } = op(s, &job.name, spec, WorkerOp::Prune, ctx).await? {
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
    if let Ok(WorkerEvent::Stats { bytes: b, .. }) = op(s, &job.name, spec, WorkerOp::Stats, ctx).await {
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
    let r = op(s, &job.name, spec, WorkerOp::Check { read_percent, slice: None }, ctx).await?;
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
    let id = spawn_locked(&s, "backup.check", &owner, &name, move |ctx| async move {
        let spec = repo_spec(&job.destination, &sec, job.bandwidth_kib);
        check_into_status(&s2, &job, &spec, req.read_percent, &ctx).await
    })?;
    audit(&owner, &format!("check of {name} started"));
    Ok(started(id))
}

/// A read with the job's repository (not a task: answered directly).
async fn read_op(s: &Arc<ResticSvc>, name: &str, op: WorkerOp) -> Res<WorkerEvent> {
    let (job, sec) = {
        let (s2, n) = (s.clone(), name.to_string());
        blocking(&s.prot, move |_| applied_job(&s2, &n)).await?
    };
    let req = WorkerRequest {
        repo: repo_spec(&job.destination, &sec, job.bandwidth_kib),
        op,
    };
    run_worker(&s.launch, name, &req, &CancellationToken::new(), Some(READ_TIMEOUT), |_| {})
        .await
        .map_err(|e| Error::new(StatusCode::BAD_GATEWAY, error_codes::BACKEND_UNAVAILABLE, e))
}

async fn snapshots(State(st): State<RsState>, Path(name): Path<String>) -> Res<Json<Vec<SnapshotEntry>>> {
    let s = svc(&st)?;
    check_name(&name)?;
    match read_op(&s, &name, WorkerOp::Snapshots { tag: None }).await? {
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
    body: Result<Json<RestoreRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    check_name(&name)?;
    let req = json_body(body)?;
    check_snapshot_id(&req.snapshot)?;
    for p in &req.paths {
        check_inner_path(p)?;
    }
    let target = match &req.target {
        RestoreTarget::Original => None,
        RestoreTarget::Folder { path } => {
            let p = path.trim().trim_end_matches('/').to_string();
            if !rynas_backup::domain::clean_abs(&p) || p.matches('/').count() < 2 {
                return Err(invalid("restore into a folder below a volume, e.g. /volume1/restore"));
            }
            Some(p)
        }
    };
    let (job, sec) = applied_job(&s, &name)?;
    let s2 = s.clone();
    let (snap, paths) = (req.snapshot.clone(), req.paths.clone());
    let id = spawn_locked(&s, "backup.restore", &owner, &name, move |ctx| async move {
        let spec = repo_spec(&job.destination, &sec, job.bandwidth_kib);
        let r = op(
            &s2,
            &job.name,
            &spec,
            WorkerOp::Restore {
                snapshot: snap,
                paths,
                target,
            },
            &ctx,
        )
        .await?;
        if let WorkerEvent::Restored { files, bytes: b } = r {
            ctx.log(&format!("restored {files} files, {}", bytes(b)));
        }
        Ok(())
    })?;
    audit(&owner, &format!("restore from {name} started"));
    Ok(started(id))
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
    let dir = tempfile::tempdir_in(std::env::temp_dir()).map_err(Error::internal)?;
    let out = dir.path().join("file");
    match read_op(
        &s,
        &name,
        WorkerOp::Dump {
            snapshot: q.snapshot.clone(),
            path: q.path.clone(),
            out: out.display().to_string(),
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
    let data = tokio::fs::read(&out).await.map_err(Error::internal)?;
    let fname: String = q
        .path
        .rsplit('/')
        .next()
        .unwrap_or("file")
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .collect();
    let mut r = data.into_response();
    let hd = r.headers_mut();
    hd.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{fname}\"")) {
        hd.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(r)
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
            tag: job_tag(&name),
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
}
