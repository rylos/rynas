//! Snapshot replication endpoints (`/v1/snapshots/replication/...`, public under
//! `/api/v1/snapshots/replication/...`) and the transfer task.
//!
//! A target is part of its snapshot job (key `snapshots`), so adding, changing or removing
//! one stages the job and, for a target with its own schedule, its scheduler task
//! `replica-<job>-<id>` together. A transfer (`replication.run`) runs `btrbk clean` then
//! `btrbk resume --preserve-snapshots` for the target's group, records the outcome in
//! `<state>/replication/<job>/<id>.json` and notifies on state changes
//! (`replication.failed` / `replication.ok`, key `replication:<job>:<id>`). Browse, restore
//! and promote read local targets only.

use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
use rynas_api_types::replication::{
    HostKey, HostKeyRequest, PromoteRequest, ReplicaDestination, ReplicaEntry, ReplicaListing,
    ReplicaRequest, ReplicaRestoreRequest, ReplicaSnapshot, ReplicaTarget, ReplicaView,
    ReplicaWarning, ReplicationKey,
};
use rynas_api_types::scheduler::{
    NotifyPolicy, ScheduledTask, SchedulerConfig, TaskAction, TaskTrigger,
};
use rynas_api_types::snapshots::{RestoreMode, SnapshotJob, SnapshotsConfig};
use rynas_api_types::{StagedChanges, TaskStarted, error_codes};
use rynas_btrfs::replication::{
    self as rep, ReplicaHost, TransferReport, clean_cmd, read_status, resume_cmd, task_id,
    write_status,
};
use rynas_btrfs::{SNAPSHOTS_KEY, browse, restore};
use rynas_notify::event;
use rynas_scheduler::SCHEDULER_KEY;
use rynas_scheduler::units::timer_name;
use rynas_tasks::TaskCtx;
use serde::Deserialize;

use crate::error::created;
use crate::ops::{Error, Res, user};
use crate::protection::{ProtState, Protection, blocking, conflict, invalid, not_found, prot};
use crate::scheduler::check_task;
use crate::services::{docs, from_value, pending_op};
use crate::snapshots::{json_body, load, save};

pub fn router(st: ProtState) -> Router {
    Router::new()
        .route("/v1/snapshots/replication", get(list).post(create))
        .route(
            "/v1/snapshots/replication/ssh-key",
            get(key_get).post(key_create),
        )
        .route("/v1/snapshots/replication/host-keys", post(host_keys))
        .route(
            "/v1/snapshots/replication/{job}/{id}",
            get(one).put(update).delete(remove),
        )
        .route("/v1/snapshots/replication/{job}/{id}/run", post(run_now))
        .route(
            "/v1/snapshots/replication/{job}/{id}/snapshots",
            get(replicas),
        )
        .route(
            "/v1/snapshots/replication/{job}/{id}/snapshots/{name}/browse",
            get(browse_replica),
        )
        .route(
            "/v1/snapshots/replication/{job}/{id}/snapshots/{name}/restore",
            post(restore_replica),
        )
        .route(
            "/v1/snapshots/replication/{job}/{id}/snapshots/{name}/promote",
            post(promote_replica),
        )
        .with_state(st)
}

fn check_ids(job: &str, id: &str) -> Res<()> {
    if rynas_btrfs::domain::valid_job_name(job) && rep::valid_target_id(id) {
        Ok(())
    } else {
        Err(Error::bad_request("invalid job or target id"))
    }
}

fn default_schedule() -> TaskTrigger {
    TaskTrigger::Calendar {
        on_calendar: "daily".into(),
        cron: None,
        persistent: true,
        randomized_delay_s: 0,
    }
}

