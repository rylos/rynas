//! rynasd's half of the scheduling gate ([`rynas_scheduler::gate`]): the answer to
//! `rynasctl gate <id>` (`ExecCondition=` of every task unit), the pauses, the leases of the
//! starts it let through, and a watch loop for retries, the overlap policy `replace` and
//! pauses that end by themselves.
//!
//! - **Pauses** live in `<state>/scheduler/pause.json` (also read by rynasctl when rynasd
//!   cannot be reached), changed at once (not staged: a pause is an action, like "run now").
//! - **Leases**: one per task, granted with a start (its `$INVOCATION_ID`), released by
//!   `task-finished`. The lease carries what the run holds in rynasd: the rynasd task
//!   started by `rynasctl job run` (its UPID), and the filesystem lock of a run that works in
//!   its own unit (a scrub: deduplication gives way, [`crate::fslock`]). A second start of
//!   the task while that rynasd task still runs is skipped (`running`). After a restart of
//!   rynasd the leases are rebuilt from the task units systemd says are running
//!   ([`Gate::rebuild`]), so nothing is persisted that could go stale.
//! - **Requests**: "run now" and the retries tell the gate what is about to start, so the
//!   start is recorded as such and is not held back by a pause or a deadline (a retry is).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jiff::tz::TimeZone;
use rynas_api_types::TaskStatus as UpidStatus;
use rynas_api_types::scheduler::{
    PauseInfo, RunTrigger, ScheduledTask, SchedulerPause, SkipReason, TaskAction,
};
use rynas_notify::event::{code, field, oneshot};
use rynas_notify::{NotifyCategory, NotifySeverity};
use rynas_scheduler::gate::{
    Decision, GateAnswer, GateRequest, Start, decide, load_pause, replace_due, save_pause,
    spool_take,
};
use rynas_scheduler::history::{ArmedRetry, GateNote, NewRun};
use rynas_scheduler::units::{UNIT_PREFIX, service_name, timer_name};
use rynas_tasks::{Cmd, now_ms};

use crate::fslock::HeavyGuard;
use crate::ops::{Error, Res};
use crate::protection::Protection;

/// How long a start rynasd asked for waits for its gate.
const REQUEST_TTL: Duration = Duration::from_secs(120);
/// Period of the watch loop.
const WATCH: Duration = Duration::from_secs(5);

struct Lease {
    invocation_id: String,
    /// The rynasd task the run started (`rynasctl job run`).
    upid: Option<String>,
    /// The filesystem lock of a run working in its own unit.
    _guard: Option<HeavyGuard>,
    /// The task action takes the heavy lock (docker updates wait for it).
    heavy: bool,
}

pub struct Gate {
    state: PathBuf,
    pause: Mutex<SchedulerPause>,
    requested: Mutex<HashMap<String, (RunTrigger, u8, Instant)>>,
    leases: Mutex<HashMap<String, Lease>>,
}

impl Gate {
    /// `state`: rynasd's state directory (the pauses are below it).
    pub fn new(state: PathBuf) -> Self {
        let pause = load_pause(&state);
        Self {
            state,
            pause: Mutex::new(pause),
            requested: Mutex::new(HashMap::new()),
            leases: Mutex::new(HashMap::new()),
        }
    }

    /// The pauses in force (ended ones dropped, and the file rewritten then).
    pub fn pause(&self) -> SchedulerPause {
        let mut p = self.pause.lock().unwrap();
        if p.expire(now_ms())
            && let Err(e) = save_pause(&self.state, &p)
        {
            tracing::warn!("scheduler pause: {e}");
        }
        p.clone()
    }

    fn change_pause(&self, f: impl FnOnce(&mut SchedulerPause)) -> Res<SchedulerPause> {
        let mut p = self.pause.lock().unwrap();
        let mut next = p.clone();
        f(&mut next);
        next.expire(now_ms());
        save_pause(&self.state, &next).map_err(Error::internal)?;
        *p = next.clone();
        Ok(next)
    }

