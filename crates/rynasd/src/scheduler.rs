//! Task Scheduler endpoints of the internal API (`/v1/scheduler/...`, public under
//! `/api/v1/scheduler/...`).
//!
//! Writes stage the `scheduler` key of the session user (`POST /v1/config/apply` applies:
//! units, scripts, `daemon-reload`, timers); "run now" starts the applied unit. Two routes
//! are internal only (rynas-web never forwards them): `finished`, called by
//! `rynasctl task-finished` from `ExecStopPost=` of every task unit, and `execute`, called by
//! `rynasctl job run` from the units of internal tasks.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::scheduler::{
    CalendarPreview, CalendarPreviewRequest, CalendarSyntax, CronEntry, NotifyPolicy,
    ScheduledTask, SchedulerConfig, SystemTimer, TaskAction, TaskRun, TaskStatus, TaskView,
};
use rynas_api_types::snapshots::SnapshotsConfig;
use rynas_api_types::{StagedChanges, TaskStarted, error_codes};
use rynas_btrfs::SNAPSHOTS_KEY;
use rynas_notify::event::{code, field, firing, oneshot, resolved};
use rynas_notify::{BodyFormat, NewEvent, NotifyCategory, NotifySeverity};
use rynas_scheduler::calendar::{Converted, cron_to_calendar, plausible_calendar};
use rynas_scheduler::domain::{normalize_task, valid_task_id};
use rynas_scheduler::history::{NewRun, OUTPUT_MAX};
use rynas_scheduler::units::{UNIT_PREFIX, service_name, timer_name};
use rynas_scheduler::{SCHEDULER_KEY, cron};
use serde::{Deserialize, Serialize};

use crate::error::created;
use crate::ops::{Error, Res, user};
use crate::protection::{
    Page, ProtState, Protection, Runtime, blocking, conflict, invalid, not_found, prot,
};
use crate::services::{docs, pending_op, stage};

/// Body of the internal `POST /v1/scheduler/tasks/{id}/finished` (`rynasctl task-finished`,
/// from the variables systemd gives `ExecStopPost=`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskFinished {
    /// `$INVOCATION_ID`.
    pub invocation_id: String,
    /// `$SERVICE_RESULT`.
    pub result: String,
    /// `$EXIT_CODE`: `exited`, `killed`, `dumped`.
    #[serde(default)]
    pub exit_code: String,
    /// `$EXIT_STATUS`: a number or a signal name.
    #[serde(default)]
    pub exit_status: String,
}

pub fn router(st: ProtState) -> Router {
    Router::new()
        .route("/v1/scheduler/tasks", get(list).post(create))
        .route(
            "/v1/scheduler/tasks/preview-calendar",
            post(preview_calendar),
        )
        .route(
            "/v1/scheduler/tasks/{id}",
            get(get_one).put(put).delete(delete),
        )
        .route("/v1/scheduler/tasks/{id}/run", post(run_now))
        .route("/v1/scheduler/tasks/{id}/enable", post(enable))
        .route("/v1/scheduler/tasks/{id}/disable", post(disable))
        .route("/v1/scheduler/tasks/{id}/history", get(history))
        .route("/v1/scheduler/tasks/{id}/history/{n}", get(history_one))
        .route("/v1/scheduler/system-timers", get(system_timers))
        .route("/v1/scheduler/cron-entries", get(cron_entries))
        .merge(root_routes())
        .with_state(st)
}

/// Internal only, called by `rynasctl` (root) from the task units: never forwarded by
/// rynas-web, and refused unless the connection's peer is uid 0.
fn root_routes() -> Router<ProtState> {
    Router::new()
        .route("/v1/scheduler/tasks/{id}/finished", post(finished))
        .route("/v1/scheduler/tasks/{id}/execute", post(execute))
        .route_layer(axum::middleware::from_fn(crate::server::require_root_peer))
}

fn check_id(id: &str) -> Res<()> {
    if valid_task_id(id) {
        Ok(())
    } else {
        Err(Error::bad_request("invalid task id"))
    }
}

fn json_body<T>(b: Result<Json<T>, JsonRejection>) -> Res<T> {
    b.map(|Json(v)| v).map_err(Error::from)
}