// ---------------------------------------------------------------------------------------------
// listing
// ---------------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn view_of(
    p: &Protection,
    job: &SnapshotJob,
    t: &ReplicaTarget,
    pending: Option<rynas_api_types::PendingOp>,
    sched: &SchedulerConfig,
    applied_sched: Option<&SchedulerConfig>,
    rt: &crate::protection::Runtime,
    key: &str,
) -> ReplicaView {
    let tid = task_id(&job.name, &t.id);
    let task = sched.tasks.iter().find(|x| x.id == tid);
    // after the snapshots: the job's own timer; else the target's
    let timer_task = if t.after_snapshots {
        crate::snapshots::task_id_of(&job.name)
    } else {
        tid.clone()
    };
    let applied = applied_sched.is_some_and(|a| a.tasks.iter().any(|x| x.id == timer_task));
    let next_run_ms = (applied && t.enabled)
        .then(|| {
            let timer = timer_name(&timer_task);
            rt.timers
                .iter()
                .find(|x| x.unit == timer)
                .and_then(|x| x.next_ms)
        })
        .flatten();
    let mut status = read_status(&p.state_dir, &job.name, &t.id);
    status.held_snapshots = rep::held_snapshots(
        &p.replicas.snapshot_names(&job.snapshot_dir),
        &job.name,
        status.latest.as_deref(),
    );
    if let Some(task) = task {
        status.last_run = p.history.latest().ok().and_then(|l| l.get(&task.id).cloned());
    }
    ReplicaView {
        job: job.name.clone(),
        share: job.share.clone(),
        url: t.destination.url(),
        task_id: task.map(|x| x.id.clone()),
        schedule: task.map(|x| x.trigger.clone()),
        pending,
        next_run_ms,
        status,
        warnings: rep::warnings(p.replicas.as_ref(), job, t),
        authorized_keys_line: rep::authorized_keys_line(key, t),
        target: t.clone(),
    }
}

fn views(p: &Protection, owner: &str, rt: &crate::protection::Runtime) -> Res<Vec<ReplicaView>> {
    let c = &p.ops.config;
    let (eff, cur, _): (SnapshotsConfig, SnapshotsConfig, Option<SnapshotsConfig>) =
        docs(c, owner, SNAPSHOTS_KEY)?;
    let (sched, _, applied_sched): (SchedulerConfig, SchedulerConfig, Option<SchedulerConfig>) =
        docs(c, owner, SCHEDULER_KEY)?;
    let key = p.replicas.public_key().unwrap_or_default();
    let find = |cfg: &SnapshotsConfig, j: &str, id: &str| -> Option<ReplicaTarget> {
        cfg.jobs
            .iter()
            .find(|x| x.name == j)
            .and_then(|x| x.replicas.iter().find(|t| t.id == id).cloned())
    };
    let mut out = Vec::new();
    for j in &eff.jobs {
        for t in &j.replicas {
            let base = find(&cur, &j.name, &t.id);
            out.push(view_of(
                p,
                j,
                t,
                pending_op(Some(t), base.as_ref()),
                &sched,
                applied_sched.as_ref(),
                rt,
                &key,
            ));
        }
    }
    // targets the pending changes remove
    for j in &cur.jobs {
        for t in &j.replicas {
            if find(&eff, &j.name, &t.id).is_none() {
                out.push(view_of(
                    p,
                    j,
                    t,
                    pending_op(None, Some(t)),
                    &sched,
                    applied_sched.as_ref(),
                    rt,
                    &key,
                ));
            }
        }
    }
    Ok(out)
}

async fn list(State(st): State<ProtState>, h: HeaderMap) -> Res<Json<Vec<ReplicaView>>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    let rt = p.runtime().await;
    Ok(Json(blocking(&p, move |p| views(p, &owner, &rt)).await?))
}

async fn view(p: &Arc<Protection>, owner: String, job: String, id: String) -> Res<ReplicaView> {
    let rt = p.runtime().await;
    blocking(p, move |p| {
        views(p, &owner, &rt)?
            .into_iter()
            .find(|v| v.job == job && v.target.id == id)
            .ok_or_else(|| not_found(format!("replica target {job}/{id}")))
    })
    .await
}