    /// Pauses every task (`Some`) or resumes them (`None`).
    pub fn set_pause_all(&self, info: Option<PauseInfo>) -> Res<SchedulerPause> {
        self.change_pause(|p| p.all = info)
    }

    /// Pauses (`Some`) or resumes (`None`) one task.
    pub fn set_pause_task(&self, id: &str, info: Option<PauseInfo>) -> Res<SchedulerPause> {
        self.change_pause(|p| match info {
            Some(i) => {
                p.tasks.insert(id.to_string(), i);
            }
            None => {
                p.tasks.remove(id);
            }
        })
    }

    /// The next start of `id` is one rynasd asked for.
    pub fn request(&self, id: &str, trigger: RunTrigger, attempt: u8) {
        self.requested
            .lock()
            .unwrap()
            .insert(id.to_string(), (trigger, attempt, Instant::now()));
    }

    fn take_request(&self, id: &str) -> Option<(RunTrigger, u8)> {
        let mut r = self.requested.lock().unwrap();
        r.retain(|_, (_, _, at)| at.elapsed() < REQUEST_TTL);
        r.remove(id).map(|(t, a, _)| (t, a))
    }

    fn grant(&self, id: &str, invocation_id: &str, heavy: bool) {
        self.leases.lock().unwrap().insert(
            id.to_string(),
            Lease {
                invocation_id: invocation_id.to_string(),
                upid: None,
                _guard: None,
                heavy,
            },
        );
    }

    /// The run of `id` started the rynasd task `upid`.
    pub fn attach_upid(&self, id: &str, upid: &str) {
        if let Some(l) = self.leases.lock().unwrap().get_mut(id) {
            l.upid = Some(upid.to_string());
        }
    }

    /// The run of `id` holds a filesystem lock until it ends.
    pub fn attach_guard(&self, id: &str, guard: HeavyGuard) {
        let mut l = self.leases.lock().unwrap();
        match l.get_mut(id) {
            Some(lease) => lease._guard = Some(guard),
            // a start without the gate (rynasd was down then): a lease of its own
            None => {
                l.insert(
                    id.to_string(),
                    Lease { invocation_id: String::new(), upid: None, _guard: Some(guard), heavy: true },
                );
            }
        }
    }

    /// The run `invocation_id` of `id` ended: its lease goes (the lock with it).
    pub fn release(&self, id: &str, invocation_id: &str) {
        let mut l = self.leases.lock().unwrap();
        if l.get(id).is_some_and(|x| x.invocation_id == invocation_id || x.invocation_id.is_empty()) {
            l.remove(id);
        }
    }

    /// An earlier start of `id` still runs in rynasd (its rynasd task).
    fn running_elsewhere(&self, id: &str, invocation_id: &str, tasks: &rynas_tasks::TaskManager) -> bool {
        let l = self.leases.lock().unwrap();
        let Some(lease) = l.get(id).filter(|x| x.invocation_id != invocation_id) else {
            return false;
        };
        lease.upid.as_deref().and_then(|u| tasks.get(u)).is_some_and(|t| {
            matches!(t.status, UpidStatus::Running | UpidStatus::Queued)
        })
    }

