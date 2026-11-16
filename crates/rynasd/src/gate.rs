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
//!   A request never applies to a start of the task's own timer, and is used once.
//! - **Retry chains** (`retries` in `scheduler.db`): a failed scheduled run arms the next
//!   attempt and holds its failure notification back; a newer scheduled failure keeps the
//!   chain's count. The chain ends with a success (nothing is sent), with its last attempt
//!   (that run notifies), or dropped (task deleted or off, a pause at the due time, its start
//!   skipped or lost): then the held notification goes, once ([`drop_retry`]).
//! - **Interval tasks** (`TaskTrigger::Interval`, S3): their timer starts them after boot and
//!   an interval after each run; the gate skips a start whose interval (counted from the
//!   history, so across restarts and reboots) has not passed, without a row, and the watch
//!   loop starts the task once it is due ([`due_intervals`]), never twice within
//!   [`INTERVAL_RESTART`].
//! - **Late answers**: an answer decided after rynasctl's deadline
//!   ([`GateRequest::answer_by_ms`]) says "run" and records no skip, since rynasctl may have
//!   run the start already.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jiff::tz::TimeZone;
use rynas_api_types::TaskStatus as UpidStatus;
use rynas_api_types::scheduler::{
    PauseInfo, RunTrigger, ScheduledTask, SchedulerPause, SkipReason, TaskAction, TaskTrigger,
};
use rynas_notify::event::{code, field, oneshot};
use rynas_notify::{NewEvent, NotifyCategory, NotifySeverity};
use rynas_scheduler::gate::{
    Decision, GateAnswer, GateRequest, Start, decide, load_pause, next_retry, replace_due,
    retries_after, save_pause, spool_take,
};
use rynas_scheduler::history::{ArmedRetry, GateNote, NewRun};
use rynas_scheduler::units::{UNIT_PREFIX, runs_in_unit, service_name, timer_name};
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
    /// The task panel's entry of a run working in its own unit ([`crate::jobs::track`]):
    /// told systemd's result when the run ends.
    tracker: Option<tokio::sync::oneshot::Sender<String>>,
}

impl Lease {
    fn new(invocation_id: &str, heavy: bool) -> Self {
        Lease { invocation_id: invocation_id.to_string(), upid: None, _guard: None, heavy, tracker: None }
    }
}

pub struct Gate {
    state: PathBuf,
    pause: Mutex<SchedulerPause>,
    /// trigger, attempt, asked at (for the TTL), asked at in ms since the epoch
    requested: Mutex<HashMap<String, (RunTrigger, u8, Instant, u64)>>,
    leases: Mutex<HashMap<String, Lease>>,
    /// The applied task ids at the last watch tick (a task gone since was deleted).
    known: Mutex<Option<HashSet<String>>>,
    /// When the watch loop last started each interval task that was due (it never starts
    /// one twice within [`INTERVAL_RESTART`], whatever the history says).
    interval_started: Mutex<HashMap<String, IntervalStart>>,
}

/// The watch loop's last start of an interval task: when, the last run's end it saw then, how
/// many starts in a row left that end where it was.
type IntervalStart = (Instant, Option<u64>, u32);

/// The history of a task could not be read.
#[derive(Debug)]
pub struct HistoryUnreadable;

/// The watch loop starts a due interval task at most this often.
const INTERVAL_RESTART: Duration = Duration::from_secs(600);

impl Gate {
    /// `state`: rynasd's state directory (the pauses are below it).
    pub fn new(state: PathBuf) -> Self {
        let pause = load_pause(&state);
        Self {
            state,
            pause: Mutex::new(pause),
            requested: Mutex::new(HashMap::new()),
            leases: Mutex::new(HashMap::new()),
            known: Mutex::new(None),
            interval_started: Mutex::new(HashMap::new()),
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
            .insert(id.to_string(), (trigger, attempt, Instant::now(), now_ms()));
    }

    /// Takes the request of `id` (used once, whoever started the unit), with when it was
    /// made (ms since the epoch).
    fn take_request(&self, id: &str) -> Option<(RunTrigger, u8, u64)> {
        let mut r = self.requested.lock().unwrap();
        r.retain(|_, (_, _, at, _)| at.elapsed() < REQUEST_TTL);
        r.remove(id).map(|(t, a, _, ms)| (t, a, ms))
    }

    /// A start of `id` that rynasd asked for is still waiting for its gate.
    fn requested_now(&self, id: &str) -> bool {
        self.requested
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|(_, _, at, _)| at.elapsed() < REQUEST_TTL)
    }