async fn one(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path((job, id)): Path<(String, String)>,
) -> Res<Json<ReplicaView>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    check_ids(&job, &id)?;
    Ok(Json(view(&p, owner, job, id).await?))
}

// ---------------------------------------------------------------------------------------------
// staging
// ---------------------------------------------------------------------------------------------

fn replica_task(
    p: &Protection,
    owner: &str,
    job: &SnapshotJob,
    t: &ReplicaTarget,
    trigger: TaskTrigger,
) -> Res<ScheduledTask> {
    check_task(
        p,
        owner,
        ScheduledTask {
            id: task_id(&job.name, &t.id),
            name: format!(
                "Replication of {} to {}",
                job.share.as_deref().unwrap_or(&job.name),
                t.id
            ),
            description: String::new(),
            enabled: t.enabled,
            action: TaskAction::Replicate {
                job: job.name.clone(),
                target: t.id.clone(),
            },
            trigger,
            timeout_s: None,
            nice: None,
            io_weight: None,
            // the transfer notifies by itself (replication.*)
            notify: NotifyPolicy::Never,
            notify_output: false,
        },
        None,
    )
}

/// Checks a target against the machine: a local target must be on another btrfs filesystem.
fn check_machine(p: &Protection, job: &SnapshotJob, t: &ReplicaTarget) -> Res<()> {
    let w = rep::warnings(p.replicas.as_ref(), job, t);
    if w.contains(&ReplicaWarning::SameFilesystem) {
        return Err(invalid(format!(
            "{} is on the filesystem of {}: a replica there is not a second copy",
            t.destination.path(),
            job.subvolume
        )));
    }
    if w.contains(&ReplicaWarning::NotBtrfs) {
        return Err(invalid(format!(
            "{} is not on btrfs: snapshot replication needs btrfs on the target (file backups of other filesystems are restic's)",
            t.destination.path()
        )));
    }
    Ok(())
}

/// Puts `t` in `job` (replacing the target with the same id), its task in or out of the
/// scheduler, validates the job and stages both keys.
fn stage_target(
    p: &Protection,
    owner: &str,
    req: ReplicaRequest,
    new: bool,
) -> Res<(String, String)> {
    let mut d = load(p, owner)?;
    let orig = d.clone();
    let ReplicaRequest {
        job: jname,
        target: t,
        schedule,
    } = req;
    let job = d
        .snaps
        .jobs
        .iter_mut()
        .find(|j| j.name == jname)
        .ok_or_else(|| invalid(format!("there is no snapshot job {jname}")))?;
    let exists = job.replicas.iter().any(|x| x.id == t.id);
    if new && exists {
        return Err(conflict(
            error_codes::ALREADY_EXISTS,
            format!("replica target {jname}/{} already exists", t.id),
        ));
    }
    if !new && !exists {
        return Err(not_found(format!("replica target {jname}/{}", t.id)));
    }
    let id = t.id.clone();
    match job.replicas.iter_mut().find(|x| x.id == id) {
        Some(x) => *x = t,
        None => job.replicas.push(t),
    }
    let normalized = rynas_btrfs::domain::normalize_job(job.clone()).map_err(invalid)?;
    *job = normalized.clone();
    let t = normalized
        .replicas
        .iter()
        .find(|x| x.id == id)
        .cloned()
        .expect("just added");
    check_machine(p, &normalized, &t)?;
    let tid = task_id(&jname, &id);
    let cur_task = d.sched.tasks.iter().find(|x| x.id == tid).cloned();
    if let Some(other) = &cur_task
        && !matches!(&other.action, TaskAction::Replicate { job, target } if *job == jname && *target == id)
    {
        return Err(conflict(
            error_codes::ALREADY_EXISTS,
            format!("scheduler task {tid} already exists"),
        ));
    }
    d.sched.tasks.retain(|x| x.id != tid);
    if !t.after_snapshots {
        let trigger = schedule
            .or_else(|| cur_task.map(|x| x.trigger))
            .unwrap_or_else(default_schedule);
        d.sched
            .tasks
            .push(replica_task(p, owner, &normalized, &t, trigger)?);
    }
    save(p, owner, &d, &orig)?;
    Ok((jname, id))
}

