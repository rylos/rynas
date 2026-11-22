//! The scheduling gate (S2 of `docs/research/scheduling.md`): what decides, before a task
//! unit runs, whether this start goes ahead.
//!
//! Every task unit has `ExecCondition=+rynasctl gate <id>`. rynasctl asks rynasd
//! (`POST /v1/scheduler/tasks/{id}/gate`), which answers with [`decide`]; rynasctl exits 0
//! (run) or 1 (skip: systemd marks the start `exec-condition`, not failed, and a timer's
//! stamp has already moved on, so a skipped catch-up never comes back). A skip is recorded in
//! the run history with its reason.
//!
//! The checks, in order (a start the user asked for, "Run now" or `systemctl start`, only
//! meets the last one):
//! 0. nothing due (a timer's start, not recorded): an interval task whose interval has not
//!    passed since its last run ([`interval_due_ms`]), a weekly-window task outside its
//!    window or whose block already had its start ([`crate::window::start_due`]);
//! 1. pause of every task, then of this task ([`SchedulerPause::blocks`]);
//! 2. overlap: a timer whose time came while its service was running starts it once more as
//!    soon as the run ends (systemd 257, checked in `packaging/test-deb-systemd.sh`); with the
//!    overlap policy `skip` (stored on tasks created since S2) that start is skipped;
//!    `queue` (the default of a task without one: systemd's behaviour before) lets it run;
//! 3. the catch-up deadline: a catch-up start (`Persistent=` after downtime) runs only when a
//!    scheduled time lies within `missed.within_h` hours before it; a task with a start
//!    window (`window_min`) runs a catch-up, a queued start or a retry only inside the
//!    window of its scheduled time (otherwise skipped, `window`);
//! 4. another run of the task still going in rynasd (the lease of an earlier start);
//! 5. (rynasd, S4) the queue: a start whose claims meet what other work holds waits
//!    ([`GateAnswer::wait`], [`crate::queue`]); rynasctl asks again every [`WAIT_POLL`].
//!
//! **rynasd unreachable.** rynasctl waits at most [`GATE_TIMEOUT`] (rynasd is socket
//! activated: a stopped rynasd is started by the connection, a restarting one answers once
//! it is up). Without an answer the start goes ahead, as it did before the gate, so a
//! scheduled backup never stops silently because rynasd is broken; only a pause written in
//! [`PAUSE_FILE`] (root-only state, readable without rynasd) still skips a timer's start.
//! Either way rynasctl appends a line to [`SPOOL_FILE`], which rynasd imports into the
//! history when it is back.
//!
//! **Retries** of failed scheduled runs are armed by rynasd in `scheduler.db` (they survive
//! restarts and reboots) and started by rynasd's watch loop when due, unless a newer run
//! happened meanwhile ([`next_retry`]).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use rynas_api_types::scheduler::{
    MissedPolicy, Overlap, PendingRetry, Retry, RunTrigger, ScheduledTask, SchedulerPause,
    SkipReason, TaskTrigger,
};
use rynas_schedule::CalendarSpec;
use serde::{Deserialize, Serialize};

/// The pauses, below rynasd's state directory (root only, 0600).
pub const PAUSE_FILE: &str = "scheduler/pause.json";
/// Gate outcomes rynasctl could not hand to rynasd (one JSON line each).
pub const SPOOL_FILE: &str = "scheduler/gate-spool.jsonl";
/// How long `rynasctl gate` waits for rynasd's answer before running the start anyway.
pub const GATE_TIMEOUT: Duration = Duration::from_secs(20);
/// A timer start this close after a scheduled time (plus the random delay) is on time.
pub const ON_TIME_MS: u64 = 120_000;
/// An interval task's timer start this close before it is due runs (timers have
/// `AccuracySec=1s`); earlier it is not due.
pub const INTERVAL_ON_TIME_MS: u64 = 5_000;
/// Spool lines kept at most (a rynasd down for weeks with minutely tasks).
pub const SPOOL_MAX: usize = 2000;

/// Body of the internal `POST /v1/scheduler/tasks/{id}/gate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GateRequest {
    /// `$INVOCATION_ID` of the start.
    pub invocation_id: String,
    /// `$TRIGGER_UNIT` (systemd ≥ 253): the timer that started it, empty otherwise.
    #[serde(default)]
    pub trigger_unit: String,
    /// rynasctl runs the start anyway after this time (ms since the epoch; 0: unknown). An
    /// answer decided later says "run" and records no skip: the start may already be running.
    #[serde(default)]
    pub answer_by_ms: u64,
    /// `$TRIGGER_TIMER_REALTIME_USEC` in ms: when the timer elapsed (0: not set).
    #[serde(default)]
    pub trigger_timer_ms: u64,
    /// rynasctl can wait in the queue (S4: it asks again while the answer says `wait`). An
    /// older rynasctl never gets `wait`: its start runs, as before the queue.
    #[serde(default)]
    pub can_wait: bool,
    /// The start has been waiting in the queue since then (0: first question). rynasctl
    /// keeps it, so the order of the queue survives a restart of rynasd.
    #[serde(default)]
    pub queued_since_ms: u64,
    /// How the first answer said the waiting start came about (rynasctl sends it back on every
    /// question, so a restart of rynasd while it waits keeps its origin: a timer start stays
    /// one, paused, retried and checked as such).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_trigger: Option<RunTrigger>,
    #[serde(default)]
    pub queued_attempt: u8,
}

/// The unit's start timeout (`systemctl show -p TimeoutStartUSec --value`) lets its gate wait
/// in the queue: `infinity`, or unknown (the units rynas writes since S4 have it). A finite
/// value (a unit file written before S4, kept by hand) would kill a waiting start after it:
/// such a start is not queued, it runs as before.
pub fn start_timeout_lets_wait(value: &str) -> bool {
    let v = value.trim();
    v.is_empty() || v == "infinity"
}

/// How often `rynasctl gate` asks again while its start waits in the queue.
pub const WAIT_POLL: Duration = Duration::from_secs(5);
/// A waiter rynasd has not heard from for this long is gone (its unit was stopped).
pub const WAITER_STALE: Duration = Duration::from_secs(30);