/// A user task running as `root`: only administrators create, change, delete, enable,
/// disable or run one (checked against the system groups, [`crate::roles`]).
fn guard_root_task(p: &Protection, who: &str, t: &ScheduledTask) -> Res<()> {
    if t.action.user() == Some("root") {
        p.ops
            .admins
            .require_admin(who, &format!("task {} runs as root", t.id))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// views
// ---------------------------------------------------------------------------------------------

fn status_of(
    t: &ScheduledTask,
    applied: bool,
    pending: Option<rynas_api_types::PendingOp>,
    rt: &Runtime,
    latest: &HashMap<String, TaskRun>,
) -> TaskStatus {
    let unit = service_name(&t.id);
    let timer = timer_name(&t.id);
    TaskStatus {
        pending,
        applied,
        running: rt.running.contains(&unit),
        next_run_ms: if applied && t.enabled {
            rt.timers
                .iter()
                .find(|x| x.unit == timer)
                .and_then(|x| x.next_ms)
        } else {
            None
        },
        last_run: latest.get(&t.id).cloned(),
        unit,
    }
}

pub(crate) fn task_views(p: &Protection, owner: &str, rt: &Runtime) -> Res<Vec<TaskView>> {
    let c = &p.ops.config;
    let (eff, cur, applied): (SchedulerConfig, SchedulerConfig, Option<SchedulerConfig>) =
        docs(c, owner, SCHEDULER_KEY)?;
    let latest = p.history.latest().map_err(Error::internal)?;
    let is_applied = |id: &str| {
        applied
            .as_ref()
            .is_some_and(|a| a.tasks.iter().any(|t| t.id == id))
    };
    let mut out: Vec<TaskView> = eff
        .tasks
        .iter()
        .map(|t| {
            let base = cur.tasks.iter().find(|x| x.id == t.id);
            TaskView {
                status: status_of(t, is_applied(&t.id), pending_op(Some(t), base), rt, &latest),
                task: t.clone(),
            }
        })
        .collect();
    for t in cur
        .tasks
        .iter()
        .filter(|t| !eff.tasks.iter().any(|x| x.id == t.id))
    {
        out.push(TaskView {
            status: status_of(t, is_applied(&t.id), pending_op(None, Some(t)), rt, &latest),
            task: t.clone(),
        });
    }
    Ok(out)
}

async fn view_of(p: &Arc<Protection>, owner: String, id: String) -> Res<TaskView> {
    let rt = p.runtime().await;
    blocking(p, move |p| {
        task_views(p, &owner, &rt)?
            .into_iter()
            .find(|v| v.task.id == id)
            .ok_or_else(|| not_found(format!("task {id}")))
    })
    .await
}

async fn list(State(st): State<ProtState>, h: HeaderMap) -> Res<Json<Vec<TaskView>>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    let rt = p.runtime().await;
    Ok(Json(
        blocking(&p, move |p| task_views(p, &owner, &rt)).await?,
    ))
}

async fn get_one(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Res<Json<TaskView>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    check_id(&id)?;
    Ok(Json(view_of(&p, owner, id).await?))
}

// ---------------------------------------------------------------------------------------------
// writes (staged)
// ---------------------------------------------------------------------------------------------

/// Changes the user's effective `scheduler` document and stages it.
pub(crate) fn stage_tasks(
    p: &Protection,
    owner: &str,
    f: impl FnOnce(&mut SchedulerConfig) -> Res<()>,
) -> Res<()> {
    let c = &p.ops.config;
    let (mut doc, _, _): (SchedulerConfig, SchedulerConfig, _) = docs(c, owner, SCHEDULER_KEY)?;
    f(&mut doc)?;
    stage(c, owner, SCHEDULER_KEY, &doc)
}

/// Normalizes a task and checks it against the system (calendar with systemd-analyze,
/// users, root-owned programs) and the user's snapshot jobs.
pub(crate) fn checked_task(p: &Protection, owner: &str, t: ScheduledTask) -> Res<ScheduledTask> {
    check_task(p, owner, t, None)
}

/// [`checked_task`], with `new_job` counted as an existing snapshot job (a job and its task
/// are staged together).
pub(crate) fn check_task(
    p: &Protection,
    owner: &str,
    t: ScheduledTask,
    new_job: Option<&str>,
) -> Res<ScheduledTask> {
    let t = normalize_task(t).map_err(invalid)?;
    if let TaskAction::Snapshot { job } = &t.action
        && new_job != Some(job.as_str())
    {
        let (snaps, _, _): (SnapshotsConfig, SnapshotsConfig, _) =
            docs(&p.ops.config, owner, SNAPSHOTS_KEY)?;
        if !snaps.jobs.iter().any(|j| &j.name == job) {
            return Err(invalid(format!("there is no snapshot job {job}")));
        }
    }
    let probe = p.probe.clone();
    p.scheduler
        .check_value(std::slice::from_ref(&t), &|expr| {
            probe.analyze_calendar(expr, 1).map(|_| ())
        })
        .map_err(invalid)?;
    Ok(t)
}

async fn create(
    State(st): State<ProtState>,
    h: HeaderMap,
    body: Result<Json<ScheduledTask>, JsonRejection>,
) -> Res<Response> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    let t = json_body(body)?;
    let id = t.id.trim().to_string();
    let o = owner.clone();
    blocking(&p, move |p| {
        let t = checked_task(p, &o, t)?;
        guard_root_task(p, &o, &t)?;
        if matches!(t.action, TaskAction::Snapshot { .. }) {
            return Err(invalid(
                "snapshot tasks are created with their job (/snapshots/jobs)",
            ));
        }
        if matches!(t.action, TaskAction::PowerShutdown { .. }) {
            return Err(invalid(
                "scheduled shutdowns are created with the power schedule (/power/schedule)",
            ));
        }
        if matches!(t.action, TaskAction::Backup { .. }) {
            return Err(invalid(
                "backup tasks are created with their job (/backup/restic/jobs)",
            ));
        }
        stage_tasks(p, &o, |doc| {
            if doc.tasks.iter().any(|x| x.id == t.id) {
                return Err(conflict(
                    error_codes::ALREADY_EXISTS,
                    format!("task {} already exists", t.id),
                ));
            }
            doc.tasks.push(t);
            Ok(())
        })
    })
    .await?;
    let loc = id.clone();
    let v = view_of(&p, owner, id).await?;
    Ok(created("/api/v1/scheduler/tasks", &loc, v))
}