async fn create(
    State(st): State<ProtState>,
    h: HeaderMap,
    body: Result<Json<ReplicaRequest>, JsonRejection>,
) -> Res<Response> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    let req = json_body(body)?;
    let o = owner.clone();
    let (job, id) = blocking(&p, move |p| stage_target(p, &o, req, true)).await?;
    let v = view(&p, owner, job.clone(), id.clone()).await?;
    Ok(created(
        "/api/v1/snapshots/replication",
        &format!("{job}/{id}"),
        v,
    ))
}

async fn update(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path((job, id)): Path<(String, String)>,
    body: Result<Json<ReplicaRequest>, JsonRejection>,
) -> Res<Json<ReplicaView>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    check_ids(&job, &id)?;
    let mut req = json_body(body)?;
    if req.target.id.is_empty() {
        req.target.id = id.clone();
    }
    if req.job.is_empty() {
        req.job = job.clone();
    }
    if req.job != job || req.target.id != id {
        return Err(invalid("the job and the target id cannot change"));
    }
    let o = owner.clone();
    blocking(&p, move |p| stage_target(p, &o, req, false)).await?;
    Ok(Json(view(&p, owner, job, id).await?))
}

/// Removes a target from its job (and its task). The replicas on the target stay.
async fn remove(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path((job, id)): Path<(String, String)>,
) -> Res<Json<StagedChanges>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    check_ids(&job, &id)?;
    let staged = blocking(&p, move |p| {
        let mut d = load(p, &owner)?;
        let orig = d.clone();
        let j = d
            .snaps
            .jobs
            .iter_mut()
            .find(|j| j.name == job)
            .ok_or_else(|| not_found(format!("snapshot job {job}")))?;
        let before = j.replicas.len();
        j.replicas.retain(|t| t.id != id);
        if j.replicas.len() == before {
            return Err(not_found(format!("replica target {job}/{id}")));
        }
        let tid = task_id(&job, &id);
        d.sched.tasks.retain(|t| t.id != tid);
        save(p, &owner, &d, &orig)
    })
    .await?;
    Ok(Json(StagedChanges { staged }))
}

// ---------------------------------------------------------------------------------------------
// keys
// ---------------------------------------------------------------------------------------------

fn key_view(h: &dyn ReplicaHost, public_key: Option<String>) -> ReplicationKey {
    let public_key = public_key.unwrap_or_default();
    ReplicationKey {
        fingerprint: if public_key.is_empty() {
            String::new()
        } else {
            h.fingerprints(&public_key).into_iter().next().unwrap_or_default()
        },
        public_key,
        path: rep::KEY_PATH.into(),
    }
}

async fn key_get(State(st): State<ProtState>) -> Res<Json<ReplicationKey>> {
    let p = prot(&st)?;
    Ok(Json(
        blocking(&p, |p| Ok(key_view(p.replicas.as_ref(), p.replicas.public_key()))).await?,
    ))
}

/// Creates the replication key pair when missing (idempotent).
async fn key_create(State(st): State<ProtState>) -> Res<Json<ReplicationKey>> {
    let p = prot(&st)?;
    Ok(Json(
        blocking(&p, |p| {
            let k = p.replicas.ensure_key().map_err(Error::internal)?;
            Ok(key_view(p.replicas.as_ref(), Some(k)))
        })
        .await?,
    ))
}

