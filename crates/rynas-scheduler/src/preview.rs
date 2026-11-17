//! `POST /api/v1/scheduler/preview`: a schedule as the NAS will run it, computed with
//! `rynas-schedule` (systemd 257's calendar rules and DST behaviour) in the NAS's time zone:
//! normalized expression, the preset it is (the structured description), the next runs with
//! their wall-clock time and zone abbreviation, the clock changes of the coming year that
//! change when it runs, and the schedules systemd would never start ("no next run").

use jiff::tz::{Offset, TimeZone};
use jiff::{SignedDuration, Timestamp};
use rynas_api_types::scheduler::{
    CalendarSyntax, DstNote, PreviewRun, SchedulePreview, SchedulePreviewRequest, TaskTrigger,
};
use rynas_schedule::{CalendarSpec, Recurrence};

use crate::calendar::{Converted, cron_to_calendar};

/// Runs listed by default and at most.
pub const DEFAULT_COUNT: u32 = 5;
pub const MAX_COUNT: u32 = 20;
/// Clock changes looked at (from now).
const DST_HORIZON_DAYS: i64 = 366;
/// Weekly windows: hours looked at for block starts.
const WINDOW_HOURS: i64 = 15 * 24;

fn invalid(tz: &str, e: impl Into<String>) -> SchedulePreview {
    SchedulePreview {
        valid: false,
        tz: tz.to_string(),
        error: Some(e.into()),
        ..Default::default()
    }
}