async fn put(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<ScheduledTask>, JsonRejection>,
) -> Res<Json<TaskView>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    check_id(&id)?;
    let mut t = json_body(body)?;
    if t.id.is_empty() {
        t.id = id.clone();
    }
    if t.id != id {
        return Err(invalid("the task id cannot change"));
    }
    let o = owner.clone();
    blocking(&p, move |p| {
        let t = checked_task(p, &o, t)?;
        guard_root_task(p, &o, &t)?;
        stage_tasks(p, &o, |doc| {
            let cur = doc
                .tasks
                .iter_mut()
                .find(|x| x.id == t.id)
                .ok_or_else(|| not_found(format!("task {}", t.id)))?;
            guard_root_task(p, &o, cur)?;
            if cur.action.is_internal() && cur.action != t.action {
                return Err(invalid(
                    "the action of an internal task cannot change (only its schedule and options)",
                ));
            }
            if !cur.action.is_internal() && t.action.is_internal() {
                return Err(invalid("a user task cannot become an internal one"));
            }
            *cur = t;
            Ok(())
        })
    })
    .await?;
    Ok(Json(view_of(&p, owner, id).await?))
}

async fn delete(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Res<Json<StagedChanges>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    check_id(&id)?;
    blocking(&p, move |p| {
        stage_tasks(p, &owner, |doc| {
            let t = doc
                .tasks
                .iter()
                .find(|x| x.id == id)
                .ok_or_else(|| not_found(format!("task {id}")))?;
            guard_root_task(p, &owner, t)?;
            if let TaskAction::Snapshot { job } = &t.action {
                return Err(conflict(
                    error_codes::CONFLICT,
                    format!("task {id} runs snapshot job {job}: delete the job instead"),
                ));
            }
            if let TaskAction::Backup { job } = &t.action {
                return Err(conflict(
                    error_codes::CONFLICT,
                    format!("task {id} runs backup job {job}: delete the job instead"),
                ));
            }
            if let TaskAction::PowerShutdown { rule } = &t.action {
                return Err(conflict(
                    error_codes::CONFLICT,
                    format!(
                        "task {id} runs power schedule rule {rule}: change the schedule instead"
                    ),
                ));
            }
            doc.tasks.retain(|x| x.id != id);
            Ok(())
        })
    })
    .await?;
    Ok(Json(StagedChanges { staged: 1 }))
}