/// The host keys a host presents now, with fingerprints, for the administrator to confirm.
async fn host_keys(
    State(st): State<ProtState>,
    body: Result<Json<HostKeyRequest>, JsonRejection>,
) -> Res<Json<Vec<HostKey>>> {
    let p = prot(&st)?;
    let req = json_body(body)?;
    let host = req.host.trim().trim_start_matches('[').trim_end_matches(']').to_string();
    let probe = SnapshotJob {
        name: "probe".into(),
        share: None,
        subvolume: "/probe".into(),
        snapshot_dir: String::new(),
        retention: Default::default(),
        replicas: vec![ReplicaTarget {
            id: "probe".into(),
            enabled: true,
            destination: ReplicaDestination::Ssh {
                host: host.clone(),
                port: req.port,
                user: "root".into(),
                path: "/probe".into(),
                sudo: false,
                host_keys: Vec::new(),
            },
            retention: rynas_api_types::replication::default_target_retention(),
            compress: Default::default(),
            rate_limit_kib: 0,
            after_snapshots: true,
        }],
    };
    rep::normalize_targets(&probe).map_err(invalid)?;
    Ok(Json(
        blocking(&p, move |p| {
            let lines = p
                .replicas
                .scan_host(&host, req.port)
                .map_err(|e| Error::new(StatusCode::BAD_GATEWAY, error_codes::BAD_GATEWAY, e))?;
            let fps = p.replicas.fingerprints(&lines.join("\n"));
            Ok(lines
                .into_iter()
                .enumerate()
                .filter_map(|(i, l)| {
                    Some(HostKey {
                        line: rynas_btrfs::btrbk_ssh::known_hosts_line(&l, &host, req.port)?,
                        fingerprint: fps.get(i).cloned().unwrap_or_default(),
                    })
                })
                .collect())
        })
        .await?,
    ))
}

// ---------------------------------------------------------------------------------------------
// transfers
// ---------------------------------------------------------------------------------------------

