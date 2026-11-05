//! Snapshot endpoints of the internal API (`/v1/snapshots/...`, public under
//! `/api/v1/snapshots/...`).
//!
//! Jobs are configuration: creating one stages, in the session user's changeset, the job
//! (`snapshots`), its scheduler task `snapshot-<job>` (`scheduler`) and, for a share with SMB,
//! the share's "previous versions" pointing at the job's snapshots (`shares`). Listing,
//! browsing and previews read the system; restore, rollback and delete are tasks.

use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::scheduler::{
    NotifyPolicy, ScheduledTask, SchedulerConfig, TaskAction, TaskTrigger,
};
use rynas_api_types::snapshots::{
    RestoreRequest, RetentionPreview, RetentionPreviewRequest, RollbackPreview, RollbackRefusal,
    RollbackRequest, SnapshotInfo, SnapshotJob, SnapshotJobRequest, SnapshotJobView,
    SnapshotListing, SnapshotsConfig, SystemSnapshots,
};
use rynas_api_types::{SharesConfig, StagedChanges, TaskStarted, error_codes};
use rynas_btrfs::btrbk::{conf_path, parse_report, render_conf};
use rynas_btrfs::domain::normalize_job;
use rynas_btrfs::name::split_snapshot_name;
use rynas_btrfs::scan::{self, JobLock, lock_path};
use rynas_btrfs::{SNAPSHOTS_KEY, browse, ioctl, restore, rootfs, system};
use rynas_scheduler::SCHEDULER_KEY;
use rynas_scheduler::units::timer_name;
use rynas_services::SHARES_KEY;
use rynas_tasks::Cmd;
use serde::Deserialize;

use crate::error::created;
use crate::ops::{Error, Res, user};
use crate::protection::{ProtState, Protection, blocking, conflict, invalid, not_found, prot};
use crate::scheduler::check_task;
use crate::services::{docs, from_value, pending_op, stage};

pub fn router(st: ProtState) -> Router {
    Router::new()
        .route("/v1/snapshots", get(list))
        .route("/v1/snapshots/system", get(system_get).put(system_put))
        .route("/v1/snapshots/jobs", get(jobs).post(job_create))
        .route(
            "/v1/snapshots/jobs/preview-retention",
            post(preview_retention),
        )
        .route(
            "/v1/snapshots/jobs/{name}",
            get(job_get).put(job_put).delete(job_delete),
        )
        .route("/v1/snapshots/{id}", axum::routing::delete(snapshot_delete))
        .route("/v1/snapshots/{id}/browse", get(browse_dir))
        .route("/v1/snapshots/{id}/restore", post(restore_one))
        .route("/v1/snapshots/{id}/rollback", post(rollback))
        .route("/v1/snapshots/{id}/preview-rollback", get(preview_rollback))
        .with_state(st)
}

fn json_body<T>(b: Result<Json<T>, JsonRejection>) -> Res<T> {
    b.map(|Json(v)| v).map_err(Error::from)
}

/// The scheduler task of a job.
pub fn task_id_of(job: &str) -> String {
    format!("snapshot-{}", job.to_ascii_lowercase().replace('_', "-"))
}

fn default_schedule() -> TaskTrigger {
    TaskTrigger::Calendar {
        on_calendar: "hourly".into(),
        cron: None,
        persistent: true,
        randomized_delay_s: 0,
    }
}

fn check_name(n: &str) -> Res<()> {
    if rynas_btrfs::domain::valid_job_name(n) {
        Ok(())
    } else {
        Err(Error::bad_request("invalid job name"))
    }
}

// ---------------------------------------------------------------------------------------------
// jobs
// ---------------------------------------------------------------------------------------------

