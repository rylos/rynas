//! Power Schedule endpoints of the internal API (`/v1/power/...`, public under
//! `/api/v1/power/...`) and the scheduled shutdown task.
//!
//! `PUT /v1/power/schedule` stages, in the session user's changeset, the schedule
//! (`power.schedule`) and the scheduler tasks of its shutdown rules (`power-off-<rule>`,
//! `scheduler`) together. The task's unit runs `rynasctl power scheduled-shutdown <rule>`,
//! which calls the internal `POST /v1/power/schedule/rules/{rule}/shutdown` (never forwarded
//! by rynas-web) and follows the rynasd task `power.shutdown`: checks (RAID, other rynas
//! tasks, SMB open files), the rule's policy (wait up to N minutes, skip, force), then a
//! clean `systemctl poweroff`. The RTC alarm for the next startup is written by
//! `rynas-power-wake.service` on the way down, whatever started the shutdown.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
use rynas_api_types::power::{
    PowerAction, PowerNext, PowerRule, PowerRuleStatus, PowerSchedule, PowerScheduleState,
    ShutdownPolicy,
};
use rynas_api_types::scheduler::{
    NotifyPolicy, ScheduledTask, SchedulerConfig, TaskAction, TaskTrigger,
};
use rynas_api_types::{TaskStarted, TaskStatus, error_codes};
use rynas_notify::event::{field, oneshot};
use rynas_power::POWER_KEY;
use rynas_power::policy::{Decision, RECHECK_S, decide, mdstat_busy, smb_open_files};
use rynas_power::rules::{self, normalize, on_calendar, task_id, valid_rule_id};
use rynas_power::wake::{self, PowerState};
use rynas_scheduler::SCHEDULER_KEY;
use rynas_scheduler::units::service_name;
use rynas_tasks::Cmd;

use crate::hardware::{Hardware, HwState, blocking, hw};
use crate::ops::{Error, Res, user};
use crate::services::{docs, pending_op, stage};

pub fn router(st: HwState) -> Router {
    Router::new()
        .route("/v1/power/schedule", get(schedule_get).put(schedule_put))
        .route("/v1/power/next", get(next_get))
        // internal only (rynasctl as root): never forwarded by rynas-web
        .route(
            "/v1/power/schedule/rules/{rule}/shutdown",
            post(scheduled_shutdown),
        )
        .with_state(st)
}

fn invalid(m: impl Into<String>) -> Error {
    Error::new(
        StatusCode::BAD_REQUEST,
        error_codes::INVALID_VALUE,
        m.into(),
    )
}

fn ms(t: jiff::Timestamp) -> u64 {
    t.as_millisecond().max(0) as u64
}

/// Next startup and shutdown of `s` (the applied schedule) with the RTC state.
pub fn next_of(h: &Hardware, s: &PowerSchedule) -> PowerNext {
    let tz = rynas_power::system_tz();
    let now = jiff::Timestamp::now();
    let up = rules::next(&s.rules, PowerAction::Startup, now, &tz);
    let down = rules::next(&s.rules, PowerAction::Shutdown, now, &tz);
    let state = PowerState::load(&h.state_dir);
    PowerNext {
        next_startup_ms: up.as_ref().map(|(t, _)| ms(*t)),
        next_startup_rule: up.map(|(_, r)| r),
        next_shutdown_ms: down.as_ref().map(|(t, _)| ms(*t)),
        next_shutdown_rule: down.map(|(_, r)| r),
        time_zone: rynas_power::tz_name(&tz),
        wake: wake::support(&h.hw_root, &state),
        last_boot: state.boot,
    }
}

fn applied_schedule(h: &Hardware) -> Res<PowerSchedule> {
    crate::services::from_value(h.ops.config.applied(POWER_KEY)?)
}

fn schedule_state(h: &Hardware, owner: &str) -> Res<PowerScheduleState> {
    let c = &h.ops.config;
    let (eff, cur, _): (PowerSchedule, PowerSchedule, Option<PowerSchedule>) =
        docs(c, owner, POWER_KEY)?;
    let mut rules: Vec<PowerRuleStatus> = eff
        .rules
        .iter()
        .map(|r| PowerRuleStatus {
            id: r.id.clone(),
            pending: pending_op(Some(r), cur.rules.iter().find(|x| x.id == r.id)),
            task_id: (r.action == PowerAction::Shutdown).then(|| task_id(&r.id)),
        })
        .collect();
    for r in cur
        .rules
        .iter()
        .filter(|r| !eff.rules.iter().any(|x| x.id == r.id))
    {
        rules.push(PowerRuleStatus {
            id: r.id.clone(),
            pending: pending_op(None, Some(r)),
            task_id: None,
        });
    }
    let applied = applied_schedule(h)?;
    Ok(PowerScheduleState {
        next: next_of(h, &applied),
        pending: (eff != cur).then_some(eff),
        applied: cur,
        rules,
    })
}

