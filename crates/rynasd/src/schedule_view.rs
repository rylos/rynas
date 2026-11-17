//! The global Schedule page's endpoints (S5 of `docs/research/scheduling.md`):
//!
//! - `GET /v1/scheduler/overview?from&to`: every job (the Task Scheduler's tasks, the system
//!   timers rynas gates, the PC agents' backup plans), its starts in the range with systemd's
//!   calendar rules, its estimated length (median of its last 10 runs), the windows, what it
//!   claims and the conflicts ([`rynas_scheduler::overview`]);
//! - `GET /v1/scheduler/runs?task&outcome&from&limit&before`: the history of every task;
//! - `GET`/`PUT /v1/scheduler/maintenance-window`: the global maintenance window
//!   ([`rynas_api_types::scheduler::SchedulerConfig::maintenance`]); a `PUT` applies at once,
//!   like a pause.

use std::collections::{BTreeMap, HashMap};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use rynas_api_types::scheduler::{
    MaintenanceWindow, OverviewJob, OverviewOwner, RunOutcome, RunsPage, SchedulerConfig, SchedulerOverview, TaskTrigger,
    week_window_valid,
};
use rynas_scheduler::SCHEDULER_KEY;
use rynas_scheduler::overview::{JobInput, build};
use rynas_tasks::now_ms;
use serde::Deserialize;

use crate::ops::{Error, Res, user};
use crate::protection::{ProtState, Protection, Runtime, blocking, invalid, prot};

/// How many runs the estimate of a job's length comes from.
const ESTIMATE_RUNS: usize = 10;

pub fn routes() -> Router<ProtState> {
    Router::new()
        .route("/v1/scheduler/overview", get(overview))
        .route("/v1/scheduler/runs", get(runs))
        .route("/v1/scheduler/maintenance-window", get(get_window).put(put_window))
}

#[derive(Debug, Deserialize, Default)]
struct Range {
    from: Option<u64>,
    to: Option<u64>,
}

/// The overview of `[from, to]` (default: from now, 7 days).
pub(crate) fn overview_of(p: &Protection, owner: &str, rt: &Runtime, from: u64, to: u64) -> Res<SchedulerOverview> {
    let now = now_ms();
    let views = crate::scheduler::task_views(p, owner, rt)?;
    let estimates = p.history.estimates(ESTIMATE_RUNS).map_err(Error::internal)?;
    let table = crate::gate::table_now(p);
    let mut jobs: Vec<JobInput> = Vec::new();
    // what runs: the applied tasks, and the system timers rynas gates (they run on the
    // system's schedule even when rynas never applied their settings)
    for v in views.into_iter().filter(|v| v.status.applied || v.status.system) {
        let t = v.task;
        let claims = crate::claims::claims_of(p, &t);
        let resources: Vec<_> = claims.iter().map(|c| table.resource(c)).collect();
        let (estimate_ms, estimate_runs) = estimates.get(&t.id).map_or((None, 0), |(m, n)| (Some(*m), *n));
        let interval = matches!(t.trigger, TaskTrigger::Interval { .. });
        let last = if interval { crate::gate::interval_last(p, &t).ok().flatten().map(|r| r.1) } else { None };
        let next_ms = last.and_then(|l| rynas_scheduler::gate::interval_due_ms(&t, l));
        jobs.push(JobInput {
            job: OverviewJob {
                id: t.id.clone(),
                name: if t.name.is_empty() { t.id.clone() } else { t.name.clone() },
                owner: OverviewOwner::of(&rynas_api_types::scheduler::TaskOwner::of_action(&t.action)),
                action: Some(t.action.clone()),
                enabled: t.enabled,
                system: v.status.system,
                paused: v.status.paused.as_ref().is_some_and(|i| i.active(now)),
                estimate_ms,
                estimate_runs,
                resources: resources.clone(),
                maintenance: t.follows_maintenance(),
                trigger: Some(t.trigger.clone()),
                every_h: None,
                truncated: false,
            },
            task: Some(t),
            claims,
            resources,
            last_ms: last,
            next_ms,
        });
    }
    // the PC agents' backup plans (they run on the PCs, by their own clock)
    if let Some(agent) = p.ops.pc_agent.get().and_then(std::sync::Weak::upgrade) {
        let reg = agent.snapshot();
        for (name, d) in reg.devices.iter().filter(|(_, d)| !d.revoked) {
            let plan = &d.settings.plan;
            let shown = if d.settings.description.is_empty() { d.hostname.clone() } else { d.settings.description.clone() };
            let durations: Vec<u64> = d
                .history
                .iter()
                .rev()
                .filter(|r| !r.restore && r.ok)
                .take(ESTIMATE_RUNS)
                .filter_map(|r| r.ended_ms.checked_sub(r.started_ms))
                .collect();
            let estimate_ms = median(durations.clone());
            jobs.push(JobInput {
                job: OverviewJob {
                    id: format!("pc:{name}"),
                    name: if shown.is_empty() { name.clone() } else { shown },
                    owner: OverviewOwner::PcBackup,
                    action: None,
                    enabled: !plan.paused,
                    system: false,
                    paused: plan.paused,
                    estimate_ms,
                    estimate_runs: durations.len().min(255) as u8,
                    resources: Vec::new(),
                    maintenance: false,
                    trigger: None,
                    every_h: Some(plan.interval_hours),
                    truncated: false,
                },
                task: None,
                claims: Vec::new(),
                resources: Vec::new(),
                last_ms: d.last_ok_ms,
                next_ms: None,
            });
        }
    }
    jobs.sort_by(|a, b| a.job.name.to_lowercase().cmp(&b.job.name.to_lowercase()).then_with(|| a.job.id.cmp(&b.job.id)));
    let window = crate::scheduler::maintenance_window(p);
    let tz = rynas_power::system_tz();
    let disks = |k: &str| table.disks(k);
    let mut out = build(jobs, window.as_deref(), &tz, from, to, now, &disks);
    out.paused = p.gate.pause().all.filter(|a| a.active(now));
    Ok(out)
}