async fn set_enabled(st: ProtState, h: HeaderMap, id: String, on: bool) -> Res<Json<TaskView>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    check_id(&id)?;
    let (o, i) = (owner.clone(), id.clone());
    blocking(&p, move |p| {
        stage_tasks(p, &o, |doc| {
            let t = doc
                .tasks
                .iter_mut()
                .find(|x| x.id == i)
                .ok_or_else(|| not_found(format!("task {i}")))?;
            guard_root_task(p, &o, t)?;
            t.enabled = on;
            Ok(())
        })
    })
    .await?;
    Ok(Json(view_of(&p, owner, id).await?))
}

async fn enable(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Res<Json<TaskView>> {
    set_enabled(st, h, id, true).await
}

async fn disable(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Res<Json<TaskView>> {
    set_enabled(st, h, id, false).await
}

/// The applied task `id`.
pub(crate) fn applied_task(p: &Protection, id: &str) -> Res<Option<ScheduledTask>> {
    let doc: SchedulerConfig = crate::services::from_value(p.ops.config.applied(SCHEDULER_KEY)?)?;
    Ok(doc.tasks.into_iter().find(|t| t.id == id))
}

/// Run now: starts the applied unit (same environment as a scheduled run); 409
/// `not_applied` for a task that is only staged.
async fn run_now(
    State(st): State<ProtState>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Res<StatusCode> {
    let (p, who) = (prot(&st)?, user(&h)?);
    check_id(&id)?;
    blocking(&p, move |p| {
        let Some(t) = applied_task(p, &id)? else {
            return Err(conflict(
                error_codes::NOT_APPLIED,
                format!("task {id} is not applied yet: apply the pending changes first"),
            ));
        };
        guard_root_task(p, &who, &t)?;
        p.ops
            .config
            .system()
            .start_unit(&service_name(&id))
            .map_err(Error::internal)
    })
    .await?;
    p.invalidate();
    Ok(StatusCode::NO_CONTENT)
}

async fn history(
    State(st): State<ProtState>,
    Path(id): Path<String>,
    Query(q): Query<Page>,
) -> Res<Json<Vec<TaskRun>>> {
    let p = prot(&st)?;
    check_id(&id)?;
    let (limit, before) = (q.limit(), q.before);
    Ok(Json(
        blocking(&p, move |p| {
            p.history.list(&id, limit, before).map_err(Error::internal)
        })
        .await?,
    ))
}

async fn history_one(
    State(st): State<ProtState>,
    Path((id, n)): Path<(String, String)>,
) -> Res<Json<TaskRun>> {
    let p = prot(&st)?;
    check_id(&id)?;
    let n: u64 = n
        .parse()
        .map_err(|_| Error::bad_request("the run must be a number"))?;
    Ok(Json(
        blocking(&p, move |p| {
            p.history
                .get(&id, n)
                .map_err(Error::internal)?
                .ok_or_else(|| not_found(format!("run {n} of task {id}")))
        })
        .await?,
    ))
}

// ---------------------------------------------------------------------------------------------
// previews and read-only views of the system
// ---------------------------------------------------------------------------------------------

/// Validation and next runs of a calendar or cron expression; always 200, `valid: false`
/// with the reason when it is not usable.
async fn preview_calendar(
    State(st): State<ProtState>,
    body: Result<Json<CalendarPreviewRequest>, JsonRejection>,
) -> Res<Json<CalendarPreview>> {
    let p = prot(&st)?;
    let req = json_body(body)?;
    let count = req.count.unwrap_or(5).clamp(1, 20);
    let bad = |e: String| CalendarPreview {
        valid: false,
        error: Some(e),
        ..Default::default()
    };
    let expr = match req.syntax {
        CalendarSyntax::Calendar => req.expression.trim().to_string(),
        CalendarSyntax::Cron => match cron_to_calendar(&req.expression) {
            Ok(Converted::Calendar(c)) => c,
            Ok(Converted::Boot) => {
                return Ok(Json(
                    bad("@reboot is a boot trigger, not a calendar".into()),
                ));
            }
            Err(e) => return Ok(Json(bad(e))),
        },
    };
    if let Err(e) = plausible_calendar(&expr) {
        return Ok(Json(bad(e)));
    }
    let probe = p.probe.clone();
    let e2 = expr.clone();
    let r = tokio::task::spawn_blocking(move || probe.analyze_calendar(&e2, count))
        .await
        .map_err(Error::internal)?;
    Ok(Json(match r {
        Ok(a) => CalendarPreview {
            valid: true,
            on_calendar: expr,
            normalized: a.normalized,
            next_ms: a.next_ms,
            error: None,
        },
        Err(e) => CalendarPreview {
            on_calendar: expr,
            ..bad(e)
        },
    }))
}

async fn system_timers(State(st): State<ProtState>) -> Res<Json<Vec<SystemTimer>>> {
    let p = prot(&st)?;
    let rt = p.runtime().await;
    let theirs: Vec<_> = rt
        .timers
        .iter()
        .filter(|t| !t.unit.starts_with(UNIT_PREFIX))
        .cloned()
        .collect();
    Ok(Json(
        blocking(&p, move |p| {
            let units: Vec<String> = theirs.iter().map(|t| t.unit.clone()).collect();
            let cals = p.probe.timer_calendars(&units).unwrap_or_default();
            Ok(theirs
                .into_iter()
                .map(|t| SystemTimer {
                    on_calendar: cals.get(&t.unit).cloned().unwrap_or_default(),
                    unit: t.unit,
                    activates: t.activates,
                    next_ms: t.next_ms,
                    last_ms: t.last_ms,
                })
                .collect())
        })
        .await?,
    ))
}

async fn cron_entries(State(st): State<ProtState>) -> Res<Json<Vec<CronEntry>>> {
    let p = prot(&st)?;
    Ok(Json(
        blocking(&p, |_| Ok(cron::list(std::path::Path::new("/")))).await?,
    ))
}

// ---------------------------------------------------------------------------------------------
// internal: rynasctl
// ---------------------------------------------------------------------------------------------

/// `rynasctl task-finished <id>`: records the run, then (in the background, once journald
/// has the last lines) its output, and notifies.
async fn finished(
    State(st): State<ProtState>,
    Path(id): Path<String>,
    body: Result<Json<TaskFinished>, JsonRejection>,
) -> Res<StatusCode> {
    let p = prot(&st)?;
    check_id(&id)?;
    let f = json_body(body)?;
    if f.invocation_id.is_empty()
        || f.invocation_id.len() > 64
        || !f.invocation_id.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(Error::bad_request("invalid invocation id"));
    }
    let (exit_code, signal) = match f.exit_code.as_str() {
        "exited" => (f.exit_status.parse().ok(), None),
        "killed" | "dumped" => (None, Some(f.exit_status.clone()).filter(|s| !s.is_empty())),
        _ => (None, None),
    };
    let tid = id.clone();
    let run_id = blocking(&p, move |p| {
        let run = NewRun {
            start_ms: p.probe.start_ms(&service_name(&tid)),
            end_ms: rynas_tasks::now_ms(),
            task_id: tid,
            invocation_id: f.invocation_id.clone(),
            result: if f.result.is_empty() {
                "unknown".into()
            } else {
                f.result.chars().take(32).collect()
            },
            exit_code,
            signal,
        };
        let id = p.history.record(&run).map_err(Error::internal)?;
        Ok((id, run))
    })
    .await?;
    p.invalidate();
    let p2 = p.clone();
    tokio::spawn(async move {
        // journald may still be processing the last lines of the stream
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = tokio::task::spawn_blocking(move || collect_output(&p2, &id, run_id)).await;
    });
    Ok(StatusCode::NO_CONTENT)
}

fn collect_output(p: &Protection, task_id: &str, (run_id, run): (u64, NewRun)) {
    p.probe.sync_journal();
    let out = match p.probe.journal(&run.invocation_id, OUTPUT_MAX) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("output of task {task_id}: {e}");
            Default::default()
        }
    };
    if let Err(e) = p.history.set_output(run_id, &out.text, out.truncated) {
        tracing::warn!("output of task {task_id}: {e}");
    }
    let Ok(Some(task)) = applied_task(p, task_id) else {
        return;
    };
    let failed_before = p
        .history
        .list(task_id, 2, None)
        .ok()
        .and_then(|l| l.get(1).map(|r| !r.ok))
        .unwrap_or(false);
    if let Some(ev) = run_event(&task, &run, &out.text, failed_before) {
        p.notifier.emit(ev);
    }
}