async fn schedule_get(State(st): State<HwState>, hd: HeaderMap) -> Res<Json<PowerScheduleState>> {
    let (h, owner) = (hw(&st)?, user(&hd)?);
    Ok(Json(
        blocking(&h, move |h| schedule_state(h, &owner)).await?,
    ))
}

/// The scheduler task of a shutdown rule.
pub fn shutdown_task(r: &PowerRule) -> ScheduledTask {
    let p = r.shutdown.unwrap_or_default();
    let time = format!("{:02}:{:02}", r.hour, r.minute);
    ScheduledTask {
        id: task_id(&r.id),
        name: format!("Scheduled shutdown {} ({time})", r.id),
        description: "Power schedule rule: checks the system, then powers off. Edit it from the power schedule.".into(),
        enabled: r.enabled,
        action: TaskAction::PowerShutdown { rule: r.id.clone() },
        trigger: TaskTrigger::Calendar {
            on_calendar: on_calendar(r),
            cron: None,
            // a shutdown missed while the NAS was off must not happen at the next boot
            persistent: false,
            randomized_delay_s: 0,
        },
        // the longest wait plus the poweroff itself
        timeout_s: Some(p.max_wait_min as u64 * 60 + 600),
        nice: None,
        io_weight: None,
        notify: NotifyPolicy::OnFailure,
        notify_output: false,
    }
}

async fn schedule_put(
    State(st): State<HwState>,
    hd: HeaderMap,
    body: Result<Json<PowerSchedule>, JsonRejection>,
) -> Res<Json<PowerScheduleState>> {
    let (h, owner) = (hw(&st)?, user(&hd)?);
    let Json(req) = body?;
    let prot = h.protection.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "the task scheduler is not enabled in this rynasd".into(),
        )
    })?;
    Ok(Json(
        blocking(&h, move |h| {
            let s = normalize(req).map_err(invalid)?;
            let c = &h.ops.config;
            let (mut sched, _, _): (SchedulerConfig, SchedulerConfig, _) =
                docs(c, &owner, SCHEDULER_KEY)?;
            let want: Vec<ScheduledTask> = s
                .rules
                .iter()
                .filter(|r| r.action == PowerAction::Shutdown)
                .map(shutdown_task)
                .collect();
            for t in &want {
                if sched
                    .tasks
                    .iter()
                    .any(|x| x.id == t.id && !matches!(x.action, TaskAction::PowerShutdown { .. }))
                {
                    return Err(Error::new(
                        StatusCode::CONFLICT,
                        error_codes::ALREADY_EXISTS,
                        format!("scheduler task {} already exists", t.id),
                    ));
                }
            }
            // calendars checked by systemd-analyze like every task
            let checked: Vec<ScheduledTask> = want
                .into_iter()
                .map(|t| crate::scheduler::checked_task(&prot, &owner, t))
                .collect::<Res<_>>()?;
            sched
                .tasks
                .retain(|x| !matches!(x.action, TaskAction::PowerShutdown { .. }));
            sched.tasks.extend(checked);
            stage(c, &owner, POWER_KEY, &s)?;
            stage(c, &owner, SCHEDULER_KEY, &sched)?;
            schedule_state(h, &owner)
        })
        .await?,
    ))
}

async fn next_get(State(st): State<HwState>) -> Res<Json<PowerNext>> {
    let h = hw(&st)?;
    Ok(Json(
        blocking(&h, |h| {
            let s = applied_schedule(h)?;
            Ok(next_of(h, &s))
        })
        .await?,
    ))
}

// ---------------------------------------------------------------------------------------------
// the scheduled shutdown
// ---------------------------------------------------------------------------------------------

const SMBSTATUS_TIMEOUT: Duration = Duration::from_secs(15);