/// The preview of a request, `nas` being the NAS's zone and `now` the base of the runs.
pub fn preview(req: &SchedulePreviewRequest, nas: &TimeZone, now: Timestamp) -> SchedulePreview {
    let tz = match req.tz.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => match TimeZone::get(name) {
            Ok(z) => z,
            Err(_) => return invalid(name, format!("unknown time zone {name:?}")),
        },
        None => nas.clone(),
    };
    let tz_name = tz.iana_name().unwrap_or_default().to_string();
    let count = req
        .count
        .unwrap_or(DEFAULT_COUNT)
        .clamp(1, MAX_COUNT) as usize;
    let trigger = match (&req.expression, &req.trigger) {
        (Some(e), _) => match req.syntax {
            CalendarSyntax::Calendar => TaskTrigger::Calendar {
                on_calendar: e.clone(),
                cron: None,
                persistent: true,
                randomized_delay_s: 0,
            },
            CalendarSyntax::Cron => TaskTrigger::Calendar {
                on_calendar: String::new(),
                cron: Some(e.clone()),
                persistent: true,
                randomized_delay_s: 0,
            },
        },
        (None, Some(t)) => t.clone(),
        (None, None) => return invalid(&tz_name, "give an expression or a trigger"),
    };
    let ok = SchedulePreview {
        valid: true,
        tz: tz_name.clone(),
        ..Default::default()
    };
    let expr = match &trigger {
        TaskTrigger::Calendar {
            cron: Some(c), ..
        } if !c.trim().is_empty() => match cron_to_calendar(c) {
            Ok(Converted::Calendar(c)) => c,
            Ok(Converted::Boot) => {
                return invalid(&tz_name, "@reboot is a boot trigger, not a calendar");
            }
            Err(e) => return invalid(&tz_name, e),
        },
        TaskTrigger::Calendar { on_calendar, .. } => on_calendar.trim().to_string(),
        TaskTrigger::Recurrence { recurrence } => match recurrence.to_calendar() {
            Ok(c) => c,
            Err(e) => return invalid(&tz_name, e.to_string()),
        },
        TaskTrigger::Window { window, .. } => {
            if !rynas_api_types::dedup::window_valid(window) {
                return invalid(&tz_name, "a weekly window is 168 characters 0/1 with at least one 1");
            }
            let runs = window_starts(window, &tz, now, count);
            return SchedulePreview {
                runs: runs.iter().map(|t| run_of(*t, &tz)).collect(),
                ..ok
            };
        }
        TaskTrigger::Interval { hours, .. } if *hours == 0 || *hours > 8760 => {
            return invalid(&tz_name, "the interval is 1 to 8760 hours");
        }
        // every so many hours from the last good run (or now), only inside the allowed
        // window when there is one: the PC agents' rules (S6, `rynas_schedule::window`)
        TaskTrigger::Interval { hours, .. } => {
            let window = req.window.as_deref().map(str::trim).filter(|w| !w.is_empty());
            if window.is_some_and(|w| !rynas_schedule::window::valid(w)) {
                return invalid(&tz_name, "a weekly window is 168 characters 0/1 with at least one 1");
            }
            let now_ms = now.as_millisecond().max(0) as u64;
            let every = u64::from(*hours) * 3_600_000;
            let first = req.last_ms.map_or(now_ms, |l| l.saturating_add(every)).max(now_ms);
            let (runs, _) = rynas_schedule::window::interval_starts(first, every, 0, window, &tz, u64::MAX, count);
            return SchedulePreview {
                runs: runs
                    .iter()
                    .filter_map(|ms| Timestamp::from_millisecond(i64::try_from(*ms).ok()?).ok())
                    .map(|t| run_of(t, &tz))
                    .collect(),
                ..ok
            };
        }
        // boot, manual, interval (from the last success), after a task: no calendar
        _ => return ok,
    };
    if expr.is_empty() {
        return invalid(&tz_name, "the calendar expression is empty");
    }
    if let Err(e) = crate::calendar::plausible_calendar(&expr) {
        return invalid(&tz_name, e);
    }
    let spec = match CalendarSpec::parse_with(&expr, |z| TimeZone::get(z).is_ok()) {
        Ok(s) => s,
        Err(e) => {
            return SchedulePreview {
                on_calendar: expr,
                ..invalid(&tz_name, e.to_string())
            };
        }
    };
    // a PC's plan at set times (its last good run or its allowed window given): the agent's
    // starts, a time missed since the last run at once (`rynas_schedule::window`)
    let pc_window = req.window.as_deref().map(str::trim).filter(|w| !w.is_empty());
    if pc_window.is_some() || req.last_ms.is_some() {
        if pc_window.is_some_and(|w| !rynas_schedule::window::valid(w)) {
            return invalid(&tz_name, "a weekly window is 168 characters 0/1 with at least one 1");
        }
        let now_ms = now.as_millisecond().max(0) as u64;
        let first = rynas_schedule::window::calendar_due(&spec, &tz, req.last_ms, 0, 0).max(now_ms);
        let (starts, _) = rynas_schedule::window::calendar_starts(&spec, first, 0, pc_window, &tz, u64::MAX, count);
        return SchedulePreview {
            normalized: spec.to_string(),
            preset: Recurrence::from_spec(&spec),
            no_next_run: starts.is_empty(),
            runs: starts
                .iter()
                .filter_map(|ms| Timestamp::from_millisecond(i64::try_from(*ms).ok()?).ok())
                .map(|t| run_of(t, &tz))
                .collect(),
            on_calendar: expr,
            ..ok
        };
    }
    let runs = spec.next_runs(&tz, now, count);
    // the zone the expression runs in: its own suffix, else the preview's
    let own = if spec.utc {
        Some(TimeZone::UTC)
    } else {
        spec.timezone.as_deref().and_then(|z| TimeZone::get(z).ok())
    };
    let run_zone = own.as_ref().unwrap_or(&tz);
    let open_ended = spec.year.is_empty();
    let stuck = if open_ended { stuck_at(&spec, run_zone, now) } else { None };
    SchedulePreview {
        normalized: spec.to_string(),
        preset: Recurrence::from_spec(&spec),
        no_next_run: runs.is_empty(),
        stops: !runs.is_empty() && (stuck.is_some() || runs.len() < count && open_ended),
        stops_on: stuck.filter(|_| !runs.is_empty()),
        dst: dst_notes(&spec, run_zone, now),
        runs: runs.iter().map(|t| run_of(*t, &tz)).collect(),
        on_calendar: expr,
        ..ok
    }
}