fn job_views(
    p: &Protection,
    owner: &str,
    rt: &crate::protection::Runtime,
) -> Res<Vec<SnapshotJobView>> {
    let c = &p.ops.config;
    let (eff, cur, _): (SnapshotsConfig, SnapshotsConfig, Option<SnapshotsConfig>) =
        docs(c, owner, SNAPSHOTS_KEY)?;
    let (sched, _, applied_sched): (SchedulerConfig, SchedulerConfig, Option<SchedulerConfig>) =
        docs(c, owner, SCHEDULER_KEY)?;
    let latest = p.history.latest().map_err(Error::internal)?;
    let view = |j: &SnapshotJob, pending| {
        let tid = task_id_of(&j.name);
        let task = sched.tasks.iter().find(|t| t.id == tid);
        let applied = applied_sched
            .as_ref()
            .is_some_and(|a| a.tasks.iter().any(|t| t.id == tid));
        let names = scan::names(FsPath::new(&j.snapshot_dir), &j.name).unwrap_or_default();
        let latest_ms = names
            .first()
            .and_then(|n| split_snapshot_name(n))
            .and_then(|(_, ts)| rynas_btrfs::name::parse_timestamp(ts))
            .map(|s| s.epoch_ms().max(0) as u64);
        SnapshotJobView {
            schedule: task.map(|t| t.trigger.clone()),
            enabled: task.is_some_and(|t| t.enabled),
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
            task_id: tid,
            pending,
            config_path: conf_path(&j.name).display().to_string(),
            snapshots: names.len() as u32,
            latest_ms,
            job: j.clone(),
        }
    };
    let mut out: Vec<_> = eff
        .jobs
        .iter()
        .map(|j| {
            view(
                j,
                pending_op(Some(j), cur.jobs.iter().find(|x| x.name == j.name)),
            )
        })
        .collect();
    for j in cur
        .jobs
        .iter()
        .filter(|j| !eff.jobs.iter().any(|x| x.name == j.name))
    {
        out.push(view(j, pending_op(None, Some(j))));
    }
    Ok(out)
}

async fn job_view(p: &Arc<Protection>, owner: String, name: String) -> Res<SnapshotJobView> {
    let rt = p.runtime().await;
    blocking(p, move |p| {
        job_views(p, &owner, &rt)?
            .into_iter()
            .find(|v| v.job.name == name)
            .ok_or_else(|| not_found(format!("snapshot job {name}")))
    })
    .await
}

async fn jobs(State(st): State<ProtState>, h: HeaderMap) -> Res<Json<Vec<SnapshotJobView>>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    let rt = p.runtime().await;
    Ok(Json(
        blocking(&p, move |p| job_views(p, &owner, &rt)).await?,
    ))
}

async fn job_get(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Res<Json<SnapshotJobView>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    check_name(&name)?;
    Ok(Json(job_view(&p, owner, name).await?))
}

/// The three documents a job touches, as the user sees them.
#[derive(Clone)]
struct Docs {
    snaps: SnapshotsConfig,
    sched: SchedulerConfig,
    shares: SharesConfig,
}

fn load(p: &Protection, owner: &str) -> Res<Docs> {
    let c = &p.ops.config;
    let (snaps, _, _) = docs(c, owner, SNAPSHOTS_KEY)?;
    let (sched, _, _) = docs(c, owner, SCHEDULER_KEY)?;
    let (shares, _, _) = docs(c, owner, SHARES_KEY)?;
    Ok(Docs {
        snaps,
        sched,
        shares,
    })
}

/// Stages the documents that changed since [`load`] (an untouched key is never staged: a
/// default document would otherwise replace a configuration never applied by rynas).
fn save(p: &Protection, owner: &str, d: &Docs, orig: &Docs) -> Res<u32> {
    let c = &p.ops.config;
    let mut n = 0;
    if d.snaps != orig.snaps {
        stage(c, owner, SNAPSHOTS_KEY, &d.snaps)?;
        n += 1;
    }
    if d.sched != orig.sched {
        stage(c, owner, SCHEDULER_KEY, &d.sched)?;
        n += 1;
    }
    if d.shares != orig.shares {
        stage(c, owner, SHARES_KEY, &d.shares)?;
        n += 1;
    }
    // a job, its scheduler task and the share's previous versions go live together
    if n > 1 {
        c.stage_group(owner, &[SNAPSHOTS_KEY, SCHEDULER_KEY, SHARES_KEY], false)?;
    }
    Ok(n)
}