    /// Tasks whose run holds a filesystem for heavy work now (for other features' "busy"
    /// checks: they no longer show as rynasd tasks).
    pub fn heavy_runs(&self) -> Vec<String> {
        self.leases
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, l)| l.heavy)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// After a start of rynasd: a lease for every task unit that runs now (with the lock of
    /// those that work in their own unit).
    pub async fn rebuild(p: &Arc<Protection>) {
        let p2 = p.clone();
        let found = tokio::task::spawn_blocking(move || -> Vec<(ScheduledTask, String)> {
            let running = p2.probe.running_tasks().unwrap_or_default();
            running
                .iter()
                .filter_map(|unit| {
                    let id = unit.strip_prefix(UNIT_PREFIX)?.strip_suffix(".service")?;
                    let run = p2.probe.current_run(unit).ok().flatten()?;
                    let t = crate::scheduler::applied_task(&p2, id).ok().flatten()?;
                    Some((t, run.invocation_id))
                })
                .collect()
        })
        .await
        .unwrap_or_default();
        for (t, inv) in found {
            p.gate.grant(&t.id, &inv, heavy_action(&t.action));
            if let TaskAction::Scrub { filesystem } = &t.action {
                let g = crate::fslock::FsLocks::global().heavy_paths(&[filesystem], "btrfs.scrub").await;
                p.gate.attach_guard(&t.id, g);
            }
            tracing::info!("scheduler: task {} runs ({inv}): lease rebuilt", t.id);
        }
    }
}

/// The gate of this rynasd once its watch loop runs (for the busy checks of other features).
static RUNNING: std::sync::OnceLock<Arc<Gate>> = std::sync::OnceLock::new();

/// Scheduled tasks doing heavy work now (a scrub in its own unit, a backup...): what other
/// features' busy checks must see besides rynasd's own tasks.
pub fn heavy_runs_now() -> Vec<String> {
    RUNNING.get().map(|g| g.heavy_runs()).unwrap_or_default()
}

/// Actions that take the filesystem lock for heavy work.
pub fn heavy_action(a: &TaskAction) -> bool {
    matches!(
        a,
        TaskAction::Scrub { .. } | TaskAction::Replicate { .. } | TaskAction::Backup { .. }
    )
}

fn reason_text(r: SkipReason) -> &'static str {
    match r {
        SkipReason::PausedAll => "every task is paused",
        SkipReason::Paused => "the task is paused",
        SkipReason::Missed => "a missed run past its catch-up deadline",
        SkipReason::Overlap => "its time came while the previous run was going (overlap policy skip)",
        SkipReason::Running => "the previous run is still going",
    }
}

/// Records what rynasctl spooled while rynasd could not be reached: skipped starts, starts
/// that ran without the gate, ends of runs (with their output from the journal).
pub fn import_spool(p: &Protection) {
    for e in spool_take(&p.state_dir) {
        let trigger = if e.by_timer { RunTrigger::Timer } else { RunTrigger::Other };
        if let Some(f) = &e.finished {
            let end = crate::scheduler::TaskFinished {
                invocation_id: e.invocation_id.clone(),
                result: f.result.clone(),
                exit_code: f.exit_code.clone(),
                exit_status: f.exit_status.clone(),
            };
            match crate::scheduler::record_end(p, &e.task_id, &end, None, e.at_ms) {
                Ok(Some(run)) => crate::scheduler::collect_output(p, &e.task_id, run),
                Ok(None) => {}
                Err(err) => tracing::warn!("scheduler: spooled end of {}: {}", e.task_id, err.2),
            }
            continue;
        }
        let r = match e.skipped {
            Some(reason) => p.history.record(&NewRun::skipped(
                &e.task_id,
                &e.invocation_id,
                e.at_ms,
                trigger,
                reason,
                Some("rynas was not reachable: the pause file decided".into()),
            )),
            None => p
                .history
                .note_gate(&GateNote {
                    invocation_id: e.invocation_id.clone(),
                    task_id: e.task_id.clone(),
                    at_ms: e.at_ms,
                    trigger: Some(trigger),
                    attempt: 0,
                    ungated: true,
                })
                .map(|_| 0),
        };
        if let Err(err) = r {
            tracing::warn!("scheduler: spooled gate outcome of {}: {err}", e.task_id);
        }
    }
}