fn run_of(t: Timestamp, tz: &TimeZone) -> PreviewRun {
    let z = t.to_zoned(tz.clone());
    let info = tz.to_offset_info(t);
    PreviewRun {
        ms: u64::try_from(t.as_millisecond()).unwrap_or(0),
        local: z.strftime("%Y-%m-%d %H:%M:%S").to_string(),
        abbreviation: info.abbreviation().to_string(),
        offset_s: z.offset().seconds(),
    }
}

/// Clock changes in the next year at which the expression has times that are skipped
/// (forward) or that exist twice (back). The test runs the expression in a zone with the
/// fixed offset of the wall clock around the change, where wall time is linear.
fn dst_notes(spec: &CalendarSpec, tz: &TimeZone, now: Timestamp) -> Vec<DstNote> {
    let end = now
        .checked_add(SignedDuration::from_hours(24 * DST_HORIZON_DAYS))
        .unwrap_or(now);
    let mut out = Vec::new();
    for tr in tz.following(now).take(8) {
        let at = tr.timestamp();
        if at > end {
            break;
        }
        let before = tz.to_offset(at.checked_sub(SignedDuration::from_secs(1)).unwrap_or(at));
        let after = tr.offset();
        let shift = after.seconds() - before.seconds();
        if shift == 0 {
            continue;
        }
        // the wall-clock span that is skipped (forward) or repeated (back) and its zone
        let (fixed, len) = if shift > 0 {
            (before, shift)
        } else {
            (after, -shift)
        };
        if !matches_between(spec, fixed, at, len) {
            continue;
        }
        let hm = |o: Offset| at.to_zoned(TimeZone::fixed(o)).strftime("%H:%M").to_string();
        out.push(DstNote {
            ms: u64::try_from(at.as_millisecond()).unwrap_or(0),
            date: at.to_zoned(TimeZone::fixed(fixed)).strftime("%Y-%m-%d").to_string(),
            forward: shift > 0,
            from: hm(before),
            to: hm(after),
        });
    }
    out
}

/// The local date of the first clock change of the next year where systemd gives up on the
/// expression: following the runs from a day before the change, the next one cannot be
/// computed before a day after it (a timer would be left with no next elapse).
fn stuck_at(spec: &CalendarSpec, tz: &TimeZone, now: Timestamp) -> Option<String> {
    const MAX_RUNS: usize = 5000;
    let end = now.checked_add(SignedDuration::from_hours(24 * DST_HORIZON_DAYS)).ok()?;
    let margin = SignedDuration::from_hours(26);
    for tr in tz.following(now).take(8) {
        let at = tr.timestamp();
        if at > end {
            break;
        }
        let from = at.checked_sub(margin).ok()?.max(now);
        let until = at.checked_add(margin).ok()?;
        let runs = spec.next_runs(tz, from, MAX_RUNS);
        let reached = runs.len() == MAX_RUNS || runs.last().is_some_and(|t| *t > until);
        if !reached {
            return Some(at.to_zoned(tz.clone()).strftime("%Y-%m-%d").to_string());
        }
    }
    None
}

/// The expression has a wall-clock time in `[start, start + len s)`, `start` being `at`
/// read with the fixed offset `fixed`.
fn matches_between(spec: &CalendarSpec, fixed: Offset, at: Timestamp, len: i32) -> bool {
    let z = TimeZone::fixed(fixed);
    let Some(from) = at.checked_sub(SignedDuration::from_micros(1)).ok() else {
        return false;
    };
    let Some(first) = spec.next_after(&z, from) else {
        return false;
    };
    first < at.checked_add(SignedDuration::from_secs(i64::from(len))).unwrap_or(at)
}