/// Fills the subvolume from the share and normalizes the job.
fn resolve_job(d: &Docs, mut job: SnapshotJob) -> Res<SnapshotJob> {
    if let Some(s) = &job.share {
        let share = d
            .shares
            .shares
            .iter()
            .find(|x| x.name.eq_ignore_ascii_case(s))
            .ok_or_else(|| invalid(format!("there is no share {s}")))?;
        job.share = Some(share.name.clone());
        if job.subvolume.is_empty() {
            job.subvolume = share.path.clone();
        }
    }
    normalize_job(job).map_err(invalid)
}

/// SMB previous versions of a share follow its job: on with the job's directory and names.
/// Set whether SMB is on or not: the snapshot folder of the share uses the same directory.
fn wire_shadow_copy(d: &mut Docs, job: &SnapshotJob) {
    let Some(name) = &job.share else { return };
    if let Some(sh) = d.shares.shares.iter_mut().find(|s| &s.name == name) {
        sh.smb.shadow_copy.enabled = true;
        sh.smb.shadow_copy.snapshot_dir = job.snapshot_dir.clone();
        sh.smb.shadow_copy.prefix = job.name.clone();
    }
}

/// Undoes [`wire_shadow_copy`] when the share still points at this job.
fn unwire_shadow_copy(d: &mut Docs, job: &SnapshotJob) {
    for sh in &mut d.shares.shares {
        let sc = &mut sh.smb.shadow_copy;
        if sc.snapshot_dir == job.snapshot_dir && (sc.prefix == job.name || sc.prefix.is_empty()) {
            sc.enabled = false;
            sc.snapshot_dir.clear();
            sc.prefix.clear();
        }
    }
}

fn job_task(
    p: &Protection,
    owner: &str,
    job: &SnapshotJob,
    schedule: TaskTrigger,
    enabled: bool,
) -> Res<ScheduledTask> {
    check_task(
        p,
        owner,
        ScheduledTask {
            id: task_id_of(&job.name),
            name: format!("Snapshots of {}", job.share.as_deref().unwrap_or(&job.name)),
            description: String::new(),
            enabled,
            action: TaskAction::Snapshot {
                job: job.name.clone(),
            },
            trigger: schedule,
            timeout_s: None,
            nice: None,
            io_weight: None,
            notify: NotifyPolicy::OnFailure,
            notify_output: false,
        },
        Some(&job.name),
    )
}

async fn job_create(
    State(st): State<ProtState>,
    h: HeaderMap,
    body: Result<Json<SnapshotJobRequest>, JsonRejection>,
) -> Res<Response> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    let req = json_body(body)?;
    let o = owner.clone();
    let name = blocking(&p, move |p| {
        let mut d = load(p, &o)?;
        let orig = d.clone();
        let job = resolve_job(&d, req.job)?;
        if d.snaps
            .jobs
            .iter()
            .any(|j| j.name.eq_ignore_ascii_case(&job.name))
        {
            return Err(conflict(
                error_codes::ALREADY_EXISTS,
                format!("snapshot job {} already exists", job.name),
            ));
        }
        let tid = task_id_of(&job.name);
        if d.sched.tasks.iter().any(|t| t.id == tid) {
            return Err(conflict(
                error_codes::ALREADY_EXISTS,
                format!("scheduler task {tid} already exists"),
            ));
        }
        d.snaps.jobs.push(job.clone());
        let task = job_task(
            p,
            &o,
            &job,
            req.schedule.unwrap_or_else(default_schedule),
            req.enabled.unwrap_or(true),
        )?;
        d.sched.tasks.push(task);
        wire_shadow_copy(&mut d, &job);
        save(p, &o, &d, &orig)?;
        Ok(job.name)
    })
    .await?;
    let loc = name.clone();
    let v = job_view(&p, owner, name).await?;
    Ok(created("/api/v1/snapshots/jobs", &loc, v))
}