fn started(id: String) -> Response {
    (StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response()
}

/// The applied job and target.
fn applied(p: &Protection, job: &str, id: &str) -> Res<(SnapshotJob, ReplicaTarget)> {
    let c: SnapshotsConfig = from_value(p.ops.config.applied(SNAPSHOTS_KEY)?)?;
    let j = c
        .jobs
        .into_iter()
        .find(|j| j.name == job)
        .ok_or_else(|| not_found(format!("applied snapshot job {job}")))?;
    let t = j
        .replicas
        .iter()
        .find(|t| t.id == id)
        .cloned()
        .ok_or_else(|| not_found(format!("applied replica target {job}/{id}")))?;
    Ok((j, t))
}

async fn run_now(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path((job, id)): Path<(String, String)>,
) -> Res<Response> {
    let (p, who) = (prot(&st)?, user(&h)?);
    check_ids(&job, &id)?;
    Ok(started(spawn_transfer(&p, &job, &id, &who).await?))
}

/// Task `replication.run` of one target (scheduler task or "Replicate now").
pub async fn spawn_transfer(p: &Arc<Protection>, job: &str, id: &str, user: &str) -> Res<String> {
    let (j, i) = (job.to_string(), id.to_string());
    let (job, t) = blocking(p, move |p| applied(p, &j, &i)).await?;
    if !t.enabled {
        return Err(conflict(
            error_codes::CONFLICT,
            format!("replica target {}/{} is off", job.name, t.id),
        ));
    }
    let p = p.clone();
    Ok(p.ops
        .tasks
        .clone()
        .spawn("replication.run", user, move |ctx| async move {
            transfer(&ctx, &p, &job, &t).await.map(|_| ())
        })?)
}

/// The targets that follow the snapshots of `job`, one after the other, after its snapshot
/// run. Failures are logged and notified, never returned.
pub async fn after_snapshots(ctx: &TaskCtx, p: &Arc<Protection>, job: &SnapshotJob) {
    for t in job.replicas.iter().filter(|t| t.enabled && t.after_snapshots) {
        if ctx.is_cancelled() {
            return;
        }
        if let Err(e) = transfer(ctx, p, job, t).await {
            ctx.log(&format!("replica {}: {e}", t.id));
        }
    }
}

const LOCK_RETRY: Duration = Duration::from_secs(20);
const LOCK_TRIES: u32 = 45;

/// One btrbk command, retried while another run of the job holds its lock.
async fn run_btrbk(ctx: &TaskCtx, cmd: &rynas_tasks::Cmd) -> Result<rynas_tasks::CmdOutput, String> {
    for _ in 0..LOCK_TRIES {
        ctx.log(&format!("$ {}", cmd.display()));
        let out = cmd
            .run(Some(ctx.token()), None)
            .await
            .map_err(|e| format!("btrbk: {e}"))?;
        if out.code != Some(rep::EXIT_LOCKED) {
            return Ok(out);
        }
        ctx.log("another run of this job holds its lock; waiting");
        tokio::select! {
            _ = tokio::time::sleep(LOCK_RETRY) => {}
            _ = ctx.token().cancelled() => return Err("cancelled".into()),
        }
    }
    Err("the job's lockfile stayed held by another run".into())
}

/// Checks the target, creates a local target directory, cleans interrupted receives, sends
/// the missing replicas; records the status and notifies on changes.
pub async fn transfer(
    ctx: &TaskCtx,
    p: &Arc<Protection>,
    job: &SnapshotJob,
    t: &ReplicaTarget,
) -> Result<TransferReport, String> {
    ctx.log(&format!("replica {} of {} -> {}", t.id, job.name, t.destination.url()));
    let r = transfer_inner(ctx, p, job, t).await;
    let prev = read_status(&p.state_dir, &job.name, &t.id);
    let now = rynas_tasks::now_ms();
    let next = rep::next_status(&prev, now, r.as_ref().map_err(String::as_str));
    if let Err(e) = write_status(&p.state_dir, &job.name, &t.id, &next) {
        tracing::warn!("replication status {}/{}: {e}", job.name, t.id);
    }
    let key = format!("replication:{}:{}", job.name, t.id);
    let label = format!("{} → {}", job.share.as_deref().unwrap_or(&job.name), t.id);
    match &r {
        Ok(rep_) => {
            ctx.log(&format!(
                "replica {}: {} sent, {} deleted on the target (retention), {} s",
                t.id,
                rep_.sent.len(),
                rep_.pruned.len(),
                rep_.duration_s
            ));
            let ev = event::resolved(
                NotifyCategory::Backup,
                "replication",
                &key,
                format!("Replication {label} succeeded"),
            );
            let ev = event::field(event::field(event::field(ev, "job", &job.name), "target", &t.id), "name", &label);
            p.notifier.emit(event::code(ev, "replication.ok"));
        }
        Err(e) => {
            let hint = next.hint.map(|h| format!("{h:?}").to_ascii_lowercase()).unwrap_or_default();
            let mut ev = event::firing(
                NotifyCategory::Backup,
                NotifySeverity::Error,
                "replication",
                &key,
                format!("Replication {label} failed"),
            );
            let held = rep::held_snapshots(
                &p.replicas.snapshot_names(&job.snapshot_dir),
                &job.name,
                next.latest.as_deref(),
            );
            ev.body = if held > 1 {
                format!(
                    "{e}\n{held} snapshots of {} are kept on the source until this target has them: they use more and more space while it cannot be reached.",
                    job.name
                )
            } else {
                e.clone()
            };
            let ev = event::field(event::field(event::field(ev, "job", &job.name), "target", &t.id), "name", &label);
            let ev = event::field(ev, "held_snapshots", held);
            p.notifier.emit(event::code(event::field(ev, "hint", hint), "replication.failed"));
        }
    }
    r
}

async fn transfer_inner(
    ctx: &TaskCtx,
    p: &Arc<Protection>,
    job: &SnapshotJob,
    t: &ReplicaTarget,
) -> Result<TransferReport, String> {
    if let ReplicaDestination::Local { path } = &t.destination {
        let (h, j, tt, path) = (p.replicas.clone(), job.clone(), t.clone(), path.clone());
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let w = rep::warnings(h.as_ref(), &j, &tt);
            if w.contains(&ReplicaWarning::SameFilesystem) {
                return Err(format!("{path} is on the filesystem of {}: not a second copy", j.subvolume));
            }
            if w.contains(&ReplicaWarning::NotBtrfs) {
                return Err(format!("{path} is not on btrfs"));
            }
            if w.contains(&ReplicaWarning::NotMounted) {
                return Err(format!(
                    "no volume is mounted for {path}: refusing to create it on the system disk"
                ));
            }
            h.ensure_dir(&path)
        })
        .await
        .map_err(|e| e.to_string())??;
    }
    let clean = run_btrbk(ctx, &clean_cmd(&job.name, &t.id)).await?;
    let cleaned = rep::parse_transfer(&clean.stdout);
    for n in &cleaned.pruned {
        ctx.log(&format!("removed the interrupted receive {n}"));
    }
    let out = run_btrbk(ctx, &resume_cmd(&job.name, &t.id)).await?;
    let mut r = rep::parse_transfer(&out.stdout);
    for n in &r.sent {
        ctx.log(&format!("sent {n}"));
    }
    for n in &r.pruned {
        ctx.log(&format!("deleted {n} on the target (retention)"));
    }
    let stderr: Vec<&str> = clean
        .stderr
        .lines()
        .chain(out.stderr.lines())
        .filter(|l| !l.trim().is_empty())
        .collect();
    for l in &stderr {
        ctx.log(&format!("stderr: {l}"));
    }
    let mut errors = cleaned.errors;
    errors.append(&mut r.errors);
    if !errors.is_empty() || !out.success() || !clean.success() {
        let mut msg = errors.join("; ");
        // btrbk prints the reason of an aborted target on stderr (ssh, filter, path)
        let tail: Vec<&str> = stderr.iter().rev().take(4).rev().copied().collect();
        if !tail.is_empty() {
            if !msg.is_empty() {
                msg.push_str("; ");
            }
            msg.push_str(&tail.join(" "));
        }
        if msg.is_empty() {
            msg = format!("btrbk exited with code {:?}", out.code.or(clean.code));
        }
        return Err(msg);
    }
    r.pruned.extend(cleaned.pruned);
    Ok(r)
}