    /// The watch loop may start the interval task `id` now; records the start when it may. A
    /// start that left the task's last run where it was (the gate skipped it, the start was
    /// lost) is tried again after [`INTERVAL_RESTART`], then twice as long each time, at most
    /// the task's interval: never a start every few minutes.
    fn interval_may_start(&self, id: &str, base: Option<u64>, t: &ScheduledTask) -> bool {
        let mut m = self.interval_started.lock().unwrap();
        let n = match m.get(id) {
            Some((at, b, n)) if *b == base => {
                let hours = match t.trigger {
                    TaskTrigger::Interval { hours, .. } => u64::from(hours.max(1)),
                    _ => 24,
                };
                let wait = INTERVAL_RESTART.saturating_mul(1u32 << (*n).min(10)).min(Duration::from_secs(hours * 3600));
                if at.elapsed() < wait {
                    return false;
                }
                n + 1
            }
            _ => 0,
        };
        m.insert(id.to_string(), (Instant::now(), base, n));
        true
    }

    fn grant(&self, id: &str, invocation_id: &str, heavy: bool) {
        self.leases
            .lock()
            .unwrap()
            .insert(id.to_string(), Lease::new(invocation_id, heavy));
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
        // a start without the gate (rynasd was down then): a lease of its own
        l.entry(id.to_string())
            .or_insert_with(|| Lease::new("", true))
            ._guard = Some(guard);
    }

    /// The run of `id` has a task panel entry waiting for its end; returns the run's
    /// invocation id (empty when the gate did not see its start).
    pub fn attach_tracker(
        &self,
        id: &str,
        heavy: bool,
        tracker: tokio::sync::oneshot::Sender<String>,
    ) -> String {
        let mut l = self.leases.lock().unwrap();
        let lease = l.entry(id.to_string()).or_insert_with(|| Lease::new("", heavy));
        lease.tracker = Some(tracker);
        lease.invocation_id.clone()
    }