/// A timer's activation details older than this are not this start's.
pub const STALE_TRIGGER_MS: u64 = 15 * 60_000;

/// The task's own timer started this start. systemd keeps a unit's activation details
/// (`$TRIGGER_UNIT`, `$TRIGGER_TIMER_*`) across later starts that bring none, so a start
/// rynasd asked for (`request_ms`: when) after a timer start still names the timer: the
/// timer's elapse time tells them apart (a real timer start elapsed after the request, and
/// moments ago). Without that time (an older systemd) a request wins.
pub fn started_by_timer(names_timer: bool, trigger_timer_ms: u64, request_ms: Option<u64>, now_ms: u64) -> bool {
    if !names_timer {
        return false;
    }
    match (trigger_timer_ms, request_ms) {
        (0, r) => r.is_none(),
        // the request came after the timer elapsed: the details are an earlier start's
        (t, Some(r)) if t < r => false,
        (t, _) => now_ms.saturating_sub(t) <= STALE_TRIGGER_MS,
    }
}

/// How long rynasd may take to decide before its answer counts as late
/// ([`GateRequest::answer_by_ms`]): well before [`GATE_TIMEOUT`], the margin covers the trip
/// back to rynasctl.
pub const ANSWER_BY: Duration = Duration::from_secs(15);

/// Answer of the gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateAnswer {
    pub run: bool,
    pub trigger: RunTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SkipReason>,
    /// One line for the journal of the unit (empty: say nothing).
    #[serde(default)]
    pub message: String,
    /// Not yet: the start waits in the queue; ask again after [`WAIT_POLL`] (then `run` is
    /// false and nothing is recorded).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wait: bool,
    /// The retry number of the start (rynasctl sends it back with `trigger` while it waits).
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub attempt: u8,
    /// With `wait`: when the wait ends (ms since the epoch; the queue's deadline of this
    /// start). rynasctl keeps it: once waiting, a rynasd that does not answer never makes it
    /// run outside the queue; past this moment it skips. 0: not said (an older rynasd).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub deadline_ms: u64,
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

fn is_zero_u8(n: &u8) -> bool {
    *n == 0
}

/// A gate outcome written by rynasctl while rynasd could not be reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpoolEntry {
    pub task_id: String,
    pub invocation_id: String,
    pub at_ms: u64,
    pub by_timer: bool,
    /// `None`: it ran without the gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SkipReason>,
    /// The end of a run `rynasctl task-finished` could not report (then `at_ms` is the end).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<SpoolFinished>,
}

/// systemd's report of a run's end (`$SERVICE_RESULT`, `$EXIT_CODE`, `$EXIT_STATUS`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SpoolFinished {
    pub result: String,
    #[serde(default)]
    pub exit_code: String,
    #[serde(default)]
    pub exit_status: String,
}

pub fn pause_path(state: &Path) -> PathBuf {
    state.join(PAUSE_FILE)
}

pub fn spool_path(state: &Path) -> PathBuf {
    state.join(SPOOL_FILE)
}

/// The pauses written by rynasd; none when the file is missing or unreadable.
pub fn load_pause(state: &Path) -> SchedulerPause {
    std::fs::read(pause_path(state))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Writes the pauses (atomically, 0600).
pub fn save_pause(state: &Path, p: &SchedulerPause) -> std::io::Result<()> {
    let path = pause_path(state);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&serde_json::to_vec(p).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)
}

/// Appends one outcome to the spool (rynasctl, rynasd unreachable).
pub fn spool_append(state: &Path, e: &SpoolEntry) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = spool_path(state);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > (SPOOL_MAX as u64) * 512) {
        return Ok(());
    }
    let mut line = serde_json::to_vec(e).map_err(std::io::Error::other)?;
    line.push(b'\n');
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?
        .write_all(&line)
}

/// Takes the spooled outcomes (the file is removed); unreadable lines are dropped.
pub fn spool_take(state: &Path) -> Vec<SpoolEntry> {
    let path = spool_path(state);
    let taking = path.with_extension("jsonl.taking");
    if std::fs::rename(&path, &taking).is_err() {
        return Vec::new();
    }
    let text = std::fs::read_to_string(&taking).unwrap_or_default();
    let _ = std::fs::remove_file(&taking);
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .take(SPOOL_MAX)
        .collect()
}

/// The scrubs that gave way (S4), below rynasd's state directory (root only, 0600): rynasd
/// writes an entry before it cancels the scrub, `rynasctl job run` reads it to tell a scrub
/// that gave way (exit 0, not a failure) from one that failed.
pub const YIELD_FILE: &str = "scheduler/yield.json";

/// A scrub that gave way to more urgent work on its volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct YieldEntry {
    /// The filesystem it scrubs.
    pub filesystem: String,
    /// The run that gave way.
    pub invocation_id: String,
    pub since_ms: u64,
    /// What it gave way to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<rynas_api_types::scheduler::Blocker>,
    /// The run has stopped: rynasd resumes it once the volume is free.
    #[serde(default)]
    pub stopped: bool,
}

pub fn yield_path(state: &Path) -> PathBuf {
    state.join(YIELD_FILE)
}

/// The scrubs that gave way, by task id (none when the file is missing or unreadable).
pub fn load_yields(state: &Path) -> std::collections::BTreeMap<String, YieldEntry> {
    std::fs::read(yield_path(state))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Writes the scrubs that gave way (atomically, 0600).
pub fn save_yields(state: &Path, m: &std::collections::BTreeMap<String, YieldEntry>) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = yield_path(state);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec(m).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)
}

/// The starts waiting in the queue or for the maintenance window, kept on disk (below the
/// state directory): a reboot that stops a waiting start would otherwise lose it (its timer's
/// last trigger has moved on, `Persistent=` does not catch it up). rynasd records it as
/// interrupted at its next start and starts it again when it may still run.
pub const PARKED_FILE: &str = "scheduler/waiting.json";

