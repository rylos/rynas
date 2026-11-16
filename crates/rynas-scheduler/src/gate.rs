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
//! 4. another run of the task still going in rynasd (the lease of an earlier start).
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
}

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

/// What rynasd knows about a start when the gate asks.
#[derive(Debug, Clone)]
pub struct Start<'a> {
    pub task: &'a ScheduledTask,
    pub now_ms: u64,
    /// The task's own timer started it (`$TRIGGER_UNIT`).
    pub by_timer: bool,
    /// A start rynasd itself asked for moments ago: run now, a retry (with its number).
    pub requested: Option<(RunTrigger, u8)>,
    /// Start and end of the task's latest run (skips left out).
    pub last_run: Option<(u64, u64)>,
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

pub fn decide(s: &Start) -> Decision {
    let trigger = classify(s);
    let attempt = requested(s).map_or(0, |(_, a)| a);
    let skip = |reason, detail| Decision::Skip {
        trigger,
        reason,
        detail,
    };
    // nothing due: an interval that has not passed, a block that already had its start
    if s.by_timer && requested(s).is_none() {
        match &s.task.trigger {
            TaskTrigger::Interval { .. } => {
                if let Some(due) = s.last_run.and_then(|(_, end)| interval_due_ms(s.task, end))
                    && s.now_ms + ON_TIME_MS < due
                {
                    return skip(SkipReason::NotDue, Some(format!("due at {due}")));
                }
            }
            TaskTrigger::Window { window, .. }
                if !crate::window::start_due(window, s.tz, s.now_ms, s.last_run.map(|r| r.0)) =>
            {
                return skip(SkipReason::NotDue, None);
            }
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
            return skip(reason, detail);
        }
        let policy = s.task.effective_policy();
        let calendar = matches!(
            s.task.trigger,
            TaskTrigger::Calendar { .. } | TaskTrigger::Recurrence { .. }
        );
        if s.by_timer && calendar {
            match timer_start(s) {
                TimerStart::AfterOverlap if policy.overlap == Overlap::Skip => {
                    return skip(SkipReason::Overlap, None);
                }
                TimerStart::AfterOverlap => {
                    if !within_window(s) {
                        return skip(SkipReason::Window, Some(format!("{} min", policy.window_min)));
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
                        return skip(SkipReason::Missed, Some(format!("{h} h")));
                    }
                    if !within_window(s) {
                        return skip(SkipReason::Window, Some(format!("{} min", policy.window_min)));
                    }
                }
                TimerStart::OnTime => {}
            }
        }
        if trigger == RunTrigger::Retry && calendar && !within_window(s) {
            return skip(SkipReason::Window, Some(format!("{} min", policy.window_min)));
        }
    }
    if s.running_elsewhere {
        return skip(SkipReason::Running, None);
    }
    Decision::Run { trigger, attempt }
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
    slot_in(&spec, s.tz, s.now_ms.saturating_sub(u64::from(min) * 60_000), s.now_ms)
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
        )
    )
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
}