/// Starts of the "on" blocks of a weekly window after `now`, at most `n`.
fn window_starts(window: &str, tz: &TimeZone, now: Timestamp, n: usize) -> Vec<Timestamp> {
    let cells = window.as_bytes();
    let on = |t: Timestamp| {
        let z = t.to_zoned(tz.clone());
        let day = z.weekday().to_sunday_zero_offset() as usize;
        cells.get(day * 24 + z.hour() as usize) == Some(&b'1')
    };
    let hour = SignedDuration::from_hours(1);
    let mut out = Vec::new();
    // the first full hour after now
    let Ok(mut t) = now
        .to_zoned(tz.clone())
        .start_of_day()
        .map(|d| d.timestamp())
    else {
        return out;
    };
    while t <= now {
        let Ok(next) = t.checked_add(hour) else { return out };
        t = next;
    }
    // a window on all week has no block start: its run starts every Sunday at midnight
    let all_week = cells.iter().all(|c| *c == b'1');
    if all_week {
        for _ in 0..WINDOW_HOURS {
            let z = t.to_zoned(tz.clone());
            if z.weekday() == jiff::civil::Weekday::Sunday && z.hour() == 0 {
                out.push(t);
                if out.len() == n {
                    break;
                }
            }
            let Ok(next) = t.checked_add(hour) else { break };
            t = next;
        }
        return out;
    }
    let mut prev_on = t.checked_sub(hour).map(on).unwrap_or(false);
    for _ in 0..WINDOW_HOURS {
        let cur = on(t);
        if cur && !prev_on {
            out.push(t);
            if out.len() == n {
                break;
            }
        }
        prev_on = cur;
        let Ok(next) = t.checked_add(hour) else { break };
        t = next;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_schedule::{TimeOfDay, Weekdays};

    fn at(s: &str, zone: &str) -> Timestamp {
        s.parse::<jiff::civil::DateTime>()
            .unwrap()
            .to_zoned(TimeZone::get(zone).unwrap())
            .unwrap()
            .timestamp()
    }

    fn expr(e: &str) -> SchedulePreviewRequest {
        SchedulePreviewRequest {
            expression: Some(e.into()),
            ..Default::default()
        }
    }

    fn rome() -> TimeZone {
        TimeZone::get("Europe/Rome").unwrap()
    }

    #[test]
    fn a_preset_with_its_runs_in_the_nas_zone() {
        let p = preview(&expr("Mon..Fri 02:30"), &rome(), at("2026-11-13 12:00", "Europe/Rome"));
        assert!(p.valid, "{p:?}");
        assert_eq!(p.tz, "Europe/Rome");
        assert_eq!(p.normalized, "Mon..Fri *-*-* 02:30:00");
        assert_eq!(
            p.preset,
            Some(Recurrence::Daily { days: Weekdays::MON_FRI, times: vec![TimeOfDay::new(2, 30)] })
        );
        let local: Vec<&str> = p.runs.iter().map(|r| r.local.as_str()).collect();
        assert_eq!(
            local,
            [
                "2026-11-16 02:30:00",
                "2026-11-17 02:30:00",
                "2026-11-18 02:30:00",
                "2026-11-19 02:30:00",
                "2026-11-20 02:30:00"
            ]
        );
        assert_eq!(p.runs[0].abbreviation, "CET");
        assert_eq!(p.runs[0].offset_s, 3600);
        assert!(!p.no_next_run && !p.stops);
        // the clocks change on Sundays: a weekday schedule never meets them
        assert!(p.dst.is_empty(), "{:?}", p.dst);
        let p = preview(&expr("*-*-* 02:30"), &rome(), at("2026-11-13 12:00", "Europe/Rome"));
        assert_eq!(
            p.dst,
            [DstNote {
                ms: at("2027-03-28 03:00", "Europe/Rome").as_millisecond() as u64,
                date: "2027-03-28".into(),
                forward: true,
                from: "02:00".into(),
                to: "03:00".into(),
            }, DstNote {
                // 01:00 UTC: 03:00 summer time becomes 02:00
                ms: "2027-10-31T01:00:00Z".parse::<Timestamp>().unwrap().as_millisecond() as u64,
                date: "2027-10-31".into(),
                forward: false,
                from: "03:00".into(),
                to: "02:00".into(),
            }]
        );
    }

    #[test]
    fn clock_changes_only_when_they_touch_the_schedule() {
        let from = at("2026-01-10 12:00", "Europe/Rome");
        let p = preview(&expr("*-*-* 04:00"), &rome(), from);
        assert!(p.dst.is_empty(), "{:?}", p.dst);
        let p = preview(&expr("*-*-* 02:15"), &rome(), from);
        let kinds: Vec<(bool, &str, &str, &str)> =
            p.dst.iter().map(|d| (d.forward, d.date.as_str(), d.from.as_str(), d.to.as_str())).collect();
        assert_eq!(
            kinds,
            [(true, "2026-03-29", "02:00", "03:00"), (false, "2026-10-25", "03:00", "02:00")]
        );
        // only on Sundays at 02:15: the change days are Sundays
        let p = preview(&expr("Sat 02:15"), &rome(), from);
        assert!(p.dst.is_empty());
        // the expression's own zone has no DST
        let p = preview(&expr("*-*-* 02:15 UTC"), &rome(), from);
        assert!(p.dst.is_empty());
    }

    #[test]
    fn no_next_run_where_systemd_gives_up() {
        // America/Havana changes at midnight: systemd 257 loops on this expression on the
        // spring change and the timer is left with no next elapse
        let havana = TimeZone::get("America/Havana").unwrap();
        let every2h = expr("*-*-* 00/2:00");
        let p = preview(&every2h, &havana, at("2026-01-10 12:00", "America/Havana"));
        assert!(p.valid && p.runs.len() == 5 && !p.no_next_run, "{p:?}");
        assert!(p.stops);
        assert_eq!(p.stops_on.as_deref(), Some("2026-03-08"));
        let p = preview(&every2h, &havana, at("2026-03-07 18:30", "America/Havana"));
        assert_eq!(p.runs.len(), 2, "{p:?}");
        assert!(p.stops && !p.no_next_run);
        let p = preview(&every2h, &havana, at("2026-03-07 22:30", "America/Havana"));
        assert!(p.valid && p.no_next_run && !p.stops, "{p:?}");
        // the same in Rome is fine
        let p = preview(&every2h, &rome(), at("2026-01-10 12:00", "Europe/Rome"));
        assert!(!p.stops && p.stops_on.is_none());
        // a date in the past
        let p = preview(&expr("2020-01-01 00:00"), &rome(), at("2026-01-01 00:00", "Europe/Rome"));
        assert!(p.valid && p.no_next_run && !p.stops);
        assert!(p.runs.is_empty());
    }

    #[test]
    fn cron_triggers_and_errors() {
        let from = at("2026-11-13 12:00", "Europe/Rome");
        let p = preview(
            &SchedulePreviewRequest {
                expression: Some("30 4 * * 1".into()),
                syntax: CalendarSyntax::Cron,
                ..Default::default()
            },
            &rome(),
            from,
        );
        assert!(p.valid, "{p:?}");
        assert_eq!(p.runs[0].local, "2026-11-16 04:30:00");
        let p = preview(&expr("*-*-* 25:00"), &rome(), from);
        assert!(!p.valid && p.error.is_some());
        let p = preview(&expr(""), &rome(), from);
        assert!(!p.valid);
        let p = preview(
            &SchedulePreviewRequest {
                tz: Some("Mars/Base".into()),
                ..expr("daily")
            },
            &rome(),
            from,
        );
        assert!(!p.valid);
        // another zone than the NAS's
        let p = preview(
            &SchedulePreviewRequest {
                tz: Some("America/New_York".into()),
                count: Some(2),
                ..expr("daily")
            },
            &rome(),
            from,
        );
        assert_eq!(p.tz, "America/New_York");
        assert_eq!(p.runs.len(), 2);
        assert_eq!(p.runs[0].local, "2026-11-14 00:00:00");
        assert_eq!(p.runs[0].abbreviation, "EST");
    }

    #[test]
    fn triggers() {
        let from = at("2026-11-13 12:00", "Europe/Rome");
        let trig = |t: TaskTrigger| {
            preview(&SchedulePreviewRequest { trigger: Some(t), ..Default::default() }, &rome(), from)
        };
        let p = trig(TaskTrigger::Recurrence {
            recurrence: Recurrence::Hours { every: 6, minute: 15, from: 0, to: 23, days: Weekdays(0) },
        });
        assert_eq!(p.on_calendar, "*-*-* 00/6:15:00");
        assert_eq!(p.runs[0].local, "2026-11-13 12:15:00");
        let p = trig(TaskTrigger::Recurrence {
            recurrence: Recurrence::Daily { days: Weekdays(0), times: vec![] },
        });
        assert!(!p.valid);
        for t in [TaskTrigger::Manual, TaskTrigger::Boot { delay_s: 60 }] {
            let p = trig(t);
            assert!(p.valid && p.runs.is_empty() && !p.no_next_run);
        }
        // an interval (a PC's plan, S6): from the last good run, inside the allowed window
        let every6 = TaskTrigger::Interval { hours: 6, after_boot_min: 10 };
        let p = trig(every6.clone());
        let locals: Vec<&str> = p.runs.iter().map(|r| r.local.as_str()).collect();
        assert_eq!(locals, ["2026-11-13 12:00:00", "2026-11-13 18:00:00", "2026-11-14 00:00:00", "2026-11-14 06:00:00", "2026-11-14 12:00:00"]);
        let mut night = vec![b'0'; 168];
        for d in 0..7 {
            for h in 22..24 {
                night[d * 24 + h] = b'1';
            }
        }
        let night = String::from_utf8(night).unwrap();
        let last = at("2026-11-13 09:00", "Europe/Rome").as_millisecond() as u64;
        let p = preview(
            &SchedulePreviewRequest { trigger: Some(every6.clone()), window: Some(night), last_ms: Some(last), count: Some(3), ..Default::default() },
            &rome(),
            from,
        );
        let locals: Vec<&str> = p.runs.iter().map(|r| r.local.as_str()).collect();
        assert_eq!(locals, ["2026-11-13 22:00:00", "2026-11-14 22:00:00", "2026-11-15 22:00:00"]);
        let p = preview(&SchedulePreviewRequest { trigger: Some(every6), window: Some("1".into()), ..Default::default() }, &rome(), from);
        assert!(!p.valid);
        assert!(!trig(TaskTrigger::Interval { hours: 0, after_boot_min: 0 }).valid);
        // Friday 13 November 2026 12:00: window Saturday 01:00-05:00 and Monday 22:00-24:00
        let mut w = vec![b'0'; 168];
        for h in 1..5 {
            w[6 * 24 + h] = b'1';
        }
        w[24 + 22] = b'1';
        w[24 + 23] = b'1';
        let p = trig(TaskTrigger::Window { window: String::from_utf8(w).unwrap(), stop_at_end: true });
        let local: Vec<&str> = p.runs.iter().map(|r| r.local.as_str()).collect();
        assert_eq!(
            local,
            [
                "2026-11-14 01:00:00",
                "2026-11-16 22:00:00",
                "2026-11-21 01:00:00",
                "2026-11-23 22:00:00",
                "2026-11-28 01:00:00"
            ]
        );
        assert!(!trig(TaskTrigger::Window { window: "1".into(), stop_at_end: false }).valid);
    }
}