/// A start waiting (in [`PARKED_FILE`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parked {
    pub invocation_id: String,
    pub since_ms: u64,
    /// The end of its wait (the queue's deadline).
    pub deadline_ms: u64,
    pub trigger: RunTrigger,
    #[serde(default)]
    pub attempt: u8,
    /// The kernel's boot id when it began waiting.
    pub boot_id: String,
    /// It waited for the maintenance window (not for other work).
    #[serde(default)]
    pub window: bool,
}

pub fn parked_path(state: &Path) -> PathBuf {
    state.join(PARKED_FILE)
}

pub fn load_parked(state: &Path) -> std::collections::BTreeMap<String, Parked> {
    std::fs::read(parked_path(state))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Writes the waiting starts (atomically, 0600).
pub fn save_parked(state: &Path, m: &std::collections::BTreeMap<String, Parked>) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = parked_path(state);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec(m).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)
}

/// The kernel's id of this boot (`/proc/sys/kernel/random/boot_id`), empty when unknown.
pub fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|s| s.trim().to_string()).unwrap_or_default()
}

/// What to do with a start that was waiting when the NAS restarted: start it again when its
/// wait had not ended yet and the task catches up missed runs (or the start was a catch-up,
/// a retry or a start queued behind a run: it was owed); otherwise it is only recorded.
pub fn rearm_parked(p: &Parked, task: &ScheduledTask, now_ms: u64) -> bool {
    now_ms <= p.deadline_ms
        && task.enabled
        && (task.effective_policy().missed.catches_up()
            || matches!(p.trigger, RunTrigger::CatchUp | RunTrigger::Retry | RunTrigger::Queued | RunTrigger::Resume))
}

/// The run `invocation_id` is one rynasd asked to give way.
pub fn gave_way(state: &Path, invocation_id: &str) -> bool {
    !invocation_id.is_empty() && load_yields(state).values().any(|e| e.invocation_id == invocation_id)
}

/// What rynasd knows about a start when the gate asks.
#[derive(Debug, Clone)]
pub struct Start<'a> {
    pub task: &'a ScheduledTask,
    pub now_ms: u64,
    /// The task's own timer started it (`$TRIGGER_UNIT`).
    pub by_timer: bool,
    /// A start rynasd itself asked for moments ago: run now, a retry (with its number).
    pub requested: Option<(RunTrigger, u8)>,
    /// Start and end of the task's latest run (skips left out); for an interval task also
    /// the feature's own last check (a "Check now", the check before the task existed).
    pub last_run: Option<(u64, u64)>,
    /// An interval task that never ran is not due before this (its delay after boot, or after
    /// the start of rynasd, as the loops it replaces counted it).
    pub not_before_ms: u64,
    pub pause: &'a SchedulerPause,
    /// An earlier start of the task still runs in rynasd.
    pub running_elsewhere: bool,
    /// The NAS's time zone.
    pub tz: &'a TimeZone,
}

/// What the gate decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run {
        trigger: RunTrigger,
        attempt: u8,
    },
    Skip {
        trigger: RunTrigger,
        reason: SkipReason,
        detail: Option<String>,
    },
}

/// How a timer start relates to the schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerStart {
    /// A scheduled time just came (within the random delay and [`ON_TIME_MS`]).
    OnTime,
    /// The scheduled time came while the previous run went on: systemd starts it once
    /// more right after.
    AfterOverlap,
    /// Catching up a time missed while the timer was not running (NAS off).
    CatchUp,
}

fn ts(ms: u64) -> Option<Timestamp> {
    Timestamp::from_millisecond(i64::try_from(ms).ok()?).ok()
}

/// A scheduled time of `spec` in `[from_ms, to_ms]`.
fn slot_in(spec: &CalendarSpec, tz: &TimeZone, from_ms: u64, to_ms: u64) -> bool {
    let Some(after) = ts(from_ms.saturating_sub(1)) else {
        return false;
    };
    spec.next_after(tz, after)
        .is_some_and(|t| t.as_millisecond() <= to_ms as i64)
}

fn timer_start(s: &Start) -> TimerStart {
    let Some(expr) = s.task.trigger.on_calendar() else {
        return TimerStart::OnTime;
    };
    let Ok(spec) = CalendarSpec::parse_with(&expr, |z| TimeZone::get(z).is_ok()) else {
        return TimerStart::OnTime;
    };
    let jitter = u64::from(s.task.effective_policy().jitter_s) * 1000;
    if let Some((start, end)) = s.last_run
        && s.now_ms.saturating_sub(end) <= ON_TIME_MS
        && !slot_in(&spec, s.tz, end + 1, s.now_ms)
        && slot_in(&spec, s.tz, start, end)
    {
        // the previous run ended moments ago, a time came while it ran and none since
        return TimerStart::AfterOverlap;
    }
    if slot_in(&spec, s.tz, s.now_ms.saturating_sub(jitter + ON_TIME_MS), s.now_ms) {
        TimerStart::OnTime
    } else {
        TimerStart::CatchUp
    }
}

/// The start rynasd asked for, unless the task's own timer started it: a request left
/// behind (run now on a running task is a no-op for systemd) must never turn a timer's start
/// into a manual one that skips the pause, the overlap policy and the deadline.
fn requested(s: &Start) -> Option<(RunTrigger, u8)> {
    if s.by_timer { None } else { s.requested }
}

/// How the start came about.
pub fn classify(s: &Start) -> RunTrigger {
    if let Some((t, _)) = requested(s) {
        return t;
    }
    if !s.by_timer {
        return RunTrigger::Other;
    }
    match &s.task.trigger {
        TaskTrigger::Boot { .. } => RunTrigger::Boot,
        _ => match timer_start(s) {
            TimerStart::CatchUp => RunTrigger::CatchUp,
            TimerStart::OnTime | TimerStart::AfterOverlap => RunTrigger::Timer,
        },
    }
}