async fn job_put(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<SnapshotJobRequest>, JsonRejection>,
) -> Res<Json<SnapshotJobView>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    check_name(&name)?;
    let mut req = json_body(body)?;
    if req.job.name.is_empty() {
        req.job.name = name.clone();
    }
    if req.job.name != name {
        return Err(invalid("the job name cannot change"));
    }
    let (o, n) = (owner.clone(), name.clone());
    blocking(&p, move |p| {
        let mut d = load(p, &o)?;
        let orig = d.clone();
        let old = d
            .snaps
            .jobs
            .iter()
            .find(|j| j.name == n)
            .cloned()
            .ok_or_else(|| not_found(format!("snapshot job {n}")))?;
        let job = resolve_job(&d, req.job)?;
        let tid = task_id_of(&n);
        let cur_task = d.sched.tasks.iter().find(|t| t.id == tid).cloned();
        let schedule = req
            .schedule
            .or_else(|| cur_task.as_ref().map(|t| t.trigger.clone()))
            .unwrap_or_else(default_schedule);
        let enabled = req
            .enabled
            .or(cur_task.as_ref().map(|t| t.enabled))
            .unwrap_or(true);
        let task = job_task(p, &o, &job, schedule, enabled)?;
        for j in &mut d.snaps.jobs {
            if j.name == n {
                *j = job.clone();
            }
        }
        d.sched.tasks.retain(|t| t.id != tid);
        d.sched.tasks.push(task);
        unwire_shadow_copy(&mut d, &old);
        wire_shadow_copy(&mut d, &job);
        save(p, &o, &d, &orig)?;
        Ok(())
    })
    .await?;
    Ok(Json(job_view(&p, owner, name).await?))
}

async fn job_delete(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Res<Json<StagedChanges>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    check_name(&name)?;
    let staged = blocking(&p, move |p| {
        let mut d = load(p, &owner)?;
        let orig = d.clone();
        let job = d
            .snaps
            .jobs
            .iter()
            .find(|j| j.name == name)
            .cloned()
            .ok_or_else(|| not_found(format!("snapshot job {name}")))?;
        d.snaps.jobs.retain(|j| j.name != name);
        let tid = task_id_of(&name);
        d.sched.tasks.retain(|t| t.id != tid);
        unwire_shadow_copy(&mut d, &job);
        save(p, &owner, &d, &orig)
    })
    .await?;
    Ok(Json(StagedChanges { staged }))
}

/// System restore points as the session user sees them.
async fn system_get(State(st): State<ProtState>, h: HeaderMap) -> Res<Json<SystemSnapshots>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    Ok(Json(
        blocking(&p, move |p| {
            let (eff, _, _): (SnapshotsConfig, SnapshotsConfig, _) =
                docs(&p.ops.config, &owner, SNAPSHOTS_KEY)?;
            Ok(eff.system)
        })
        .await?,
    ))
}

/// Stages the system restore point settings (the apt hook appears or goes on apply).
async fn system_put(
    State(st): State<ProtState>,
    h: HeaderMap,
    body: Result<Json<SystemSnapshots>, JsonRejection>,
) -> Res<Json<SystemSnapshots>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    let sys = json_body(body)?;
    Ok(Json(
        blocking(&p, move |p| {
            let c = &p.ops.config;
            let (mut eff, _, _): (SnapshotsConfig, SnapshotsConfig, _) =
                docs(c, &owner, SNAPSHOTS_KEY)?;
            eff.system = sys;
            stage(c, &owner, SNAPSHOTS_KEY, &eff)?;
            let (eff, _, _): (SnapshotsConfig, SnapshotsConfig, _) =
                docs(c, &owner, SNAPSHOTS_KEY)?;
            Ok(eff.system)
        })
        .await?,
    ))
}