/// User of the rynasd tasks that scheduler units start (`rynasctl job run`): their outcome
/// is notified once, by the unit's run ([`run_event`], with the task's policy), so the
/// generic task watch skips them.
pub const SCHEDULER_USER: &str = "scheduler";

/// Longest output attached to a notification (the engine keeps 4000 characters of a body):
/// the end of the output, where the error usually is.
const NOTIFY_OUTPUT_MAX: usize = 3500;

/// The notification of a finished run, following the task's policy:
/// - a failure opens the condition (`firing`, error);
/// - a success resolves it (the engine ignores a resolution when nothing was open);
/// - with `always`, a success that resolves nothing is a `oneshot` info event.
///
/// Stable keys: `snapshot:job:<job>` (category backup) for snapshot jobs,
/// `scheduler:task:<id>` (category task) for every other task.
pub fn run_event(
    task: &ScheduledTask,
    run: &NewRun,
    output: &str,
    failed_before: bool,
) -> Option<NewEvent> {
    if task.notify == NotifyPolicy::Never {
        return None;
    }
    // event codes: `snapshot.job_*` / `sched.task_*` + failed|ok|finished, field `name`
    let (cat, source, key, what, family, name) = match &task.action {
        TaskAction::Snapshot { job } => (
            NotifyCategory::Backup,
            "snapshots",
            format!("snapshot:job:{job}"),
            format!("Snapshot job {job}"),
            "snapshot.job",
            job.clone(),
        ),
        TaskAction::Backup { job } => (
            NotifyCategory::Backup,
            "backup",
            format!("backup:job:{job}"),
            format!("Backup job {job}"),
            "backup.job",
            job.clone(),
        ),
        _ => (
            NotifyCategory::Task,
            "scheduler",
            format!("scheduler:task:{}", task.id),
            format!("Task {}", task.name),
            "sched.task",
            task.name.clone(),
        ),
    };
    let how = match (run.exit_code, &run.signal) {
        (Some(c), _) => format!("exit code {c}"),
        (None, Some(s)) => format!("killed by SIG{s}"),
        _ => run.result.clone(),
    };
    let (mut ev, outcome) = if !run.ok() {
        let title = format!("{what} failed ({how})");
        (
            firing(cat, NotifySeverity::Error, source, &key, title),
            "failed",
        )
    } else if failed_before || task.notify == NotifyPolicy::OnFailure {
        (
            resolved(cat, source, &key, format!("{what} succeeded")),
            "ok",
        )
    } else {
        let title = format!("{what} finished");
        let ev = oneshot(
            cat,
            NotifySeverity::Info,
            source,
            format!("{key}:ok"),
            title,
        );
        (ev, "finished")
    };
    if task.notify_output && !output.is_empty() {
        ev.body = tail(output, NOTIFY_OUTPUT_MAX);
        ev.body_format = BodyFormat::Log;
    } else {
        ev.body = format!(
            "Result: {} ({how}). See the task history for the output.",
            run.result
        );
    }
    let ev = code(ev, &format!("{family}_{outcome}"));
    let ev = field(field(ev, "task", &task.id), "result", &run.result);
    let ev = field(ev, "name", name);
    let ev = match &run.signal {
        Some(s) => field(ev, "signal", s),
        None => ev,
    };
    Some(match run.exit_code {
        Some(c) => field(ev, "exit_code", c),
        None => ev,
    })
}