/// The gate's decision for a start.
pub fn decide(s: &Start) -> Decision {
    let mut trigger = classify(s);
    let attempt = requested(s).map_or(0, |(_, a)| a);
    let skip = |trigger, reason, detail| Decision::Skip {
        trigger,
        reason,
        detail,
    };
    let mut fallback = false;
    // nothing due: an interval that has not passed, a block that already had its start, the
    // DST fallback hour of a daily window on a normal night
    if s.by_timer && requested(s).is_none() {
        match &s.task.trigger {
            TaskTrigger::Interval { .. } => {
                let due = match s.last_run {
                    Some((_, end)) => interval_due_ms(s.task, end),
                    None => Some(s.not_before_ms),
                };
                // no calendar tolerance here: it would take minutes off the delay after
                // boot (a timer whose `OnBootSec=` passed long ago elapses at once after an
                // upgrade) and off the interval; rynasd's watch loop starts it when due
                if let Some(due) = due
                    && s.now_ms + INTERVAL_ON_TIME_MS < due
                {
                    return skip(trigger, SkipReason::NotDue, Some(format!("due at {due}")));
                }
            }
            TaskTrigger::Window { window, .. }
                if !crate::window::start_due(window, s.tz, s.now_ms, s.last_run.map(|r| r.0)) =>
            {
                return skip(trigger, SkipReason::NotDue, None);
            }
            TaskTrigger::Calendar { .. } if !on_time(s) => match fallback_start(s) {
                // a normal night: the window already had its run (a catch-up after a boot
                // in that hour still goes on as one)
                Some(false) if ran_in_window(s) => return skip(trigger, SkipReason::NotDue, None),
                Some(false) => {}
                Some(true) => {
                    fallback = true;
                    trigger = RunTrigger::Timer;
                }
                None => {}
            },
            _ => {}
        }
    }
    let asked = matches!(trigger, RunTrigger::Manual | RunTrigger::Other);
    if !asked {
        if let Some(reason) = s.pause.blocks(&s.task.id, s.now_ms) {
            let p = match reason {
                SkipReason::PausedAll => s.pause.all.as_ref(),
                _ => s.pause.tasks.get(&s.task.id),
            };
            let detail = p.map(|p| p.reason.clone()).filter(|r| !r.is_empty());
            return skip(trigger, reason, detail);
        }
        let policy = s.task.effective_policy();
        let calendar = matches!(
            s.task.trigger,
            TaskTrigger::Calendar { .. } | TaskTrigger::Recurrence { .. }
        );
        if s.by_timer && calendar && !fallback {
            match timer_start(s) {
                TimerStart::AfterOverlap if policy.overlap == Overlap::Skip => {
                    return skip(trigger, SkipReason::Overlap, None);
                }
                TimerStart::AfterOverlap => {
                    if !within_window(s) {
                        return skip(trigger, SkipReason::Window, Some(format!("{} min", policy.window_min)));
                    }
                    return Decision::Run {
                        trigger: RunTrigger::Queued,
                        attempt,
                    };
                }
                TimerStart::CatchUp => {
                    if let MissedPolicy::RunOnce { within_h: Some(h) } = policy.missed
                        && !within_deadline(s, h)
                    {
                        return skip(trigger, SkipReason::Missed, Some(format!("{h} h")));
                    }
                    if !within_window(s) {
                        return skip(trigger, SkipReason::Window, Some(format!("{} min", policy.window_min)));
                    }
                }
                TimerStart::OnTime => {}
            }
        }
        if trigger == RunTrigger::Retry && calendar && !within_window(s) {
            return skip(trigger, SkipReason::Window, Some(format!("{} min", policy.window_min)));
        }
    }
    if s.running_elsewhere {
        return skip(trigger, SkipReason::Running, None);
    }
    Decision::Run { trigger, attempt }
}

/// A scheduled time of the task's calendar came within its random delay before the start.
fn on_time(s: &Start) -> bool {
    let Some(expr) = s.task.trigger.on_calendar() else {
        return true;
    };
    let Ok(spec) = CalendarSpec::parse_with(&expr, |z| TimeZone::get(z).is_ok()) else {
        return true;
    };
    let jitter = u64::from(s.task.effective_policy().jitter_s) * 1000;
    slot_in(&spec, s.tz, s.now_ms.saturating_sub(jitter + ON_TIME_MS), s.now_ms)
}

/// A start in the DST fallback hour of a daily start window
/// ([`crate::window::dst_fallback`]): `Some(due)`, or `None` when it is not one.
fn fallback_start(s: &Start) -> Option<bool> {
    let p = s.task.effective_policy();
    let (h, m) = crate::window::daily_time(&s.task.trigger.on_calendar()?)?;
    crate::window::fallback_due(s.tz, s.now_ms, h, m, p.window_min, u64::from(p.jitter_s) * 1000 + ON_TIME_MS)
}

/// The task's latest run started inside its start window before now.
fn ran_in_window(s: &Start) -> bool {
    let w = u64::from(s.task.effective_policy().window_min) * 60_000;
    s.last_run.is_some_and(|(start, _)| start + w >= s.now_ms)
}

/// A scheduled time lies within the task's start window before the start (always, without
/// a start window).
fn within_window(s: &Start) -> bool {
    let min = s.task.effective_policy().window_min;
    if min == 0 {
        return true;
    }
    let Some(expr) = s.task.trigger.on_calendar() else {
        return true;
    };
    let Ok(spec) = CalendarSpec::parse_with(&expr, |z| TimeZone::get(z).is_ok()) else {
        return true;
    };
    if slot_in(&spec, s.tz, s.now_ms.saturating_sub(u64::from(min) * 60_000), s.now_ms) {
        return true;
    }
    // a daily window whose time did not exist today: its civil window still counts
    let Some((h, m)) = crate::window::daily_time(&expr) else {
        return false;
    };
    if !crate::window::gap_on_day(s.tz, s.now_ms, h, m) {
        return false;
    }
    let Some(now) = ts(s.now_ms) else { return false };
    let z = now.to_zoned(s.tz.clone());
    let minute = u32::from(z.hour() as u8) * 60 + u32::from(z.minute() as u8);
    let start = u32::from(h) * 60 + u32::from(m);
    minute >= start && minute < start + min
}