/// The gate: `POST /v1/scheduler/tasks/{id}/gate` (root only, from `rynasctl gate`).
pub fn answer(p: &Protection, id: &str, req: &GateRequest) -> Res<GateAnswer> {
    import_spool(p);
    let now = now_ms();
    let Some(task) = crate::scheduler::applied_task(p, id)? else {
        // a unit left by an apply in progress: as before the gate
        return Ok(GateAnswer {
            run: true,
            trigger: RunTrigger::Other,
            skipped: None,
            message: format!("rynas: task {id} is not in the applied configuration; running it"),
        });
    };
    let requested = p.gate.take_request(id);
    let pause = p.gate.pause();
    let tz = TimeZone::system();
    let last_run = p.history.last_ran(id).map_err(Error::internal)?;
    let start = Start {
        task: &task,
        now_ms: now,
        by_timer: req.trigger_unit == timer_name(id),
        requested,
        last_run,
        pause: &pause,
        running_elsewhere: p.gate.running_elsewhere(id, &req.invocation_id, &p.ops.tasks),
        tz: &tz,
    };
    match decide(&start) {
        Decision::Run { trigger, attempt } => {
            p.gate.grant(id, &req.invocation_id, heavy_action(&task.action));
            p.history
                .note_gate(&GateNote {
                    invocation_id: req.invocation_id.clone(),
                    task_id: id.to_string(),
                    at_ms: now,
                    trigger: Some(trigger),
                    attempt,
                    ungated: false,
                })
                .map_err(Error::internal)?;
            let message = match trigger {
                RunTrigger::Retry => format!(
                    "rynas: retry {attempt} of {} after a failed run",
                    task.effective_policy().retry.attempts
                ),
                RunTrigger::Queued => {
                    "rynas: its time came while the previous run was going: starting now (overlap policy queue)".into()
                }
                RunTrigger::CatchUp => "rynas: catching up a run missed while the NAS was off".into(),
                _ => String::new(),
            };
            Ok(GateAnswer { run: true, trigger, skipped: None, message })
        }
        Decision::Skip { trigger, reason, detail } => {
            let run = NewRun::skipped(id, &req.invocation_id, now, trigger, reason, detail.clone());
            p.history.record(&run).map_err(Error::internal)?;
            p.invalidate();
            if reason == SkipReason::Missed
                && task.effective_policy().rules.on_missed
            {
                p.notifier.emit(missed_event(&task));
            }
            let mut message = format!("rynas: start skipped: {}", reason_text(reason));
            if let Some(d) = detail {
                message.push_str(&format!(" ({d})"));
            }
            Ok(GateAnswer { run: false, trigger, skipped: Some(reason), message })
        }
    }
}

fn missed_event(task: &ScheduledTask) -> rynas_notify::NewEvent {
    let name = if task.name.is_empty() { task.id.clone() } else { task.name.clone() };
    let mut ev = oneshot(
        NotifyCategory::Task,
        NotifySeverity::Warning,
        "scheduler",
        format!("scheduler:task:{}:missed", task.id),
        format!("Task {name}: missed run not caught up"),
    );
    ev.body = "The NAS was off at its scheduled time and came back after the catch-up deadline.".into();
    let ev = code(ev, "sched.task_missed");
    field(field(ev, "task", &task.id), "name", name)
}

/// After a run ended: arms its retry, or drops the armed one. `run` is the recorded run with
/// the trigger and attempt from the gate.
pub fn after_run(p: &Protection, task: &ScheduledTask, run: &NewRun) {
    if run.ok() {
        let _ = p.history.clear_retry(&task.id);
        return;
    }
    if run.skipped.is_some() || !rynas_scheduler::gate::retries_after(run.trigger) {
        return;
    }
    let policy = task.effective_policy();
    let next = rynas_scheduler::gate::next_retry(&policy.retry, run.attempt, run.end_ms);
    let r = match next {
        Some(retry) => {
            tracing::info!(
                "scheduler: task {} failed; retry {} of {} at {}",
                task.id,
                retry.attempt,
                retry.attempts,
                retry.due_ms
            );
            p.history.set_retry(&task.id, &ArmedRetry { retry, after_ms: run.end_ms })
        }
        None => p.history.clear_retry(&task.id).map(|_| ()),
    };
    if let Err(e) = r {
        tracing::warn!("scheduler: retry of {}: {e}", task.id);
    }
}

