//! The global Schedule page's endpoints (S5 of `docs/research/scheduling.md`):
//!
//! - `GET /v1/scheduler/overview?from&to`: every job (the Task Scheduler's tasks, the system
//!   timers rynas gates, the PC agents' backup plans), its starts in the range with systemd's
//!   calendar rules, its estimated length (median of its last 10 successful runs), the
//!   windows, what it claims and the conflicts ([`rynas_scheduler::overview`]), from the
//!   applied configuration only;
//! - `GET /v1/scheduler/runs?task&outcome&from&limit&before`: the history of every task;
//! - `GET`/`PUT /v1/scheduler/maintenance-window`: the global maintenance window (its own
//!   key, [`MAINTENANCE_KEY`]); a `PUT` applies at once, like a pause.

use std::collections::{BTreeMap, HashMap};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use rynas_api_types::scheduler::{
    MAINTENANCE_KEY, MaintenanceWindow, OverviewJob, OverviewOwner, RunOutcome, RunsPage, ScheduledTask, SchedulerConfig, SchedulerOverview, TaskTrigger,
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
pub(crate) fn overview_of(p: &Protection, rt: &Runtime, from: u64, to: u64) -> Res<SchedulerOverview> {
    let now = now_ms();
    let estimates = p.history.estimates(ESTIMATE_RUNS).map_err(Error::internal)?;
    let table = crate::gate::table_now(p);
    let pause = p.gate.pause();
    // what runs: the applied tasks (never a user's pending draft), and the system timers rynas
    // gates while the system has them (they run on the system's schedule even when rynas never
    // applied their settings)
    let applied = p.ops.config.applied(SCHEDULER_KEY).map_err(Error::from)?;
    let mut tasks: Vec<(ScheduledTask, bool)> = rynas_scheduler::domain::tasks_of(applied.as_ref())
        .into_iter()
        .map(|mut t| {
            t.migrate();
            (t, false)
        })
        .collect();
    for (id, _, timer) in rynas_api_types::scheduler::system::GATED_UNITS {
        if rt.timers.iter().any(|x| x.unit == *timer)
            && let Some(t) = crate::scheduler::gated_system_task(p, id)
        {
            tasks.push((t, true));
        }
    }
    let mut jobs: Vec<JobInput> = Vec::new();
    for (t, system) in tasks {
        let paused = pause.tasks.get(&t.id).is_some_and(|i| i.active(now));
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
                system,
                paused,
                estimate_ms,
                estimate_runs,
                resources: resources.clone(),
                maintenance: t.follows_maintenance(),
                trigger: Some(t.trigger.clone()),
                every_h: None,
                window: None,
                truncated: false,
            },
            task: Some(t),
            claims,
            resources,
            last_ms: last,
            interval_from_ms: None,
            from_start: true,
            next_ms,
        });
    }
    // the PC agents' backup plans (they run on the PCs, by their own clock and the rules of
    // `rynas_schedule::window`, the same as the overview's)
    if let Some(agent) = p.ops.pc_agent.get().and_then(std::sync::Weak::upgrade) {
        let reg = agent.snapshot();
        for (name, d) in reg.devices.iter().filter(|(_, d)| !d.revoked) {
            let plan = &d.settings.plan;
            let durations: Vec<u64> = d
                .history
                .iter()
                .rev()
                .filter(|r| !r.restore && r.ok)
                .take(ESTIMATE_RUNS)
                .filter_map(|r| r.ended_ms.checked_sub(r.started_ms))
                .collect();
            let estimate_ms = median(durations.clone());
            // what the agent really follows: set times and "at every start" from 0.8.21, the
            // interval the NAS sends instead to an older one
            let (shared, every) = crate::pc_agent::plan_trigger(plan, &rynas_power::system_tz(), now, rynas_api_types::pcagent::agent_follows_calendar(&d.agent_version));
            let (trigger, every_h) = match shared {
                TaskTrigger::Interval { .. } => (None, Some(every)),
                t => (Some(t), None),
            };
            jobs.push(JobInput {
                job: OverviewJob {
                    id: crate::pc_agent::task_id_of(name),
                    name: pc_name(name, d),
                    owner: OverviewOwner::PcBackup,
                    action: None,
                    enabled: !plan.paused,
                    system: false,
                    paused: plan.paused,
                    estimate_ms,
                    estimate_runs: durations.len().min(255) as u8,
                    resources: Vec::new(),
                    maintenance: false,
                    trigger,
                    every_h,
                    // an agent older than 0.8.20 ignores the window: drawn as it runs
                    window: plan
                        .window
                        .clone()
                        .filter(|w| week_window_valid(w) && rynas_api_types::pcagent::agent_follows_window(&d.agent_version)),
                    truncated: false,
                },
                task: None,
                claims: Vec::new(),
                resources: Vec::new(),
                last_ms: d.last_ok_ms,
                // the interval as this PC's agent counts it (from the run's start from 0.8.24)
                interval_from_ms: crate::pc_agent::last_ok(d).map(|l| l.started_ms),
                from_start: rynas_api_types::pcagent::agent_counts_from_start(&d.agent_version),
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

/// How a PC is named on the Schedule page: its description, else its host name, else its
/// device name.
fn pc_name(device: &str, d: &crate::pc_agent::Dev) -> String {
    let shown = if d.settings.description.is_empty() { d.hostname.clone() } else { d.settings.description.clone() };
    if shown.is_empty() { device.to_string() } else { shown }
}

fn median(mut v: Vec<u64>) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2 })
}

/// A window stored inside the `scheduler` key (a development build of S5) moves to its own
/// key: applied there (unless one is set already), then dropped from the `scheduler`
/// document. Nothing to do otherwise (no apply).
pub fn migrate_window(cfg: &rynas_config::Config, log: &dyn Fn(&str)) -> Result<(), String> {
    let cur = cfg.applied(SCHEDULER_KEY).map_err(|e| e.to_string())?;
    let Some(old) = cur.as_ref().and_then(|v| v.get("maintenance")).and_then(|w| w.as_str()).map(str::to_string) else {
        return Ok(());
    };
    if cfg.applied(MAINTENANCE_KEY).map_err(|e| e.to_string())?.is_none() && week_window_valid(&old) {
        let v = serde_json::to_value(MaintenanceWindow { window: Some(old) }).map_err(|e| e.to_string())?;
        cfg.apply_value("rynas", MAINTENANCE_KEY, Some(v), "maintenance window moved to its own key", log)
            .map_err(|e| e.to_string())?;
    }
    cfg.apply_update("rynas", SCHEDULER_KEY, "maintenance window moved to its own key", log, |cur| {
        let Some(mut v) = cur else { return Ok(None) };
        match v.as_object_mut().and_then(|o| o.remove("maintenance")) {
            Some(_) => Ok(Some(v)),
            None => Ok(None),
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

async fn overview(State(st): State<ProtState>, Query(q): Query<Range>) -> Res<Json<SchedulerOverview>> {
    let p = prot(&st)?;
    let from = q.from.unwrap_or_else(now_ms);
    let to = q.to.unwrap_or(from.saturating_add(7 * 86_400_000));
    if to < from {
        return Err(invalid("the end of the range is before its start"));
    }
    if to - from > rynas_scheduler::overview::MAX_RANGE_MS {
        return Err(invalid("the range is at most 35 days"));
    }
    let rt = p.runtime().await;
    Ok(Json(blocking(&p, move |p| overview_of(p, &rt, from, to)).await?))
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
        && !t.strip_prefix("pc:").is_some_and(rynas_api_types::pcagent::valid_device)
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
    // the PCs' backups (`pc:<device>`): the PC's description or host name, as the overview
    let pcs = p.ops.pc_agent.get().and_then(std::sync::Weak::upgrade).map(|a| a.snapshot());
    let mut out = BTreeMap::new();
    for r in runs {
        if out.contains_key(&r.task_id) {
            continue;
        }
        if let Some(device) = r.task_id.strip_prefix("pc:") {
            let d = pcs.as_ref().and_then(|reg| reg.devices.get(device));
            out.insert(r.task_id.clone(), d.map(|d| pc_name(device, d)).unwrap_or_else(|| device.to_string()));
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

/// `PUT /v1/scheduler/maintenance-window`: applied at once under its own key (no unit, no
/// reload, nobody's pending changes of the `scheduler` key touched; the gate reads the window
/// at every start), with a revision like any apply.
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
        let next = serde_json::to_value(MaintenanceWindow { window: window.clone() }).map_err(Error::internal)?;
        p.ops
            .config
            .apply_update(&who, MAINTENANCE_KEY, comment, &|l| tracing::info!("scheduler: {l}"), |cur| {
                let now = rynas_scheduler::maintenance::window_of(cur.as_ref());
                if now == window && cur.is_some() {
                    return Ok(None);
                }
                Ok(Some(next.clone()))
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