/// What `btrbk prune -n` would delete now with the retention of `job` (saved or not).
async fn preview_retention(
    State(st): State<ProtState>,
    h: HeaderMap,
    body: Result<Json<RetentionPreviewRequest>, JsonRejection>,
) -> Res<Json<RetentionPreview>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    let req = json_body(body)?;
    Ok(Json(
        blocking(&p, move |p| {
            let d = load(p, &owner)?;
            let job = resolve_job(&d, req.job)?;
            let names = scan::names(FsPath::new(&job.snapshot_dir), &job.name).unwrap_or_default();
            if names.is_empty() {
                return Ok(RetentionPreview::default());
            }
            // the job's configuration without its lock: a preview never waits for a run
            let conf: String = render_conf(&job)
                .map_err(invalid)?
                .lines()
                .filter(|l| !l.starts_with("lockfile"))
                .map(|l| format!("{l}\n"))
                .collect();
            let dir = p.state_dir.join("tmp");
            std::fs::create_dir_all(&dir).map_err(Error::internal)?;
            let file = tempfile_in(&dir, &conf).map_err(Error::internal)?;
            let out = Cmd::new("btrbk")
                .arg("-c")
                .arg(&file)
                .args(["prune", "-n", "--format=raw"])
                .env_clear()
                .timeout(std::time::Duration::from_secs(60))
                .output_blocking();
            let _ = std::fs::remove_file(&file);
            let out = out.map_err(|e| {
                Error::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    error_codes::BACKEND_UNAVAILABLE,
                    format!("btrbk: {e}"),
                )
            })?;
            let r = parse_report(&out.stdout);
            if !out.success() || !r.errors.is_empty() {
                return Err(Error::internal(format!(
                    "btrbk prune: {} {}",
                    r.errors.join("; "),
                    out.stderr.trim()
                )));
            }
            Ok(RetentionPreview {
                keep: names
                    .iter()
                    .filter(|n| !r.deleted.contains(n))
                    .cloned()
                    .collect(),
                delete: names
                    .into_iter()
                    .filter(|n| r.deleted.contains(n))
                    .collect(),
            })
        })
        .await?,
    ))
}