// ---------------------------------------------------------------------------------------------
// replicas of local targets: list, browse, restore, promote
// ---------------------------------------------------------------------------------------------

/// The applied local target directory of `job/id`.
fn local_dir(p: &Protection, job: &str, id: &str) -> Res<(SnapshotJob, PathBuf)> {
    let (j, t) = applied(p, job, id)?;
    match t.destination {
        ReplicaDestination::Local { path } => Ok((j, PathBuf::from(path))),
        ReplicaDestination::Ssh { .. } => Err(invalid(
            "replicas on another host are browsed and restored there",
        )),
    }
}

fn check_name(job: &str, name: &str) -> Res<()> {
    match rynas_btrfs::name::split_snapshot_name(name) {
        Some((prefix, _)) if prefix == job && !name.contains('/') => Ok(()),
        _ => Err(Error::bad_request("invalid replica name")),
    }
}

async fn replicas(
    State(st): State<ProtState>,
    Path((job, id)): Path<(String, String)>,
) -> Res<Json<Vec<ReplicaSnapshot>>> {
    let p = prot(&st)?;
    check_ids(&job, &id)?;
    Ok(Json(
        blocking(&p, move |p| {
            let (_, dir) = local_dir(p, &job, &id)?;
            match rep::scan_replicas(&dir, &job) {
                Ok(v) => Ok(v),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(e) => Err(Error::internal(format!("{}: {e}", dir.display()))),
            }
        })
        .await?,
    ))
}

#[derive(Deserialize)]
struct BrowseQuery {
    #[serde(default)]
    path: String,
}