fn median(mut v: Vec<u64>) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2 })
}

async fn overview(State(st): State<ProtState>, h: HeaderMap, Query(q): Query<Range>) -> Res<Json<SchedulerOverview>> {
    let (p, owner) = (prot(&st)?, user(&h)?);
    let from = q.from.unwrap_or_else(now_ms);
    let to = q.to.unwrap_or(from + 7 * 86_400_000);
    if to < from {
        return Err(invalid("the end of the range is before its start"));
    }
    if to - from > rynas_scheduler::overview::MAX_RANGE_MS {
        return Err(invalid("the range is at most 35 days"));
    }
    let rt = p.runtime().await;
    Ok(Json(blocking(&p, move |p| overview_of(p, &owner, &rt, from, to)).await?))
}

#[derive(Debug, Deserialize, Default)]
struct RunsQuery {
    task: Option<String>,
    outcome: Option<String>,
    from: Option<u64>,
    before: Option<u64>,
    limit: Option<usize>,
}

async fn runs(State(st): State<ProtState>, Query(q): Query<RunsQuery>) -> Res<Json<RunsPage>> {
    let p = prot(&st)?;
    let outcome = match q.outcome.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => Some(
            serde_json::from_value::<RunOutcome>(serde_json::Value::String(s.into()))
                .map_err(|_| Error::bad_request("outcome is ok, failed, skipped or yielded"))?,
        ),
    };
    if let Some(t) = q.task.as_deref().filter(|t| !t.is_empty())
        && !rynas_scheduler::domain::valid_task_id(t)
    {
        return Err(Error::bad_request("invalid task id"));
    }
    let limit = q.limit.unwrap_or(crate::ops::LIST_DEFAULT).clamp(1, crate::ops::LIST_MAX);
    Ok(Json(
        blocking(&p, move |p| {
            let task = q.task.as_deref().filter(|t| !t.is_empty());
            let mut runs = p.history.all_runs(task, outcome, q.from, q.before, limit + 1).map_err(Error::internal)?;
            let more = runs.len() > limit;
            runs.truncate(limit);
            let names = names_of(p, &runs);
            Ok(RunsPage { runs, names, more })
        })
        .await?,
    ))
}

/// The names of the tasks of `runs` (the applied configuration and the gated system tasks;
/// a task deleted since keeps its id).
fn names_of(p: &Protection, runs: &[rynas_api_types::scheduler::TaskRun]) -> BTreeMap<String, String> {
    let doc: SchedulerConfig = p
        .ops
        .config
        .applied(SCHEDULER_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let known: HashMap<&str, &str> = doc.tasks.iter().map(|t| (t.id.as_str(), t.name.as_str())).collect();
    let mut out = BTreeMap::new();
    for r in runs {
        if out.contains_key(&r.task_id) {
            continue;
        }
        let name = match known.get(r.task_id.as_str()) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => crate::scheduler::applied_task(p, &r.task_id)
                .ok()
                .flatten()
                .map(|t| t.name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| r.task_id.clone()),
        };
        out.insert(r.task_id.clone(), name);
    }
    out
}

async fn get_window(State(st): State<ProtState>) -> Res<Json<MaintenanceWindow>> {
    let p = prot(&st)?;
    Ok(Json(blocking(&p, |p| Ok(MaintenanceWindow { window: crate::scheduler::maintenance_window(p) })).await?))
}

/// `PUT /v1/scheduler/maintenance-window`: applied at once (the scheduler's units do not
/// change: the gate reads the window at every start).
async fn put_window(
    State(st): State<ProtState>,
    h: HeaderMap,
    body: Result<Json<MaintenanceWindow>, JsonRejection>,
) -> Res<Json<MaintenanceWindow>> {
    let (p, who) = (prot(&st)?, user(&h)?);
    let want = body.map(|Json(v)| v).map_err(Error::from)?;
    if let Some(w) = &want.window
        && !week_window_valid(w)
    {
        return Err(invalid("the maintenance window is 168 cells of 0 or 1 with at least one 1"));
    }
    let window = want.window.clone();
    blocking(&p, move |p| {
        let comment = if window.is_some() { "maintenance window set" } else { "maintenance window off" };
        p.ops
            .config
            .apply_update(&who, SCHEDULER_KEY, comment, &|l| tracing::info!("scheduler: {l}"), |cur| {
                let mut doc: SchedulerConfig = match cur {
                    Some(v) => serde_json::from_value(v).map_err(|e| e.to_string())?,
                    None => SchedulerConfig::default(),
                };
                if doc.maintenance == window {
                    return Ok(None);
                }
                doc.maintenance = window.clone();
                serde_json::to_value(doc).map(Some).map_err(|e| e.to_string())
            })
            .map_err(Error::from)?;
        tracing::info!(
            "scheduler: maintenance window {} by {who}",
            if window.is_some() { "set" } else { "turned off" }
        );
        p.invalidate();
        Ok(())
    })
    .await?;
    Ok(Json(MaintenanceWindow { window: want.window }))
}