/// One round of the enabled checks: what keeps the NAS busy.
async fn busy_reasons(h: &Arc<Hardware>, p: &ShutdownPolicy, me: &str, unit: &str) -> Vec<String> {
    let mut busy = Vec::new();
    if p.check_storage {
        let mdstat = rynas_hw::under(&h.hw_root, "/proc/mdstat");
        let text = tokio::fs::read_to_string(mdstat).await.unwrap_or_default();
        busy.extend(mdstat_busy(&text));
        for t in h.ops.tasks.list(500) {
            if t.status == TaskStatus::Running && t.kind.contains("scrub") {
                busy.push(format!("{} running ({})", t.kind, t.id));
            }
        }
    }
    if p.check_tasks {
        for t in h.ops.tasks.list(500) {
            if t.status == TaskStatus::Running && t.id != me && !t.kind.contains("scrub") {
                busy.push(format!("rynas task {} running", t.kind));
            }
        }
        if let Some(prot) = &h.protection {
            for u in prot.runtime().await.running.iter().filter(|u| *u != unit) {
                busy.push(format!("scheduler task {u} running"));
            }
        }
    }
    if p.check_smb {
        let cmd = Cmd::new("smbstatus")
            .args(["-L", "--json"])
            .env_clear()
            .timeout(SMBSTATUS_TIMEOUT);
        match cmd.run(None, None).await {
            Ok(o) if o.success() => match smb_open_files(&o.stdout) {
                Ok(0) => {}
                Ok(n) => busy.push(format!("{n} files open over SMB")),
                Err(e) => tracing::warn!("scheduled shutdown: {e}"),
            },
            Ok(o) => tracing::warn!(
                "scheduled shutdown: smbstatus exit code {:?}: {}",
                o.code,
                o.stderr.trim()
            ),
            // no Samba on this machine: nothing to wait for
            Err(e) => tracing::debug!("scheduled shutdown: smbstatus: {e}"),
        }
    }
    busy
}

/// `rynasctl power scheduled-shutdown <rule>`: starts the task `power.shutdown`.
async fn scheduled_shutdown(
    State(st): State<HwState>,
    hd: HeaderMap,
    Path(rule): Path<String>,
) -> Res<Response> {
    let (h, who) = (hw(&st)?, user(&hd)?);
    if !valid_rule_id(&rule) {
        return Err(Error::bad_request("invalid rule id"));
    }
    let r2 = rule.clone();
    let r = blocking(&h, move |h| {
        Ok(applied_schedule(h)?
            .rules
            .into_iter()
            .find(|r| r.id == r2 && r.action == PowerAction::Shutdown))
    })
    .await?
    .ok_or_else(|| Error::not_found(format!("applied shutdown rule {rule}")))?;
    let h2 = h.clone();
    let id = h
        .ops
        .tasks
        .spawn("power.shutdown", &who, move |ctx| async move {
            let h = h2;
            if !r.enabled {
                ctx.log(&format!("rule {} is disabled: nothing to do", r.id));
                return Ok(());
            }
            let p = r.shutdown.unwrap_or_default();
            let unit = service_name(&task_id(&r.id));
            let mut waited = 0u64;
            loop {
                let busy = busy_reasons(&h, &p, ctx.id(), &unit).await;
                for b in &busy {
                    ctx.log(&format!("busy: {b}"));
                }
                match decide(&p, &busy, waited) {
                    Decision::PowerOff => {
                        if !busy.is_empty() {
                            ctx.log("policy force: shutting down anyway");
                        }
                        break;
                    }
                    Decision::Wait => {
                        ctx.log(&format!(
                            "waiting (up to {} min, {} min so far)",
                            p.max_wait_min,
                            waited / 60
                        ));
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(RECHECK_S)) => {}
                            _ = ctx.token().cancelled() => return Err("cancelled".into()),
                        }
                        waited += RECHECK_S;
                    }
                    Decision::Skip => {
                        ctx.log("scheduled shutdown skipped");
                        let ev = oneshot(
                            NotifyCategory::System,
                            NotifySeverity::Warning,
                            "power",
                            format!("power:shutdown:{}:skipped", r.id),
                            format!("Scheduled shutdown {} skipped", r.id),
                        );
                        let mut ev = field(ev, "rule", &r.id);
                        ev.body = format!("The NAS stayed on: {}.", busy.join("; "));
                        h.notifier.emit(ev);
                        return Ok(());
                    }
                }
            }
            let mut ev = oneshot(
                NotifyCategory::System,
                NotifySeverity::Info,
                "power",
                format!("power:shutdown:{}", r.id),
                format!("Scheduled shutdown {}", r.id),
            );
            ev.body = "The NAS is shutting down as scheduled.".into();
            h.notifier.emit(ev);
            let Some((prog, args)) = h.poweroff.split_first() else {
                return Err("no poweroff command".into());
            };
            ctx.log("powering off");
            ctx.run(
                &Cmd::new(prog)
                    .args(args)
                    .env_clear()
                    .timeout(Duration::from_secs(60)),
            )
            .await
            .map(|_| ())
            .map_err(|e| format!("poweroff: {e}"))
        })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}