/// A retry is armed for the task: a failure notification waits for the last attempt.
pub fn retry_pending(p: &Protection, id: &str) -> bool {
    p.history
        .retries()
        .is_ok_and(|l| l.iter().any(|(t, _)| t == id))
}

/// The watch loop: due retries, the overlap policy `replace`, pauses that end by themselves.
pub async fn watch(p: Arc<Protection>) {
    let _ = RUNNING.set(p.gate.clone());
    Gate::rebuild(&p).await;
    loop {
        tokio::time::sleep(WATCH).await;
        let p2 = p.clone();
        let _ = tokio::task::spawn_blocking(move || tick(&p2)).await;
    }
}

fn tick(p: &Protection) {
    import_spool(p);
    let now = now_ms();
    let due: Vec<(String, ArmedRetry)> = p
        .history
        .retries()
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, a)| a.retry.due_ms <= now)
        .collect();
    let applied: Vec<ScheduledTask> = p
        .ops
        .config
        .applied(rynas_scheduler::SCHEDULER_KEY)
        .ok()
        .flatten()
        .map(|v| rynas_scheduler::domain::tasks_of(Some(&v)))
        .unwrap_or_default();
    let replacing: Vec<&ScheduledTask> = applied
        .iter()
        .filter(|t| t.effective_policy().overlap == rynas_api_types::scheduler::Overlap::Replace)
        .collect();
    // nothing to do: no systemd question
    if due.is_empty() && replacing.is_empty() {
        return;
    }
    let pause = p.gate.pause();
    let running = p.probe.running_tasks().unwrap_or_default();
    for (id, armed) in due {
        let unit = service_name(&id);
        let task = applied.iter().find(|t| t.id == id);
        // gone, off, or a newer run happened (the schedule, a catch-up, run now)
        let stale = task.is_none_or(|t| !t.enabled)
            || p.history.ran_since(&id, armed.after_ms).unwrap_or(false);
        if stale {
            let _ = p.history.clear_retry(&id);
            continue;
        }
        // it runs now: the retry waits for that run to end
        if running.contains(&unit) {
            continue;
        }
        let _ = p.history.clear_retry(&id);
        if let Some(reason) = pause.blocks(&id, now) {
            // a pause drops the retry (the next scheduled run comes after the pause)
            let inv = format!("retry{:x}{}", now, armed.retry.attempt);
            let mut r = NewRun::skipped(&id, &inv, now, RunTrigger::Retry, reason, None);
            r.attempt = armed.retry.attempt;
            let _ = p.history.record(&r);
            p.invalidate();
            continue;
        }
        p.gate.request(&id, RunTrigger::Retry, armed.retry.attempt);
        match p.ops.config.system().start_unit(&unit) {
            Ok(()) => tracing::info!("scheduler: task {id}: retry {} started", armed.retry.attempt),
            Err(e) => tracing::warn!("scheduler: task {id}: retry: {e}"),
        }
    }
    // overlap policy replace: a time came after the running one started
    let tz = TimeZone::system();
    for task in replacing {
        let unit = service_name(&task.id);
        if !running.contains(&unit) || pause.blocks(&task.id, now).is_some() {
            continue;
        }
        let Some(start) = p.probe.start_ms(&unit) else { continue };
        if replace_due(task, start, now, &tz) {
            tracing::info!(
                "scheduler: task {}: its time came while it ran: restarting it (overlap policy replace)",
                task.id
            );
            p.gate.request(&task.id, RunTrigger::Timer, 0);
            let _ = p
                .ops
                .config
                .system()
                .run(&Cmd::new("systemctl").args(["restart", "--no-block", &unit]));
        }
    }
}