/// A private file with `text` (mode 0600, unique name).
fn tempfile_in(dir: &FsPath, text: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    for i in 0..100u32 {
        let p = dir.join(format!(
            "preview-{}-{}-{i}.conf",
            std::process::id(),
            rynas_tasks::now_ms()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&p)
        {
            Ok(mut f) => {
                f.write_all(text.as_bytes())?;
                return Ok(p);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no temporary name"))
}

// ---------------------------------------------------------------------------------------------
// snapshots on disk
// ---------------------------------------------------------------------------------------------

/// Where the snapshots of a job (or the system restore points) are.
struct Located {
    job: String,
    share: Option<String>,
    dir: PathBuf,
    /// The live subvolume (restore target).
    subvolume: PathBuf,
    system: bool,
}

fn applied_snapshots(p: &Protection) -> Res<SnapshotsConfig> {
    from_value(p.ops.config.applied(SNAPSHOTS_KEY)?)
}

fn sources(c: &SnapshotsConfig) -> Vec<Located> {
    let mut v: Vec<Located> = c
        .jobs
        .iter()
        .map(|j| Located {
            job: j.name.clone(),
            share: j.share.clone(),
            dir: PathBuf::from(&j.snapshot_dir),
            subvolume: PathBuf::from(&j.subvolume),
            system: false,
        })
        .collect();
    if c.system.enabled {
        v.push(Located {
            job: c.system.prefix.clone(),
            share: None,
            dir: PathBuf::from(&c.system.snapshot_dir),
            subvolume: PathBuf::from(&c.system.subvolume),
            system: true,
        });
    }
    v
}

fn locate(p: &Protection, id: &str) -> Res<Located> {
    if id.contains('/') || id.contains('\0') {
        return Err(Error::bad_request("invalid snapshot id"));
    }
    let (prefix, _) =
        split_snapshot_name(id).ok_or_else(|| Error::bad_request("invalid snapshot id"))?;
    sources(&applied_snapshots(p)?)
        .into_iter()
        .find(|l| l.job == prefix)
        .ok_or_else(|| not_found(format!("snapshot {id}")))
}

#[derive(Deserialize)]
struct ListQuery {
    share: Option<String>,
    job: Option<String>,
}

async fn list(
    State(st): State<ProtState>,
    Query(q): Query<ListQuery>,
) -> Res<Json<Vec<SnapshotInfo>>> {
    let p = prot(&st)?;
    Ok(Json(
        blocking(&p, move |p| {
            let reasons = system::reasons(&system::reasons_path(&p.state_dir));
            let mut out = Vec::new();
            for l in sources(&applied_snapshots(p)?) {
                if q.share
                    .as_ref()
                    .is_some_and(|s| l.share.as_ref() != Some(s))
                {
                    continue;
                }
                if q.job.as_ref().is_some_and(|j| *j != l.job) {
                    continue;
                }
                let snaps = scan::scan(&l.dir, &l.job).map_err(Error::internal)?;
                out.extend(snaps.into_iter().map(|s| SnapshotInfo {
                    reason: l.system.then(|| reasons.get(&s.name).cloned()).flatten(),
                    id: s.name,
                    job: l.job.clone(),
                    share: l.share.clone(),
                    path: s.path.display().to_string(),
                    created_ms: s.created_ms.max(0) as u64,
                    readonly: s.info.as_ref().is_some_and(|i| i.readonly),
                    uuid: s.info.as_ref().and_then(|i| i.uuid.clone()),
                    parent_uuid: s.info.as_ref().and_then(|i| i.parent_uuid.clone()),
                    received_uuid: s.info.as_ref().and_then(|i| i.received_uuid.clone()),
                    deletion: s.deletion,
                }));
            }
            out.sort_by_key(|s| std::cmp::Reverse(s.created_ms));
            Ok(out)
        })
        .await?,
    ))
}

#[derive(Deserialize)]
struct BrowseQuery {
    #[serde(default)]
    path: String,
}

async fn browse_dir(
    State(st): State<ProtState>,
    Path(id): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> Res<Json<SnapshotListing>> {
    let p = prot(&st)?;
    Ok(Json(
        blocking(&p, move |p| {
            let l = locate(p, &id)?;
            let item = browse::resolve(&l.dir.join(&id), &q.path)?;
            let (entries, truncated) = browse::list(&item)?;
            let path = browse::components(&q.path)?.join("/");
            Ok(SnapshotListing {
                id,
                path,
                entries,
                truncated,
            })
        })
        .await?,
    ))
}

fn started(id: String) -> Response {
    (StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response()
}

fn now_s() -> i64 {
    (rynas_tasks::now_ms() / 1000) as i64
}

async fn restore_one(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<RestoreRequest>, JsonRejection>,
) -> Res<Response> {
    let (p, who) = (prot(&st)?, user(&h)?);
    let req = json_body(body)?;
    let i = id.clone();
    let l = blocking(&p, move |p| locate(p, &i)).await?;
    browse::components(&req.path)?;
    if req.path.trim_matches('/').is_empty() {
        return Err(invalid(
            "choose a file or folder; the whole shared folder goes back with a rollback",
        ));
    }
    let task = p
        .ops
        .tasks
        .spawn("snapshot.restore", &who, move |ctx| async move {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                c.log(&format!(
                    "restoring {} from {id} ({:?})",
                    req.path, req.mode
                ));
                let r = restore::restore(
                    &l.dir.join(&id),
                    &l.subvolume,
                    &req.path,
                    req.mode,
                    now_s(),
                    &|| c.is_cancelled(),
                )
                .map_err(|e| e.to_string())?;
                if let Some(s) = &r.saved_as {
                    c.log(&format!("the current version was kept as {s}"));
                }
                c.log(&format!(
                    "restored as {}: {} files, {} folders, {} bytes{}",
                    r.restored_to,
                    r.files,
                    r.dirs,
                    r.bytes,
                    if r.skipped > 0 {
                        format!(", {} special entries skipped", r.skipped)
                    } else {
                        String::new()
                    }
                ));
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())?
        })?;
    Ok(started(task))
}

async fn rollback(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<RollbackRequest>, JsonRejection>,
) -> Res<Response> {
    let (p, who) = (prot(&st)?, user(&h)?);
    let req = json_body(body)?;
    if req.confirm != id {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            error_codes::CONFIRMATION_REQUIRED,
            "a rollback replaces the whole shared folder: confirm with the snapshot id".into(),
        ));
    }
    let i = id.clone();
    let (l, sys) = blocking(&p, move |p| {
        Ok((locate(p, &i)?, applied_snapshots(p)?.system))
    })
    .await?;
    if l.system {
        return root_rollback(&p, &who, id, l, sys).await;
    }
    // the snapshot folder of the share is mounted inside the subvolume being replaced: it
    // is unmounted first and mounted again on the new one
    let view = {
        let sv = l.subvolume.to_string_lossy().into_owned();
        blocking(&p, move |p| {
            let shares: SharesConfig = from_value(p.ops.config.applied(SHARES_KEY)?)?;
            Ok(shares
                .shares
                .iter()
                .find(|s| s.path == sv)
                .and_then(rynas_services::snapview::wanted)
                .map(|(path, _)| rynas_services::snapview::unit_name(&path)))
        })
        .await?
    };
    let ops = p.ops.clone();
    let task = p.ops.tasks.spawn("snapshot.rollback", &who, move |ctx| async move {
        let c = ctx.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let _lock = JobLock::acquire(&lock_path(&l.job)).map_err(|e| e.to_string())?;
            let sys = ops.config.system();
            if let Some(u) = &view {
                c.log(&format!("unmounting the snapshot folder ({u})"));
                sys.stop_unit(u)?;
            }
            c.log(&format!("rolling {} back to {id}", l.subvolume.display()));
            let r = restore::rollback(&l.dir.join(&id), &l.subvolume, now_s()).map_err(|e| e.to_string());
            if let Some(u) = &view
                && let Err(e) = sys.start_unit(u)
            {
                c.log(&format!("{u}: {e}"));
            }
            let r = r?;

            c.log(&format!(
                "done; the previous content is kept as {} next to it (delete it when no longer needed). NFS clients must mount the share again.",
                r.kept_as
            ));
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?
    })?;
    Ok(started(task))
}

/// The preview of a rollback to `l`/`id` (no side effect).
fn rollback_preview(
    p: &Protection,
    id: &str,
    l: &Located,
    sys: &SystemSnapshots,
) -> Res<RollbackPreview> {
    let path = l.dir.join(id);
    let snaps = scan::scan(&l.dir, &l.job).map_err(Error::internal)?;
    let snap = snaps
        .iter()
        .find(|s| s.name == id)
        .ok_or_else(|| not_found(format!("snapshot {id}")))?;
    let readonly = snap.info.as_ref().is_some_and(|i| i.readonly);
    if !l.system {
        let ok = snap.deletion != rynas_api_types::snapshots::SnapshotDeletion::NotSubvolume;
        return Ok(RollbackPreview {
            id: id.into(),
            allowed: ok,
            refusal: (!ok).then_some(RollbackRefusal::NotReadonly),
            subvolume: Some(l.subvolume.display().to_string()),
            ..Default::default()
        });
    }
    let ops = p.root.ops(sys, &p.state_dir);
    let k = rootfs::snapshot_kernel(&path);
    let refusal = if !ops.synodeb() {
        Some(RollbackRefusal::NotSynodeb)
    } else if !readonly {
        Some(RollbackRefusal::NotReadonly)
    } else if k.release.is_none() {
        Some(RollbackRefusal::NoKernel)
    } else if !k.modules {
        Some(RollbackRefusal::NoModules)
    } else {
        None
    };
    Ok(RollbackPreview {
        id: id.into(),
        system: true,
        allowed: refusal.is_none(),
        refusal,
        kernel: k.release,
        running_kernel: ops.running_kernel(),
        modules: k.modules,
        subvolume: None,
    })
}

async fn preview_rollback(
    State(st): State<ProtState>,
    Path(id): Path<String>,
) -> Res<Json<RollbackPreview>> {
    let p = prot(&st)?;
    Ok(Json(
        blocking(&p, move |p| {
            let l = locate(p, &id)?;
            let sys = applied_snapshots(p)?.system;
            rollback_preview(p, &id, &l, &sys)
        })
        .await?,
    ))
}

/// Whether the restore points directory is part of the root subvolume (then the restore
/// points move into the new root), not a subvolume or mount of its own.
fn points_inside_root(sys: &SystemSnapshots) -> bool {
    let dir = FsPath::new(&sys.snapshot_dir);
    if ioctl::is_mount_point(dir) {
        return false;
    }
    let uuid = |p: &FsPath| {
        ioctl::open_dir(p)
            .and_then(|f| ioctl::subvol_info(&f))
            .ok()
            .and_then(|i| i.uuid)
    };
    let (a, b) = (uuid(dir), uuid(FsPath::new(&sys.subvolume)));
    a.is_some() && a == b
}

/// Root rollback (synodeb): refused up front when the preview says so, then a task.
async fn root_rollback(
    p: &Arc<Protection>,
    who: &str,
    id: String,
    l: Located,
    sys: SystemSnapshots,
) -> Res<Response> {
    let (i, s2) = (id.clone(), sys.clone());
    let (pv, l) = blocking(p, move |p| Ok((rollback_preview(p, &i, &l, &s2)?, l))).await?;
    if let Some(r) = pv.refusal {
        let why = match r {
            RollbackRefusal::NotSynodeb => {
                "rolling the system back from rynas needs synodeb (synodeb-dom sync); on this system it is a console procedure"
            }
            RollbackRefusal::NoKernel => "the restore point has no kernel image",
            RollbackRefusal::NoModules => {
                "the restore point lacks /lib/modules of its kernel: it could not boot"
            }
            RollbackRefusal::NotReadonly => "not a read-only restore point",
        };
        return Err(conflict(error_codes::CONFLICT, why));
    }
    let ops = p.root.ops(&sys, &p.state_dir);
    let task = p.ops.tasks.spawn("snapshot.rollback", who, move |ctx| async move {
        let c = ctx.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let plan = rootfs::RootRollback {
                snapshot: l.dir.join(&id),
                snapshot_dir: l.dir.clone(),
                root: PathBuf::from(&sys.subvolume),
                move_points: points_inside_root(&sys),
                run_dir: PathBuf::from(rootfs::RUN_DIR),
                now_s: now_s(),
                id,
            };
            c.log(&format!(
                "rolling the system back to {} (kernel {} instead of {})",
                plan.id,
                pv.kernel.as_deref().unwrap_or("?"),
                pv.running_kernel.as_deref().unwrap_or("?")
            ));
            let r = rootfs::rollback_root(ops.as_ref(), &plan, &|m| c.log(m))?;
            c.log(&format!(
                "done: reboot now to start {} with kernel {}. Until then the current system keeps running; it is kept as restore point {} (and as {} at the top of the filesystem, delete it after the reboot).",
                plan.id, r.kernel, r.before, r.kept_as
            ));
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?
    })?;
    Ok(started(task))
}

#[derive(Deserialize)]
struct ConfirmQuery {
    confirm: Option<String>,
}

async fn snapshot_delete(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<ConfirmQuery>,
) -> Res<Response> {
    let (p, who) = (prot(&st)?, user(&h)?);
    if q.confirm.as_deref() != Some(id.as_str()) {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            error_codes::CONFIRMATION_REQUIRED,
            "deleting a snapshot needs ?confirm=<snapshot id>".into(),
        ));
    }
    let i = id.clone();
    let l = blocking(&p, move |p| {
        let l = locate(p, &i)?;
        // refuse at once what the task would refuse
        let snaps = scan::scan(&l.dir, &l.job).map_err(Error::internal)?;
        let s = snaps
            .iter()
            .find(|s| s.name == i)
            .ok_or_else(|| not_found(format!("snapshot {i}")))?;
        if s.deletion != rynas_api_types::snapshots::SnapshotDeletion::Allowed {
            return Err(conflict(
                error_codes::CONFLICT,
                format!("{i} is kept: {}", scan::refusal(s.deletion)),
            ));
        }
        Ok(l)
    })
    .await?;
    let task = p
        .ops
        .tasks
        .spawn("snapshot.delete", &who, move |ctx| async move {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                scan::delete(&l.dir, &l.job, &id, &lock_path(&l.job)).map_err(|e| e.to_string())?;
                c.log(&format!("deleted {id}"));
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())?
        })?;
    Ok(started(task))
}