/// When an interval task is due again after a run that ended at `end_ms`: the interval plus
/// a random part of the policy's random delay, drawn once per run (the same answer every time
/// it is asked); `None` for other triggers.
pub fn interval_due_ms(t: &ScheduledTask, end_ms: u64) -> Option<u64> {
    let TaskTrigger::Interval { hours, .. } = t.trigger else {
        return None;
    };
    let jitter_ms = u64::from(t.effective_policy().jitter_s) * 1000;
    let extra = if jitter_ms == 0 {
        0
    } else {
        // FNV-1a over the task id and the run's end: stable, spread
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in t.id.bytes().chain(end_ms.to_le_bytes()) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h % jitter_ms
    };
    Some(end_ms + u64::from(hours.max(1)) * 3_600_000 + extra)
}

/// A scheduled time lies at most `h` hours (plus the random delay) before the start.
fn within_deadline(s: &Start, h: u32) -> bool {
    let Some(expr) = s.task.trigger.on_calendar() else {
        return true;
    };
    let Ok(spec) = CalendarSpec::parse_with(&expr, |z| TimeZone::get(z).is_ok()) else {
        return true;
    };
    let back = u64::from(h) * 3_600_000 + u64::from(s.task.effective_policy().jitter_s) * 1000;
    slot_in(&spec, s.tz, s.now_ms.saturating_sub(back), s.now_ms)
}

/// The wait before retry number `attempt` (1 = first): `delay_s`, doubled for each retry
/// before it with `backoff`, at most `max_delay_s` (when set); at least 1 s.
pub fn retry_delay_s(r: &Retry, attempt: u8) -> u32 {
    let mut d = u64::from(r.delay_s.max(1));
    if r.backoff {
        d = d.saturating_mul(1u64 << u32::from(attempt.saturating_sub(1)).min(20));
    }
    if r.max_delay_s > 0 {
        d = d.min(u64::from(r.max_delay_s.max(r.delay_s)));
    }
    d.min(u64::from(u32::MAX)) as u32
}

/// The retry to arm after a failed run that was retry number `done` (0 = a normal run), or
/// `None` when the policy allows no more.
pub fn next_retry(r: &Retry, done: u8, now_ms: u64) -> Option<PendingRetry> {
    if done >= r.attempts {
        return None;
    }
    let attempt = done + 1;
    Some(PendingRetry {
        attempt,
        attempts: r.attempts,
        due_ms: now_ms + u64::from(retry_delay_s(r, attempt)) * 1000,
    })
}

/// Runs of this trigger can be retried (a run started by hand is not: its user sees it).
pub fn retries_after(t: Option<RunTrigger>) -> bool {
    matches!(
        t,
        Some(
            RunTrigger::Timer
                | RunTrigger::CatchUp
                | RunTrigger::Boot
                | RunTrigger::Retry
                | RunTrigger::Queued
                | RunTrigger::Resume
        )
    )
}

/// A start waits in the queue at most a day, unless its policy says less.
pub const QUEUE_MAX: Duration = Duration::from_secs(24 * 3600);

/// How long a start of `t` may wait in the queue before it is skipped (`busy`): its catch-up
/// deadline, else its start window, else [`QUEUE_MAX`]; the md RAID check [`MD_CHECK_QUEUE`]
/// (skipped, its month would be lost: `mdcheck_continue` only goes on with a check that
/// started).
pub fn queue_limit_ms(t: &ScheduledTask) -> u64 {
    if matches!(t.action, rynas_api_types::scheduler::TaskAction::MdCheck { .. }) {
        return MD_CHECK_QUEUE.as_millis() as u64;
    }
    let p = t.effective_policy();
    let max = QUEUE_MAX.as_millis() as u64;
    match p.missed {
        MissedPolicy::RunOnce { within_h: Some(h) } => (u64::from(h) * 3_600_000).min(max),
        _ if p.window_min > 0 => (u64::from(p.window_min) * 60_000).min(max),
        _ => max,
    }
}

/// The md RAID check waits in the queue up to six days (its next monthly start is weeks away;
/// a nightly continuation does not run while it waits).
pub const MD_CHECK_QUEUE: Duration = Duration::from_secs(6 * 24 * 3600);

/// When a start of `t` that began waiting at `since_ms` stops waiting (ms since the epoch):
/// a task with a start window waits only until the window of its scheduled time ends (the
/// latest scheduled time at or before `since_ms`), never past it; the others
/// [`queue_limit_ms`] after `since_ms`.
pub fn queue_deadline_ms(t: &ScheduledTask, since_ms: u64, tz: &TimeZone) -> u64 {
    let p = t.effective_policy();
    if p.window_min > 0
        && let Some(expr) = t.trigger.on_calendar()
        && let Ok(spec) = CalendarSpec::parse_with(&expr, |z| TimeZone::get(z).is_ok())
    {
        let w = u64::from(p.window_min) * 60_000;
        // the latest scheduled time within one window before the first wait: looked for from
        // close by first, further back only while none is found, so a calendar of every
        // second or minute is not walked over the whole window at every question
        let back = w + u64::from(p.jitter_s) * 1000 + ON_TIME_MS;
        let next_ms = |at: u64| ts(at).and_then(|a| spec.next_after(tz, a)).map(|n| n.as_millisecond().max(0) as u64);
        let mut slot = None;
        let mut reach = 60_000u64.min(back);
        loop {
            let mut at = since_ms.saturating_sub(reach);
            while let Some(n) = next_ms(at).filter(|n| *n <= since_ms) {
                slot = Some(n);
                at = n;
            }
            if slot.is_some() || reach >= back {
                break;
            }
            reach = reach.saturating_mul(8).min(back);
        }
        if let Some(s) = slot {
            return s + w;
        }
    }
    since_ms + queue_limit_ms(t)
}