/// The last `max` characters of `s`, marked when cut.
fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let cut: String = s.chars().skip(n - max).collect();
    format!("[…]\n{cut}")
}

/// `rynasctl job run <id>`: starts the rynasd task of an applied internal task (as user
/// [`SCHEDULER_USER`]).
async fn execute(State(st): State<ProtState>, Path(id): Path<String>) -> Res<Response> {
    let p = prot(&st)?;
    check_id(&id)?;
    let i = id.clone();
    let task = blocking(&p, move |p| applied_task(p, &i))
        .await?
        .ok_or_else(|| not_found(format!("applied task {id}")))?;
    if !task.action.is_internal() {
        return Err(invalid(format!("task {id} is not an internal task")));
    }
    let upid = crate::jobs::spawn(&p, &task, SCHEDULER_USER).await?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: upid })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_notify::EventState;

    fn task(action: serde_json::Value, notify: &str, output: bool) -> ScheduledTask {
        serde_json::from_value(serde_json::json!({
            "id": "t1", "name": "Nightly", "action": action, "trigger": {"kind": "manual"},
            "notify": notify, "notify_output": output,
        }))
        .unwrap()
    }

    fn run(result: &str, code: Option<i32>) -> NewRun {
        NewRun {
            task_id: "t1".into(),
            result: result.into(),
            exit_code: code,
            ..Default::default()
        }
    }

    #[test]
    fn run_events_follow_the_policy() {
        let cmd = serde_json::json!({"kind": "command", "command": "false", "user": "alice"});
        let t = task(cmd.clone(), "on_failure", false);
        let e = run_event(&t, &run("exit-code", Some(1)), "boom", false).unwrap();
        assert_eq!(
            (e.state, e.severity, e.key.as_deref()),
            (
                EventState::Firing,
                NotifySeverity::Error,
                Some("scheduler:task:t1")
            )
        );
        assert!(e.title.contains("exit code 1"), "{}", e.title);
        assert!(!e.body.contains("boom"), "no output without notify_output");
        // a success resolves (the engine ignores it when nothing is open)
        let e = run_event(&t, &run("success", Some(0)), "", false).unwrap();
        assert_eq!(e.state, EventState::Resolved);
        let never = task(cmd.clone(), "never", true);
        assert!(run_event(&never, &run("exit-code", Some(1)), "x", false).is_none());
        // always: a plain success is a oneshot info, a recovery a resolution
        let t = task(cmd, "always", true);
        let e = run_event(&t, &run("success", Some(0)), "ok", false).unwrap();
        assert_eq!(
            (e.state, e.severity, e.key.as_deref(), e.body.as_str()),
            (
                EventState::Oneshot,
                NotifySeverity::Info,
                Some("scheduler:task:t1:ok"),
                "ok"
            )
        );
        let e = run_event(&t, &run("success", Some(0)), "ok", true).unwrap();
        assert_eq!(e.state, EventState::Resolved);
    }

    #[test]
    fn snapshot_jobs_have_their_own_key_and_long_output_keeps_its_end() {
        let action = serde_json::json!({"kind": "snapshot", "job": "docs"});
        let t = task(action, "on_failure", true);
        let out = format!("{}\nbtrbk: ERROR: no space", "x".repeat(10_000));
        let e = run_event(&t, &run("exit-code", Some(10)), &out, false).unwrap();
        assert_eq!(e.key.as_deref(), Some("snapshot:job:docs"));
        assert_eq!(e.category, NotifyCategory::Backup);
        assert!(e.body.ends_with("no space") && e.body.starts_with("[…]"));
        assert!(e.body.chars().count() < 4000);
        let e = run_event(&t, &run("signal", None), "", false).unwrap();
        assert!(e.title.contains("signal") && !e.fields.contains_key("exit_code"));
    }
}
