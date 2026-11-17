//! The global maintenance window (S5 of `docs/research/scheduling.md`): the pure rules.
//!
//! The window is 168 cells `0`/`1`, Sunday first (`window[day * 24 + hour]`), in the NAS's
//! local time, like every weekly grid of rynas. A scheduled start of heavy work that follows
//! it ([`ScheduledTask::follows_maintenance`]) and comes while the window is closed waits in
//! the gate's queue until the window opens ([`opens_at`]), or is skipped when that comes after
//! the start's deadline (the queue's own, `gate::queue_deadline_ms`): [`defer`].
//!
//! Local hours follow the clock on the wall: on the spring-forward night the missing hour has
//! no cell to be in, and the repeated hour of the fall-back night is the same cell twice.

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use rynas_api_types::scheduler::{RunTrigger, ScheduledTask, week_window_on};

/// How far ahead [`opens_at`] looks (a week and a day: every cell is met).
const LOOK_AHEAD_H: u64 = 8 * 24;

fn ts(ms: u64) -> Option<Timestamp> {
    Timestamp::from_millisecond(i64::try_from(ms).ok()?).ok()
}

fn cell_of(dt: &DateTime) -> usize {
    dt.weekday().to_sunday_zero_offset() as usize * 24 + dt.hour() as usize
}

/// The window is open at `ms`.
pub fn is_open(window: &str, tz: &TimeZone, ms: u64) -> bool {
    ts(ms).is_some_and(|t| week_window_on(window, cell_of(&t.to_zoned(tz.clone()).datetime())))
}

/// When the window is open next, at or after `ms`: `ms` itself when it is open now, else the
/// start of its next "on" hour. `None` for a window without any "on" cell.
pub fn opens_at(window: &str, tz: &TimeZone, ms: u64) -> Option<u64> {
    let mut at = ms;
    for _ in 0..LOOK_AHEAD_H + 2 {
        let t = ts(at)?;
        let local = t.to_zoned(tz.clone()).datetime();
        if week_window_on(window, cell_of(&local)) {
            return Some(at);
        }
        // the start of the next local hour (a gap hour: the moment after the jump)
        let hour = local.with().minute(0).second(0).subsec_nanosecond(0).build().ok()?;
        let next = hour
            .checked_add(jiff::ToSpan::hour(1))
            .ok()
            .and_then(|n| tz.to_ambiguous_zoned(n).compatible().ok())
            .map(|z| z.timestamp().as_millisecond().max(0) as u64);
        at = match next {
            Some(n) if n > at => n,
            _ => at + 3_600_000,
        };
    }
    None
}

/// A moment in the NAS's local time, for the journal: "Wed 01:00 CET".
pub fn wall_clock(tz: &TimeZone, ms: u64) -> String {
    ts(ms).map(|t| t.to_zoned(tz.clone()).strftime("%a %H:%M %Z").to_string()).unwrap_or_default()
}

/// Starts the window holds back: those of the schedule (its timer, a catch-up, a queued
/// start, a retry, the delay after boot). Run now, a start by hand and a scrub resuming the
/// work it started go at once.
pub fn defers(trigger: RunTrigger) -> bool {
    matches!(
        trigger,
        RunTrigger::Timer | RunTrigger::CatchUp | RunTrigger::Queued | RunTrigger::Retry | RunTrigger::Boot
    )
}

/// What the window does to a start of `t` at `now_ms` that began waiting at `since_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Defer {
    /// No window, a task that does not follow it, a start it does not hold back, or the
    /// window is open: the start goes on (to the queue).
    Go,
    /// It waits until the window opens then.
    Until(u64),
    /// The window opens only after the start's deadline (or never): skipped.
    Skip(Option<u64>),
}