/// The overlap policy `replace` applies now: the task runs since `run_start_ms` and one of
/// its scheduled times came after that start.
pub fn replace_due(t: &ScheduledTask, run_start_ms: u64, now_ms: u64, tz: &TimeZone) -> bool {
    if t.effective_policy().overlap != Overlap::Replace {
        return false;
    }
    let Some(expr) = t.trigger.on_calendar() else {
        return false;
    };
    let Ok(spec) = CalendarSpec::parse_with(&expr, |z| TimeZone::get(z).is_ok()) else {
        return false;
    };
    slot_in(&spec, tz, run_start_ms + 1000, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::scheduler::{PauseInfo, RunPolicy, TaskAction};

    fn utc() -> TimeZone {
        TimeZone::UTC
    }

    /// ms of a UTC wall-clock time.
    fn at(s: &str) -> u64 {
        let t: Timestamp = format!("{s}Z").parse().unwrap();
        t.as_millisecond() as u64
    }

    fn task(on_calendar: &str, policy: RunPolicy) -> ScheduledTask {
        let mut t = ScheduledTask {
            id: "t".into(),
            name: String::new(),
            description: String::new(),
            enabled: true,
            action: TaskAction::Command { command: "true".into(), user: "root".into() },
            trigger: TaskTrigger::Calendar {
                on_calendar: on_calendar.into(),
                cron: None,
                persistent: true,
                randomized_delay_s: 0,
            },
            timeout_s: None,
            nice: None,
            io_weight: None,
            notify: Default::default(),
            notify_output: false,
            owner: Default::default(),
            policy,
        };
        t.migrate();
        t
    }

    fn start<'a>(t: &'a ScheduledTask, now: &str, pause: &'a SchedulerPause, tz: &'a TimeZone) -> Start<'a> {
        Start {
            task: t,
            now_ms: at(now),
            by_timer: true,
            requested: None,
            last_run: None,
            not_before_ms: 0,
            pause,
            running_elsewhere: false,
            tz,
        }
    }

    #[test]
    fn on_time_catch_up_and_deadline() {
        let tz = utc();
        let none = SchedulerPause::default();
        let t = task("*-*-* 02:00:00", RunPolicy::default());
        let s = start(&t, "2026-11-16T02:00:01", &none, &tz);
        assert_eq!(decide(&s), Decision::Run { trigger: RunTrigger::Timer, attempt: 0 });
        // the NAS came up at 09:00 after missing 02:00: a catch-up, no deadline
        let s = start(&t, "2026-11-16T09:00:00", &none, &tz);
        assert_eq!(decide(&s), Decision::Run { trigger: RunTrigger::CatchUp, attempt: 0 });
        // deadline 6 h: skipped as missed; 8 h: runs
        let late = task("*-*-* 02:00:00", RunPolicy { missed: Some(MissedPolicy::RunOnce { within_h: Some(6) }), ..Default::default() });
        let s = start(&late, "2026-11-16T09:00:00", &none, &tz);
        assert_eq!(
            decide(&s),
            Decision::Skip { trigger: RunTrigger::CatchUp, reason: SkipReason::Missed, detail: Some("6 h".into()) }
        );
        let ok = task("*-*-* 02:00:00", RunPolicy { missed: Some(MissedPolicy::RunOnce { within_h: Some(8) }), ..Default::default() });
        assert_eq!(decide(&start(&ok, "2026-11-16T09:00:00", &none, &tz)), Decision::Run { trigger: RunTrigger::CatchUp, attempt: 0 });
        // the deadline never touches a start on time, nor one asked for by hand
        assert_eq!(decide(&start(&late, "2026-11-16T02:00:00", &none, &tz)), Decision::Run { trigger: RunTrigger::Timer, attempt: 0 });
        let mut by_hand = start(&late, "2026-11-16T09:00:00", &none, &tz);
        by_hand.by_timer = false;
        by_hand.requested = Some((RunTrigger::Manual, 0));
        assert_eq!(decide(&by_hand), Decision::Run { trigger: RunTrigger::Manual, attempt: 0 });
        by_hand.requested = None;
        by_hand.by_timer = false;
        assert_eq!(decide(&by_hand), Decision::Run { trigger: RunTrigger::Other, attempt: 0 });
    }

    /// An interval task's timer start is not due before its time: no calendar tolerance (a
    /// never-run task whose `OnBootSec=` passed long ago elapses at once after an upgrade; its
    /// delay after rynasd's start must hold).
    #[test]
    fn interval_starts_have_no_tolerance() {
        let tz = utc();
        let none = SchedulerPause::default();
        let mut t = task("*-*-* 02:00:00", RunPolicy::default());
        t.trigger = TaskTrigger::Interval { hours: 1, after_boot_min: 2 };
        let mut s = start(&t, "2026-11-16T10:00:00", &none, &tz);
        s.not_before_ms = at("2026-11-16T10:02:00");
        assert!(matches!(decide(&s), Decision::Skip { reason: SkipReason::NotDue, .. }));
        s.now_ms = at("2026-11-16T10:01:58");
        assert!(matches!(decide(&s), Decision::Run { .. }));
        // after a run: due an interval after its end
        s.last_run = Some((at("2026-11-16T09:00:00"), at("2026-11-16T09:10:00")));
        s.now_ms = at("2026-11-16T10:08:30");
        assert!(matches!(decide(&s), Decision::Skip { reason: SkipReason::NotDue, .. }));
    }

    #[test]
    fn random_delay_counts_as_on_time() {
        let tz = utc();
        let none = SchedulerPause::default();
        let t = task("*-*-* 02:00:00", RunPolicy { jitter_s: Some(600), missed: Some(MissedPolicy::RunOnce { within_h: Some(1) }), ..Default::default() });
        assert_eq!(classify(&start(&t, "2026-11-16T02:09:59", &none, &tz)), RunTrigger::Timer);
        assert_eq!(classify(&start(&t, "2026-11-16T02:20:00", &none, &tz)), RunTrigger::CatchUp);
        assert!(matches!(decide(&start(&t, "2026-11-16T02:20:00", &none, &tz)), Decision::Run { .. }));
        assert!(matches!(decide(&start(&t, "2026-11-16T03:20:01", &none, &tz)), Decision::Skip { reason: SkipReason::Missed, .. }));
    }

    #[test]
    fn pauses() {
        let tz = utc();
        let t = task("hourly", RunPolicy::default());
        let mut p = SchedulerPause::default();
        p.tasks.insert("t".into(), PauseInfo { since_ms: 0, until_ms: None, reason: String::new(), by: "admin".into() });
        assert!(matches!(decide(&start(&t, "2026-11-16T05:00:00", &p, &tz)), Decision::Skip { reason: SkipReason::Paused, detail: None, .. }));
        p.all = Some(PauseInfo { since_ms: 0, until_ms: Some(at("2026-11-16T06:00:00")), reason: "disk swap".into(), by: "admin".into() });
        assert_eq!(
            decide(&start(&t, "2026-11-16T05:00:00", &p, &tz)),
            Decision::Skip { trigger: RunTrigger::Timer, reason: SkipReason::PausedAll, detail: Some("disk swap".into()) }
        );
        // the pause of every task ended at 06:00; the task's own still holds
        assert!(matches!(decide(&start(&t, "2026-11-16T07:00:00", &p, &tz)), Decision::Skip { reason: SkipReason::Paused, .. }));
        // run now goes ahead while paused; a retry does not
        let mut s = start(&t, "2026-11-16T05:00:00", &p, &tz);
        s.by_timer = false;
        s.requested = Some((RunTrigger::Manual, 0));
        assert!(matches!(decide(&s), Decision::Run { .. }));
        s.requested = Some((RunTrigger::Retry, 1));
        assert!(matches!(decide(&s), Decision::Skip { trigger: RunTrigger::Retry, reason: SkipReason::PausedAll, .. }));
        let mut q = p.clone();
        assert!(q.expire(at("2026-11-16T07:00:00")));
        assert!(q.all.is_none() && q.tasks.len() == 1);
        assert!(!q.expire(at("2026-11-16T07:00:00")));
    }

    #[test]
    fn overlap_policies() {
        let tz = utc();
        let none = SchedulerPause::default();
        // every 10 minutes; the run of 05:00 went on until 05:12: systemd starts it again
        // at 05:12 for the time of 05:10
        let t = task("*:0/10", RunPolicy { overlap: Some(Overlap::Skip), ..Default::default() });
        let mut s = start(&t, "2026-11-16T05:12:00", &none, &tz);
        s.last_run = Some((at("2026-11-16T05:00:00"), at("2026-11-16T05:11:59")));
        assert_eq!(decide(&s), Decision::Skip { trigger: RunTrigger::Timer, reason: SkipReason::Overlap, detail: None });
        let q = task("*:0/10", RunPolicy { overlap: Some(Overlap::Queue), ..Default::default() });
        let mut s2 = s.clone();
        s2.task = &q;
        assert_eq!(decide(&s2), Decision::Run { trigger: RunTrigger::Queued, attempt: 0 });
        // a task without a stored overlap (written before S2) queues, as systemd did
        let old = task("*:0/10", RunPolicy::default());
        assert_eq!(old.effective_policy().overlap, Overlap::Queue);
        s2.task = &old;
        assert_eq!(decide(&s2), Decision::Run { trigger: RunTrigger::Queued, attempt: 0 });
        // a boot long after the last run is a catch-up, not an overlap
        let mut s3 = start(&t, "2026-11-16T09:15:00", &none, &tz);
        s3.last_run = Some((at("2026-11-16T05:00:00"), at("2026-11-16T05:01:00")));
        assert_eq!(classify(&s3), RunTrigger::CatchUp);
        // still running in rynasd: skipped whatever started it
        let mut s4 = start(&t, "2026-11-16T05:20:00", &none, &tz);
        s4.running_elsewhere = true;
        assert!(matches!(decide(&s4), Decision::Skip { reason: SkipReason::Running, .. }));
        s4.by_timer = false;
        s4.requested = Some((RunTrigger::Manual, 0));
        assert!(matches!(decide(&s4), Decision::Skip { trigger: RunTrigger::Manual, reason: SkipReason::Running, .. }));
        // replace: due once a time came after the run started
        let r = task("*:0/10", RunPolicy { overlap: Some(Overlap::Replace), ..Default::default() });
        assert!(!replace_due(&r, at("2026-11-16T05:00:00"), at("2026-11-16T05:09:00"), &tz));
        assert!(replace_due(&r, at("2026-11-16T05:00:00"), at("2026-11-16T05:10:00"), &tz));
        assert!(!replace_due(&t, at("2026-11-16T05:00:00"), at("2026-11-16T05:10:00"), &tz));
    }

    #[test]
    fn boot_trigger() {
        let tz = utc();
        let none = SchedulerPause::default();
        let mut t = task("hourly", RunPolicy::default());
        t.trigger = TaskTrigger::Boot { delay_s: 60 };
        assert_eq!(decide(&start(&t, "2026-11-16T05:12:00", &none, &tz)), Decision::Run { trigger: RunTrigger::Boot, attempt: 0 });
    }

    #[test]
    fn a_stale_request_never_takes_over_a_timer_start() {
        let tz = utc();
        let mut paused = SchedulerPause::default();
        paused.tasks.insert("t".into(), PauseInfo { since_ms: 0, ..Default::default() });
        let late = task("*-*-* 02:00:00", RunPolicy { missed: Some(MissedPolicy::RunOnce { within_h: Some(1) }), ..Default::default() });
        // run now on a running task left a request behind; the timer's next start is still
        // the timer's: paused, past its deadline
        let mut s = start(&late, "2026-11-16T09:00:00", &paused, &tz);
        s.requested = Some((RunTrigger::Manual, 0));
        assert_eq!(classify(&s), RunTrigger::CatchUp);
        assert!(matches!(decide(&s), Decision::Skip { trigger: RunTrigger::CatchUp, reason: SkipReason::Paused, .. }));
        let none = SchedulerPause::default();
        s.pause = &none;
        assert!(matches!(decide(&s), Decision::Skip { reason: SkipReason::Missed, .. }));
        s.requested = Some((RunTrigger::Retry, 2));
        assert!(matches!(decide(&s), Decision::Skip { trigger: RunTrigger::CatchUp, reason: SkipReason::Missed, .. }));
        // the same request on a start rynasd made (no timer) counts
        s.by_timer = false;
        assert_eq!(decide(&s), Decision::Run { trigger: RunTrigger::Retry, attempt: 2 });
    }

    #[test]
    fn timer_starts_are_told_from_stale_activation_details() {
        let now = 1_000_000_000;
        // a timer start: elapsed moments ago, after any request
        assert!(started_by_timer(true, now - 50, None, now));
        assert!(started_by_timer(true, now - 50, Some(now - 60_000), now));
        // a retry or run now after an earlier timer start: the details are older than the
        // request
        assert!(!started_by_timer(true, now - 9_000, Some(now - 2_000), now));
        // run now 290 ms after a timer start (found on systemd 257)
        assert!(!started_by_timer(true, now - 300, Some(now - 10), now));
        // details left from long ago, nothing asked (systemctl start by hand)
        assert!(!started_by_timer(true, now - STALE_TRIGGER_MS - 1, None, now));
        // an older systemd without the elapse time: a request wins
        assert!(started_by_timer(true, 0, None, now));
        assert!(!started_by_timer(true, 0, Some(now), now));
        assert!(!started_by_timer(false, now, None, now));
    }

    #[test]
    fn run_now_across_a_scheduled_time_keeps_the_scheduled_run_with_queue() {
        let tz = utc();
        let none = SchedulerPause::default();
        // run now at 04:58 ran until 05:03: the time of 05:00 fell inside it; systemd starts
        // the unit by its timer right after
        let q = task("*-*-* 05:00:00", RunPolicy::default());
        let mut s = start(&q, "2026-11-16T05:03:01", &none, &tz);
        s.last_run = Some((at("2026-11-16T04:58:00"), at("2026-11-16T05:03:00")));
        assert_eq!(decide(&s), Decision::Run { trigger: RunTrigger::Queued, attempt: 0 });
    }

    #[test]
    fn retry_waits() {
        let r = Retry::TRANSFER;
        assert_eq!([1, 2, 3, 4].map(|a| retry_delay_s(&r, a)), [600, 1200, 2400, 3600]);
        let flat = Retry { attempts: 2, delay_s: 30, backoff: false, max_delay_s: 0 };
        assert_eq!(retry_delay_s(&flat, 2), 30);
        assert_eq!(next_retry(&r, 0, 1000), Some(PendingRetry { attempt: 1, attempts: 3, due_ms: 601_000 }));
        assert_eq!(next_retry(&r, 2, 0).map(|p| p.attempt), Some(3));
        assert_eq!(next_retry(&r, 3, 0), None);
        assert_eq!(next_retry(&Retry::NONE, 0, 0), None);
        assert!(retries_after(Some(RunTrigger::CatchUp)) && !retries_after(Some(RunTrigger::Manual)) && !retries_after(None));
    }

    #[test]
    fn pause_file_and_spool() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(load_pause(d.path()), SchedulerPause::default());
        let p = SchedulerPause { all: Some(PauseInfo { since_ms: 5, ..Default::default() }), ..Default::default() };
        save_pause(d.path(), &p).unwrap();
        assert_eq!(load_pause(d.path()), p);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(pause_path(d.path())).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(spool_take(d.path()).is_empty());
        let e = SpoolEntry { task_id: "t".into(), invocation_id: "ab".into(), at_ms: 7, by_timer: true, skipped: Some(SkipReason::PausedAll), finished: None };
        spool_append(d.path(), &e).unwrap();
        spool_append(d.path(), &SpoolEntry { skipped: None, ..e.clone() }).unwrap();
        let got = spool_take(d.path());
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], e);
        assert!(spool_take(d.path()).is_empty());
    }

    #[test]
    fn a_start_window_bounds_the_wait_in_the_queue() {
        let tz = utc();
        // a 90-minute window from 02:30: a start that began waiting at 03:10 waits until
        // 04:00, the end of the window, not 90 minutes from 03:10
        let w = task("*-*-* 02:30:00", RunPolicy { window_min: Some(90), jitter_s: Some(600), ..Default::default() });
        assert_eq!(queue_deadline_ms(&w, at("2026-11-16T03:10:00"), &tz), at("2026-11-16T04:00:00"));
        // without a window: a day after it began waiting
        let d = task("*-*-* 02:30:00", RunPolicy::default());
        assert_eq!(queue_deadline_ms(&d, at("2026-11-16T03:10:00"), &tz), at("2026-11-17T03:10:00"));
        // the md RAID check: six days
        let mut md = rynas_api_types::scheduler::system::md_check(false);
        md.migrate();
        assert_eq!(queue_limit_ms(&md), 6 * 24 * 3_600_000);
    }

    /// A start cut by a restart of the NAS while it waited is started again only within its
    /// deadline and when it was owed (the task catches up, or the start was a catch-up).
    #[test]
    fn interrupted_waits_are_started_again_when_owed() {
        let parked = |trigger, deadline_ms| Parked {
            invocation_id: "i".into(),
            since_ms: 0,
            deadline_ms,
            trigger,
            attempt: 0,
            boot_id: "b".into(),
            window: true,
        };
        let catches_up = task("*-*-* 02:00:00", RunPolicy::default());
        assert!(rearm_parked(&parked(RunTrigger::Timer, 10_000), &catches_up, 5_000));
        assert!(!rearm_parked(&parked(RunTrigger::Timer, 10_000), &catches_up, 20_000), "past its deadline");
        let skips = task("*-*-* 02:00:00", RunPolicy { missed: Some(MissedPolicy::Skip), ..Default::default() });
        assert!(!rearm_parked(&parked(RunTrigger::Timer, 10_000), &skips, 5_000));
        assert!(rearm_parked(&parked(RunTrigger::CatchUp, 10_000), &skips, 5_000));
        let mut off = catches_up.clone();
        off.enabled = false;
        assert!(!rearm_parked(&parked(RunTrigger::Timer, 10_000), &off, 5_000));
        // what rynasctl keeps: the end of its wait
        let a: GateAnswer = serde_json::from_str(r#"{"run":false,"trigger":"timer","wait":true,"deadline_ms":42}"#).unwrap();
        assert_eq!(a.deadline_ms, 42);
        let old: GateAnswer = serde_json::from_str(r#"{"run":false,"trigger":"timer","wait":true}"#).unwrap();
        assert_eq!(old.deadline_ms, 0);
    }

    #[test]
    fn only_units_without_a_start_timeout_wait_in_the_queue() {
        assert!(start_timeout_lets_wait("infinity\n"));
        assert!(start_timeout_lets_wait(""));
        assert!(!start_timeout_lets_wait("1min 30s"));
    }
}