    /// The run `invocation_id` of `id` ended with systemd's `result`: its lease goes (the
    /// lock with it), its task panel entry learns the result.
    pub fn release(&self, id: &str, invocation_id: &str, result: &str) {
        let lease = {
            let mut l = self.leases.lock().unwrap();
            if l.get(id).is_some_and(|x| x.invocation_id == invocation_id || x.invocation_id.is_empty()) {
                l.remove(id)
            } else {
                None
            }
        };
        if let Some(t) = lease.and_then(|mut x| x.tracker.take()) {
            let _ = t.send(result.to_string());
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
    /// those that work in their own unit, and their task panel entry).
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
            if runs_in_unit(&t.action) {
                crate::jobs::track(p, &t);
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
        SkipReason::Window => "past the start window of its scheduled time",
        SkipReason::NotDue => "not due yet",
    }
}

/// Records what rynasctl spooled while rynasd could not be reached: skipped starts, starts
/// that ran without the gate, ends of runs (with their output from the journal). Each entry
/// is recorded once however often it is imported.
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
                Ok(Some((run, notify))) => crate::scheduler::collect_output(p, &e.task_id, run, notify),
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

/// The gate: `POST /v1/scheduler/tasks/{id}/gate` (root only, from `rynasctl gate`). Quick
/// on purpose (the spool is imported by the watch loop, not here).
pub fn answer(p: &Protection, id: &str, req: &GateRequest) -> Res<GateAnswer> {
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
    // used once, and dropped by a start of the timer (run now on a running task left it);
    // a start rynasd asked for may still name the timer (systemd keeps activation details)
    let asked = p.gate.take_request(id);
    let by_timer = rynas_scheduler::gate::started_by_timer(
        req.trigger_unit == timer_name(id),
        req.trigger_timer_ms,
        asked.map(|(_, _, ms)| ms),
        now,
    );
    let requested = asked.map(|(t, a, _)| (t, a));
    let pause = p.gate.pause();
    let tz = TimeZone::system();
    let interval = matches!(task.trigger, TaskTrigger::Interval { .. });
    let last_run = if interval {
        // a history that cannot be read: an interval start is not due (no extra runs)
        match interval_last(p, &task) {
            Ok(l) => l,
            Err(HistoryUnreadable) => Some((now, now)),
        }
    } else {
        p.history.last_ran(id).map_err(Error::internal)?
    };
    let start = Start {
        task: &task,
        now_ms: now,
        by_timer,
        requested,
        last_run,
        not_before_ms: interval_not_before(&task, now),
        pause: &pause,
        running_elsewhere: p.gate.running_elsewhere(id, &req.invocation_id, &p.ops.tasks),
        tz: &tz,
    };
    let decision = decide(&start);
    let late = req.answer_by_ms > 0 && now_ms() > req.answer_by_ms;
    let (trigger, attempt) = match &decision {
        Decision::Run { trigger, attempt } => (*trigger, *attempt),
        Decision::Skip { trigger, .. } if late => {
            let attempt = if by_timer { 0 } else { requested.map_or(0, |(_, a)| a) };
            tracing::warn!(
                "scheduler: task {id}: the gate decided after rynasctl's deadline: running the start, no skip recorded"
            );
            (*trigger, attempt)
        }
        // nothing due (an interval that has not passed, a window block that had its start):
        // not an event worth a row; the watch loop starts an interval task when it is due
        Decision::Skip { trigger, reason: SkipReason::NotDue, .. } => {
            return Ok(GateAnswer {
                run: false,
                trigger: *trigger,
                skipped: Some(SkipReason::NotDue),
                message: String::new(),
            });
        }
        Decision::Skip { trigger, reason, detail } => {
            let (trigger, reason) = (*trigger, *reason);
            // an interval task's start skipped for the same reason as its latest one: one row
            let repeated = interval
                && p.history.list(id, 1, None).ok().and_then(|v| v.into_iter().next()).is_some_and(|r| {
                    r.skipped == Some(reason) && r.trigger == Some(trigger) && r.detail == *detail
                });
            if !repeated {
                let run = NewRun::skipped(id, &req.invocation_id, now, trigger, reason, detail.clone());
                p.history.record(&run).map_err(Error::internal)?;
            }
            p.invalidate();
            if trigger == RunTrigger::Retry {
                drop_retry(p, id, "its start was skipped");
            }
            if reason == SkipReason::Missed && task.effective_policy().rules.on_missed {
                p.notifier.emit(missed_event(&task));
            }
            let mut message = format!("rynas: start skipped: {}", reason_text(reason));
            if let Some(d) = detail {
                message.push_str(&format!(" ({d})"));
            }
            return Ok(GateAnswer { run: false, trigger, skipped: Some(reason), message });
        }
    };
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
        _ if late => "rynas: the scheduling gate answered late; running the start".into(),
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

fn missed_event(task: &ScheduledTask) -> NewEvent {
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

/// After a run ended: arms its retry, keeps the chain going or ends it. `true`: the run
/// notifies now; `false`: an armed retry holds its failure notification back ([`hold`]).
///
/// - a success ends the chain (the held failure is dropped: the task works again);
/// - the run of a started retry is that retry, whatever the gate saw (a request lost to a
///   restart of rynasd); the chain moves on, or ends with it (then it notifies);
/// - a newer scheduled run that fails while the chain waits keeps the chain's count and time;
/// - a run started by hand notifies as it always did and leaves the chain alone.
pub fn after_run(p: &Protection, task: &ScheduledTask, run: &NewRun) -> bool {
    if run.ok() {
        let _ = p.history.clear_retry(&task.id);
        return true;
    }
    let armed = p.history.retry(&task.id).ok().flatten();
    let done = match armed {
        Some(a) if a.started_ms.is_some() => a.retry.attempt,
        Some(a) if retries_after(run.trigger) => {
            if let Err(e) = p.history.set_retry(&task.id, &ArmedRetry { after_ms: run.end_ms, ..a }) {
                tracing::warn!("scheduler: retry of {}: {e}", task.id);
            }
            return false;
        }
        _ if !retries_after(run.trigger) => return true,
        _ if run.trigger == Some(RunTrigger::Retry) => run.attempt,
        _ => 0,
    };
    let policy = task.effective_policy();
    match next_retry(&policy.retry, done, run.end_ms) {
        Some(retry) => {
            tracing::info!(
                "scheduler: task {} failed; retry {} of {} at {}",
                task.id,
                retry.attempt,
                retry.attempts,
                retry.due_ms
            );
            let armed = ArmedRetry { retry, after_ms: run.end_ms, started_ms: None };
            match p.history.set_retry(&task.id, &armed) {
                Ok(()) => false,
                Err(e) => {
                    tracing::warn!("scheduler: retry of {}: {e}", task.id);
                    true
                }
            }
        }
        // the last attempt failed: its own notification is the chain's
        None => {
            let _ = p.history.clear_retry(&task.id);
            true
        }
    }
}

/// Holds the failure notification of a run in the task's armed retry ([`after_run`] said
/// so); sent at once when the chain ended meanwhile.
pub fn hold(p: &Protection, id: &str, ev: NewEvent) {
    let held = serde_json::to_string(&ev)
        .ok()
        .is_some_and(|j| p.history.hold(id, &j).unwrap_or(false));
    if !held {
        p.notifier.emit(ev);
    }
}

/// Ends the retry chain of `id` without a success: the failure notification it held back is
/// sent now. Exactly once: only the caller that takes the chain's row sends it.
pub fn drop_retry(p: &Protection, id: &str, why: &str) {
    match p.history.take_retry(id) {
        Ok(Some((armed, held))) => {
            tracing::info!("scheduler: task {id}: retry {} dropped: {why}", armed.retry.attempt);
            if let Some(ev) = held.and_then(|j| serde_json::from_str::<NewEvent>(&j).ok()) {
                p.notifier.emit(ev);
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("scheduler: retry of {id}: {e}"),
    }
}

/// The watch loop: due retries, the overlap policy `replace`, pauses that end by themselves.
pub async fn watch(p: Arc<Protection>) {
    let _ = started_ms();
    let _ = RUNNING.set(p.gate.clone());
    Gate::rebuild(&p).await;
    loop {
        tokio::time::sleep(WATCH).await;
        let p2 = p.clone();
        let _ = tokio::task::spawn_blocking(move || tick(&p2)).await;
    }
}

/// Tasks gone from the applied configuration since the last tick (deleted): their pause and
/// their retry chain go.
fn forget_deleted(p: &Protection, applied: &[ScheduledTask]) {
    let now: HashSet<String> = applied.iter().map(|t| t.id.clone()).collect();
    let gone: Vec<String> = {
        let mut known = p.gate.known.lock().unwrap();
        let gone = known
            .as_ref()
            .map(|old| old.difference(&now).cloned().collect())
            .unwrap_or_default();
        *known = Some(now);
        gone
    };
    for id in gone {
        if p.gate.pause().tasks.contains_key(&id) {
            match p.gate.set_pause_task(&id, None) {
                Ok(_) => tracing::info!("scheduler: task {id} deleted: its pause goes"),
                Err(e) => tracing::warn!("scheduler: pause of deleted task {id}: {}", e.2),
            }
        }
        drop_retry(p, &id, "the task was deleted");
    }
}

/// Milliseconds since the epoch when the machine booted (`/proc/uptime`).
fn boot_ms(now: u64) -> u64 {
    let up = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|t| t.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0);
    now.saturating_sub((up * 1000.0) as u64)
}

fn due_intervals<'a>(
    p: &Protection,
    applied: &'a [ScheduledTask],
    retries: &[(String, ArmedRetry)],
    pause: &SchedulerPause,
    now: u64,
) -> Vec<&'a ScheduledTask> {
    let mut out = Vec::new();
    for t in applied.iter().filter(|t| matches!(t.trigger, TaskTrigger::Interval { .. })) {
        // a history that cannot be read: nothing is started (no extra runs)
        let Ok(last) = interval_last(p, t) else { continue };
        let due = interval_due(t, last, now);
        // the Docker page shows the next check: the same time the loop starts it at
        if matches!(t.action, TaskAction::DockerImageCheck)
            && let Some(d) = p.ops.docker.get()
        {
            d.next_check_ms.store(if t.enabled { due } else { 0 }, std::sync::atomic::Ordering::Relaxed);
        }
        if !t.enabled
            || due > now
            || pause.blocks(&t.id, now).is_some()
            || retries.iter().any(|(id, _)| *id == t.id)
            || p.gate.requested_now(&t.id)
            || !p.gate.interval_may_start(&t.id, last.map(|l| l.1), t)
        {
            continue;
        }
        out.push(t);
    }
    out
}

/// When rynasd started (ms since the epoch): an interval task that never ran counts its
/// delay from the later of the boot and this, as the loops it replaces did.
static STARTED_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

pub fn started_ms() -> u64 {
    *STARTED_MS.get_or_init(now_ms)
}

/// The job's own record of its last check, kept by its feature (before the task existed, or a
/// "Check now"): the next automatic one counts from it too.
fn feature_last_ms(p: &Protection, t: &ScheduledTask) -> Option<u64> {
    let docker = p.ops.docker.get()?;
    match t.action {
        TaskAction::DockerImageCheck => Some(docker.report.lock().unwrap().checked_at_ms).filter(|m| *m > 0),
        TaskAction::AppsCatalog => docker.apps.get()?.upgrade()?.online.get()?.last_check_ms(),
        _ => None,
    }
}

/// The last run of an interval task: its latest recorded run, or the feature's own last
/// check when that is later (as `(start, end)`). `Err` when the history cannot be read.
pub fn interval_last(p: &Protection, t: &ScheduledTask) -> Result<Option<(u64, u64)>, HistoryUnreadable> {
    let ran = p.history.last_ran(&t.id).map_err(|e| {
        tracing::warn!("scheduler: history of {}: {e}", t.id);
        HistoryUnreadable
    })?;
    Ok(match (ran, feature_last_ms(p, t)) {
        (Some(r), Some(f)) if f > r.1 => Some((f, f)),
        (r, None) | (r @ Some(_), Some(_)) => r,
        (None, Some(f)) => Some((f, f)),
    })
}

/// An interval task that never ran is not due before its delay after the later of the boot
/// and the start of rynasd.
pub fn interval_not_before(t: &ScheduledTask, now: u64) -> u64 {
    let TaskTrigger::Interval { after_boot_min, .. } = t.trigger else {
        return 0;
    };
    boot_ms(now).max(started_ms()) + u64::from(after_boot_min) * 60_000
}

/// When an interval task is due ([`rynas_scheduler::gate::interval_due_ms`] from its last
/// run, else [`interval_not_before`]).
fn interval_due(t: &ScheduledTask, last: Option<(u64, u64)>, now: u64) -> u64 {
    match last {
        Some((_, end)) => rynas_scheduler::gate::interval_due_ms(t, end).unwrap_or(u64::MAX),
        None => interval_not_before(t, now),
    }
}

/// One turn of the watch loop (public for the tests).
pub fn tick(p: &Protection) {
    import_spool(p);
    let now = now_ms();
    let applied: Vec<ScheduledTask> = match p.ops.config.applied(rynas_scheduler::SCHEDULER_KEY) {
        Ok(v) => rynas_scheduler::domain::tasks_of(v.as_ref()),
        Err(e) => {
            tracing::warn!("scheduler watch: {e}");
            return;
        }
    };
    forget_deleted(p, &applied);
    let retries = p.history.retries().unwrap_or_default();
    let replacing: Vec<&ScheduledTask> = applied
        .iter()
        .filter(|t| t.effective_policy().overlap == rynas_api_types::scheduler::Overlap::Replace)
        .collect();
    let pause = p.gate.pause();
    let due = due_intervals(p, &applied, &retries, &pause, now);
    // nothing to do: no systemd question
    if retries.is_empty() && replacing.is_empty() && due.is_empty() {
        return;
    }
    let running = p.probe.running_tasks().unwrap_or_default();
    for task in due {
        let unit = service_name(&task.id);
        if running.contains(&unit) {
            continue;
        }
        p.gate.request(&task.id, RunTrigger::Timer, 0);
        match p.ops.config.system().start_unit(&unit) {
            Ok(()) => tracing::info!("scheduler: task {}: its interval has passed: started", task.id),
            Err(e) => tracing::warn!("scheduler: task {}: start: {e}", task.id),
        }
    }
    for (id, armed) in retries {
        let unit = service_name(&id);
        let Some(task) = applied.iter().find(|t| t.id == id) else {
            drop_retry(p, &id, "the task is gone");
            continue;
        };
        if !task.enabled {
            drop_retry(p, &id, "the task is off");
            continue;
        }
        // a run goes on: its end moves the chain on (the retry itself, or a run before it)
        if running.contains(&unit) {
            continue;
        }
        if let Some(at) = armed.started_ms {
            // started, nothing runs and no end came: the start was lost
            if now.saturating_sub(at) > REQUEST_TTL.as_millis() as u64 {
                drop_retry(p, &id, "its start did not happen");
            }
            continue;
        }
        if armed.retry.due_ms > now {
            continue;
        }
        if let Some(reason) = pause.blocks(&id, now) {
            // a pause drops the chain (the next scheduled run comes after the pause)
            let inv = format!("retry{:x}{}", now, armed.retry.attempt);
            let mut r = NewRun::skipped(&id, &inv, now, RunTrigger::Retry, reason, None);
            r.attempt = armed.retry.attempt;
            let _ = p.history.record(&r);
            p.invalidate();
            drop_retry(p, &id, "the task is paused");
            continue;
        }
        let _ = p.history.retry_started(&id, now);
        p.gate.request(&id, RunTrigger::Retry, armed.retry.attempt);
        match p.ops.config.system().start_unit(&unit) {
            Ok(()) => tracing::info!("scheduler: task {id}: retry {} started", armed.retry.attempt),
            Err(e) => {
                tracing::warn!("scheduler: task {id}: retry: {e}");
                drop_retry(p, &id, "its unit could not be started");
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A start of a due interval task that leaves its last run where it was (the gate skipped
    /// it) is tried again later and later, never every few minutes; a new last run resets it.
    #[test]
    fn interval_restarts_back_off() {
        let dir = tempfile::tempdir().unwrap();
        let g = Gate::new(dir.path().to_path_buf());
        let t = rynas_api_types::scheduler::system::pc_retention();
        assert!(g.interval_may_start(&t.id, Some(1), &t));
        assert!(!g.interval_may_start(&t.id, Some(1), &t), "not again at once");
        // pretend the wait passed: the next wait doubles
        let back = |g: &Gate, by: Duration| {
            let mut m = g.interval_started.lock().unwrap();
            let e = m.get_mut(&t.id).unwrap();
            e.0 = Instant::now().checked_sub(by).unwrap();
        };
        back(&g, INTERVAL_RESTART + Duration::from_secs(1));
        assert!(g.interval_may_start(&t.id, Some(1), &t));
        back(&g, INTERVAL_RESTART + Duration::from_secs(1));
        assert!(!g.interval_may_start(&t.id, Some(1), &t), "the second wait is twice as long");
        // at most the interval (one hour for the retention)
        back(&g, Duration::from_secs(3601));
        assert!(g.interval_may_start(&t.id, Some(1), &t));
        // a run happened: at once again
        assert!(g.interval_may_start(&t.id, Some(2), &t));
    }
}