/// [`Defer`] for a start: `window` is the global window, if set.
pub fn defer(window: Option<&str>, t: &ScheduledTask, trigger: RunTrigger, since_ms: u64, now_ms: u64, tz: &TimeZone) -> Defer {
    let Some(w) = window else { return Defer::Go };
    if !t.follows_maintenance() || !defers(trigger) {
        return Defer::Go;
    }
    match opens_at(w, tz, now_ms) {
        Some(o) if o <= now_ms => Defer::Go,
        Some(o) if o <= crate::gate::queue_deadline_ms(t, since_ms, tz) => Defer::Until(o),
        o => Defer::Skip(o),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::scheduler::{RunPolicy, SmartTestKind, TaskAction, TaskTrigger};

    /// Every day from `from` to `to` (hours, `to` excluded).
    fn nightly(from: usize, to: usize) -> String {
        let mut w = vec![b'0'; 168];
        for d in 0..7 {
            for h in from..to {
                w[d * 24 + h] = b'1';
            }
        }
        String::from_utf8(w).unwrap()
    }

    fn ms(s: &str, tz: &TimeZone) -> u64 {
        let dt: DateTime = s.parse().unwrap();
        tz.to_ambiguous_zoned(dt).compatible().unwrap().timestamp().as_millisecond() as u64
    }

    fn task(action: TaskAction, policy: RunPolicy) -> ScheduledTask {
        ScheduledTask {
            id: "t".into(),
            name: "t".into(),
            description: String::new(),
            enabled: true,
            action,
            trigger: TaskTrigger::Calendar { on_calendar: "*-*-* 14:00:00".into(), cron: None, persistent: true, randomized_delay_s: 0 },
            timeout_s: None,
            nice: None,
            io_weight: None,
            notify: Default::default(),
            notify_output: false,
            owner: Default::default(),
            policy,
        }
    }

    #[test]
    fn open_and_next_hour() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let w = nightly(1, 5);
        assert!(is_open(&w, &tz, ms("2026-11-17T01:00", &tz)));
        assert!(is_open(&w, &tz, ms("2026-11-17T04:59", &tz)));
        assert!(!is_open(&w, &tz, ms("2026-11-17T05:00", &tz)));
        assert_eq!(opens_at(&w, &tz, ms("2026-11-17T14:20", &tz)), Some(ms("2026-11-18T01:00", &tz)));
        assert_eq!(opens_at(&w, &tz, ms("2026-11-17T02:20", &tz)), Some(ms("2026-11-17T02:20", &tz)));
        // a weekend-only window, seen on a Tuesday
        let mut sat = vec![b'0'; 168];
        sat[6 * 24 + 3] = b'1';
        let sat = String::from_utf8(sat).unwrap();
        assert_eq!(opens_at(&sat, &tz, ms("2026-11-17T12:00", &tz)), Some(ms("2026-11-21T03:00", &tz)));
        // no "on" cell at all
        assert_eq!(opens_at(&"0".repeat(168), &tz, 0), None);
    }

    #[test]
    fn dst_nights() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        // spring forward 2026-03-29: 02:00 does not exist, a window from 02:00 opens at 03:00
        let w = nightly(2, 5);
        assert_eq!(opens_at(&w, &tz, ms("2026-03-29T01:30", &tz)), Some(ms("2026-03-29T03:00", &tz)));
        // fall back 2026-10-25: the repeated hour 02:00 is on both times
        let at = ms("2026-10-25T02:30", &tz) + 3_600_000; // the second 02:30
        assert!(is_open(&w, &tz, at));
    }

    #[test]
    fn heavy_work_waits_or_is_skipped() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let w = nightly(1, 5);
        let now = ms("2026-11-17T14:00", &tz);
        let scrub = task(TaskAction::Scrub { filesystem: "/srv/a".into() }, RunPolicy::default());
        // a day at most in the queue: the window at 01:00 is within it
        assert_eq!(defer(Some(&w), &scrub, RunTrigger::Timer, now, now, &tz), Defer::Until(ms("2026-11-18T01:00", &tz)));
        assert_eq!(defer(Some(&w), &scrub, RunTrigger::CatchUp, now, now, &tz), Defer::Until(ms("2026-11-18T01:00", &tz)));
        // no window, run now, a resume: at once
        assert_eq!(defer(None, &scrub, RunTrigger::Timer, now, now, &tz), Defer::Go);
        assert_eq!(defer(Some(&w), &scrub, RunTrigger::Manual, now, now, &tz), Defer::Go);
        assert_eq!(defer(Some(&w), &scrub, RunTrigger::Resume, now, now, &tz), Defer::Go);
        // inside the window
        let night = ms("2026-11-18T02:00", &tz);
        assert_eq!(defer(Some(&w), &scrub, RunTrigger::Timer, night, night, &tz), Defer::Go);
        // a catch-up deadline of 6 h: the window opens 11 h later, skipped
        let late = task(
            TaskAction::Backup { job: "b".into() },
            RunPolicy { missed: Some(rynas_api_types::scheduler::MissedPolicy::RunOnce { within_h: Some(6) }), ..Default::default() },
        );
        assert_eq!(defer(Some(&w), &late, RunTrigger::Timer, now, now, &tz), Defer::Skip(Some(ms("2026-11-18T01:00", &tz))));
        // light work never follows it; a SMART short test neither
        let snap = task(TaskAction::Snapshot { job: "s".into() }, RunPolicy::default());
        assert_eq!(defer(Some(&w), &snap, RunTrigger::Timer, now, now, &tz), Defer::Go);
        let short = task(TaskAction::SmartTest { disk: "sda".into(), test: SmartTestKind::Short }, RunPolicy::default());
        assert_eq!(defer(Some(&w), &short, RunTrigger::Timer, now, now, &tz), Defer::Go);
    }

    #[test]
    fn own_windows_unless_opted_in() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let w = nightly(1, 5);
        let now = ms("2026-11-17T14:00", &tz);
        // Docker's automatic updates keep their own start window
        let docker = task(TaskAction::DockerAutoUpdate, RunPolicy { window_min: Some(60), ..Default::default() });
        assert!(!docker.follows_maintenance());
        assert_eq!(defer(Some(&w), &docker, RunTrigger::Timer, now, now, &tz), Defer::Go);
        // opted in: the window opens after the end of its own start window, skipped
        let opted = task(TaskAction::DockerAutoUpdate, RunPolicy { window_min: Some(60), maintenance: Some(true), ..Default::default() });
        assert!(matches!(defer(Some(&w), &opted, RunTrigger::Timer, now, now, &tz), Defer::Skip(_)));
        // a heavy task the user took out of the window
        let out = task(TaskAction::Scrub { filesystem: "/srv/a".into() }, RunPolicy { maintenance: Some(false), ..Default::default() });
        assert_eq!(defer(Some(&w), &out, RunTrigger::Timer, now, now, &tz), Defer::Go);
    }
}