async fn browse_replica(
    State(st): State<ProtState>,
    Path((job, id, name)): Path<(String, String, String)>,
    Query(q): Query<BrowseQuery>,
) -> Res<Json<ReplicaListing>> {
    let p = prot(&st)?;
    check_ids(&job, &id)?;
    check_name(&job, &name)?;
    Ok(Json(
        blocking(&p, move |p| {
            let (_, dir) = local_dir(p, &job, &id)?;
            let item = browse::resolve(&dir.join(&name), &q.path)?;
            let (entries, truncated) = browse::list(&item)?;
            Ok(ReplicaListing {
                path: browse::components(&q.path)?.join("/"),
                name,
                entries: entries
                    .into_iter()
                    .map(|e| ReplicaEntry {
                        name: e.name,
                        kind: e.kind,
                        size_bytes: e.size_bytes,
                        mtime_ms: e.mtime_ms,
                    })
                    .collect(),
                truncated,
            })
        })
        .await?,
    ))
}

/// Copies a file or folder of a replica next to the original in the job's live subvolume.
async fn restore_replica(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path((job, id, name)): Path<(String, String, String)>,
    body: Result<Json<ReplicaRestoreRequest>, JsonRejection>,
) -> Res<Response> {
    let (p, who) = (prot(&st)?, user(&h)?);
    check_ids(&job, &id)?;
    check_name(&job, &name)?;
    let req = json_body(body)?;
    browse::components(&req.path)?;
    if req.path.trim_matches('/').is_empty() {
        return Err(invalid(
            "choose a file or folder; a whole replica comes back with Promote",
        ));
    }
    let (j, i) = (job.clone(), id.clone());
    let (sj, dir) = blocking(&p, move |p| local_dir(p, &j, &i)).await?;
    let task = p
        .ops
        .tasks
        .spawn("replication.restore", &who, move |ctx| async move {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                c.log(&format!("restoring {} from the replica {name} ({id})", req.path));
                let r = restore::restore(
                    &dir.join(&name),
                    FsPath::new(&sj.subvolume),
                    &req.path,
                    RestoreMode::Copy,
                    (rynas_tasks::now_ms() / 1000) as i64,
                    &|| c.is_cancelled(),
                )
                .map_err(|e| e.to_string())?;
                c.log(&format!(
                    "restored as {} in {}: {} files, {} folders, {} bytes",
                    r.restored_to, sj.subvolume, r.files, r.dirs, r.bytes
                ));
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())?
        })?;
    Ok(started(task))
}

/// A writable snapshot of a replica as a new folder at the top of the target's volume.
async fn promote_replica(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path((job, id, name)): Path<(String, String, String)>,
    body: Result<Json<PromoteRequest>, JsonRejection>,
) -> Res<Response> {
    let (p, who) = (prot(&st)?, user(&h)?);
    check_ids(&job, &id)?;
    check_name(&job, &name)?;
    let req = json_body(body)?;
    if req.confirm != name {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            error_codes::CONFIRMATION_REQUIRED,
            format!("type the replica name {name} to confirm"),
        ));
    }
    if !rep::valid_folder(&req.folder) {
        return Err(invalid(format!("invalid folder name {:?}", req.folder)));
    }
    let (j, i) = (job.clone(), id.clone());
    let (dir, volume) = blocking(&p, move |p| {
        let (_, dir) = local_dir(p, &j, &i)?;
        let fs = p
            .replicas
            .fs_of(&dir.display().to_string())
            .filter(|f| f.btrfs && f.mount != "/")
            .ok_or_else(|| invalid("the target's volume is not mounted"))?;
        Ok((dir, PathBuf::from(fs.mount)))
    })
    .await?;
    let task = p
        .ops
        .tasks
        .spawn("replication.promote", &who, move |ctx| async move {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let to = rep::promote(&dir.join(&name), &volume, &req.folder)
                    .map_err(|e| e.to_string())?;
                c.log(&format!(
                    "{name} promoted to the writable subvolume {} (the replica stays read-only)",
                    to.display()
                ));
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())?
        })?;
    Ok(started(task))
}
