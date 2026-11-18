//! Weekly windows (168 cells `0`/`1`, Sunday first, `window[day * 24 + hour]`, the format of
//! every weekly grid of rynas) and the interval schedule of the PC agents, with the same
//! rules on the NAS (the overview, the previews, the global maintenance window) and on the
//! PCs (`rynas-agent`'s loop): S6 of `docs/research/scheduling.md`.
//!
//! Local hours follow the clock on the wall: on the spring-forward night the missing hour has
//! no cell to be in, and the repeated hour of the fall-back night is the same cell twice.
//!
//! The interval schedule ([`interval_due`], [`start_at`]): a backup is due `every` after the
//! end of the last good one, never before `delay` after the start of the agent (anacron-like:
//! a PC that was off is backed up soon after it comes back); with an allowed window it starts
//! only inside it, at the first moment the window is open from the due time on.

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;

use crate::spec::CalendarSpec;

/// Cells of a weekly window (one per hour, Sunday first).
pub const CELLS: usize = 168;

/// How far ahead [`opens_at`] looks (a week and a day: every cell is met).
const LOOK_AHEAD_H: u64 = 8 * 24;

/// A weekly window is 168 cells `0`/`1` with at least one `1`.
pub fn valid(w: &str) -> bool {
    w.len() == CELLS && w.bytes().all(|b| b == b'0' || b == b'1') && w.contains('1')
}

/// The cell of a weekly window is on (`cell` wraps around the week).
pub fn cell_on(w: &str, cell: usize) -> bool {
    w.as_bytes().get(cell % CELLS) == Some(&b'1')
}

fn ts(ms: u64) -> Option<Timestamp> {
    Timestamp::from_millisecond(i64::try_from(ms).ok()?).ok()
}

/// The cell of a local date and time.
pub fn cell_of(dt: &DateTime) -> usize {
    dt.weekday().to_sunday_zero_offset() as usize * 24 + dt.hour() as usize
}

/// The window is open at `ms`.
pub fn is_open(window: &str, tz: &TimeZone, ms: u64) -> bool {
    ts(ms).is_some_and(|t| cell_on(window, cell_of(&t.to_zoned(tz.clone()).datetime())))
}

/// When the window is open next, at or after `ms`: `ms` itself when it is open now, else the
/// start of its next "on" hour. `None` for a window without any "on" cell.
pub fn opens_at(window: &str, tz: &TimeZone, ms: u64) -> Option<u64> {
    let mut at = ms;
    for _ in 0..LOOK_AHEAD_H + 2 {
        let t = ts(at)?;
        let local = t.to_zoned(tz.clone()).datetime();
        if cell_on(window, cell_of(&local)) {
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

/// When the window closes next after `ms` (the end of the "on" block `ms` is in), `None`
/// when it is closed at `ms` or never closes (every cell on).
pub fn closes_at(window: &str, tz: &TimeZone, ms: u64) -> Option<u64> {
    if !is_open(window, tz, ms) {
        return None;
    }
    let mut at = ms;
    for _ in 0..LOOK_AHEAD_H + 2 {
        let local = ts(at)?.to_zoned(tz.clone()).datetime();
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
        if !is_open(window, tz, at) {
            return Some(at);
        }
    }
    None
}

/// When an interval backup is due (ignoring any window): `every_ms` after the last good
/// run's end, at least `delay_ms` after the agent started (`started_ms`); never backed up:
/// after the delay.
pub fn interval_due(last_ok_ms: Option<u64>, every_ms: u64, started_ms: u64, delay_ms: u64) -> u64 {
    let due = last_ok_ms.map_or(0, |ok| ok.saturating_add(every_ms.max(1)));
    due.max(started_ms.saturating_add(delay_ms))
}

/// When a start due at `due_ms` may happen: at once without a window (or when it is open),
/// else when the window opens next. `None`: a window without any "on" cell.
pub fn start_at(window: Option<&str>, tz: &TimeZone, due_ms: u64) -> Option<u64> {
    match window {
        None => Some(due_ms),
        Some(w) => opens_at(w, tz, due_ms),
    }
}

/// The next starts of an interval schedule from `first_due_ms`, up to `to_ms` and at most
/// `max` (`true` when cut): each start where [`start_at`] puts it, the next one due
/// `every_ms` plus the run's expected length (`length_ms`) after it, as the agent counts from
/// the end of the last good run.
pub fn interval_starts(
    first_due_ms: u64,
    every_ms: u64,
    length_ms: u64,
    window: Option<&str>,
    tz: &TimeZone,
    to_ms: u64,
    max: usize,
) -> (Vec<u64>, bool) {
    if every_ms == 0 {
        return (Vec::new(), false);
    }
    next_starts(first_due_ms, |end| Some(end.saturating_add(every_ms)), length_ms, window, tz, to_ms, max)
}

/// When a calendar backup is due (ignoring any window), the way `Persistent=true` catches up:
/// the first time of the calendar after the end of the last good run (a time missed while
/// the PC was off is due at once, and only once: the run that catches it up moves the base
/// past every missed time), at least `delay_ms` after the agent started; never backed up:
/// after the delay. `u64::MAX` when the calendar has no time left.
pub fn calendar_due(spec: &CalendarSpec, tz: &TimeZone, last_ok_ms: Option<u64>, started_ms: u64, delay_ms: u64) -> u64 {
    let after_start = started_ms.saturating_add(delay_ms);
    match last_ok_ms {
        None => after_start,
        Some(ok) => next_time(spec, tz, ok).unwrap_or(u64::MAX).max(after_start),
    }
}

/// When a backup "also at every start of the PC" is due: `delay_ms` after the agent started,
/// unless a good run already ended in this boot of the PC (`done_this_boot`: the agent keys it
/// by the kernel's boot id, so a restart of the agent alone runs nothing again); then not
/// before the next boot (`u64::MAX`).
pub fn boot_due(done_this_boot: bool, started_ms: u64, delay_ms: u64) -> u64 {
    if done_this_boot {
        return u64::MAX;
    }
    started_ms.saturating_add(delay_ms)
}

/// The calendar's first time after `ms`.
fn next_time(spec: &CalendarSpec, tz: &TimeZone, ms: u64) -> Option<u64> {
    spec.next_after(tz, ts(ms)?).map(|t| t.as_millisecond().max(0) as u64)
}

/// The next starts of a calendar schedule from `first_due_ms` (see [`interval_starts`]):
/// each next one at the calendar's first time after the previous run's expected end.
pub fn calendar_starts(
    spec: &CalendarSpec,
    first_due_ms: u64,
    length_ms: u64,
    window: Option<&str>,
    tz: &TimeZone,
    to_ms: u64,
    max: usize,
) -> (Vec<u64>, bool) {
    next_starts(first_due_ms, |end| next_time(spec, tz, end), length_ms, window, tz, to_ms, max)
}

/// A PC's backup plan: due by its calendar (when it has one) or every `every_ms`, from the
/// last good run, not before the delay after the agent's start. One rule for the agent's loop,
/// the NAS's overview and previews.
pub fn plan_due(calendar: Option<&CalendarSpec>, every_ms: u64, tz: &TimeZone, last_ok_ms: Option<u64>, started_ms: u64, delay_ms: u64) -> u64 {
    match calendar {
        Some(spec) => calendar_due(spec, tz, last_ok_ms, started_ms, delay_ms),
        None => interval_due(last_ok_ms, every_ms, started_ms, delay_ms),
    }
}

/// When a PC's next scheduled backup is due (ignoring the pause, the window and the retries
/// of a failed run), as `rynas-agent` counts it: [`plan_due`] (the interval when the calendar
/// has no time left, so backups go on), and with "also at every start of the PC"
/// ([`boot_due`]) whichever comes first. `boot_done`: `None` without "also at every start",
/// else whether a good run already ended in this boot of the PC. The agent's loop and the
/// NAS's view of a PC (is it late, when is the next backup) both use it.
#[allow(clippy::too_many_arguments)]
pub fn agent_due(
    calendar: Option<&CalendarSpec>,
    every_ms: u64,
    tz: &TimeZone,
    last_ok_ms: Option<u64>,
    started_ms: u64,
    delay_ms: u64,
    boot_done: Option<bool>,
) -> u64 {
    let mut due = plan_due(calendar, every_ms, tz, last_ok_ms, started_ms, delay_ms);
    if calendar.is_some() && due == u64::MAX {
        due = interval_due(last_ok_ms, every_ms, started_ms, delay_ms);
    }
    match boot_done {
        Some(done) => due.min(boot_due(done, started_ms, delay_ms)),
        None => due,
    }
}

fn next_starts(
    first_due_ms: u64,
    next_due: impl Fn(u64) -> Option<u64>,
    length_ms: u64,
    window: Option<&str>,
    tz: &TimeZone,
    to_ms: u64,
    max: usize,
) -> (Vec<u64>, bool) {
    let mut out = Vec::new();
    let mut due = first_due_ms;
    while let Some(at) = start_at(window, tz, due) {
        if at > to_ms {
            break;
        }
        if out.len() >= max {
            return (out, true);
        }
        out.push(at);
        match next_due(at.saturating_add(length_ms)) {
            Some(d) if d > at => due = d,
            _ => break,
        }
    }
    (out, false)
}

/// The gaps between a calendar's next `n` + 1 times from `now_ms`.
fn gaps(spec: &CalendarSpec, tz: &TimeZone, now_ms: u64, n: usize) -> Vec<u64> {
    let mut times = Vec::new();
    let mut at = now_ms;
    while times.len() <= n {
        match next_time(spec, tz, at) {
            Some(t) => {
                times.push(t);
                at = t;
            }
            None => break,
        }
    }
    times.windows(2).map(|w| w[1] - w[0]).collect()
}

/// The hours between a calendar's next times (the median of the next eight gaps from `now_ms`),
/// 1 to 8760: what an agent that knows only intervals is told instead (older agents follow
/// any interval; a monthly calendar is about 730 h).
pub fn calendar_every_h(spec: &CalendarSpec, tz: &TimeZone, now_ms: u64) -> u32 {
    let mut g = gaps(spec, tz, now_ms, 8);
    if g.is_empty() {
        return 24;
    }
    g.sort_unstable();
    let h = (g[g.len() / 2] + 1_800_000) / 3_600_000;
    h.clamp(1, 8760) as u32
}

/// The shortest gap between a calendar's next 50 times from `now_ms` (`None`: fewer than two
/// times left): a PC's plan refuses times closer than an hour.
pub fn calendar_min_gap_ms(spec: &CalendarSpec, tz: &TimeZone, now_ms: u64) -> Option<u64> {
    gaps(spec, tz, now_ms, 50).into_iter().min()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz() -> TimeZone {
        TimeZone::get("Europe/Rome").unwrap()
    }

    fn ms(s: &str) -> u64 {
        let dt: DateTime = s.parse().unwrap();
        tz().to_ambiguous_zoned(dt).compatible().unwrap().timestamp().as_millisecond() as u64
    }

    /// Every day from `from` to `to` (hours, `to` excluded).
    fn nightly(from: usize, to: usize) -> String {
        let mut w = vec![b'0'; CELLS];
        for d in 0..7 {
            for h in from..to {
                w[d * 24 + h] = b'1';
            }
        }
        String::from_utf8(w).unwrap()
    }

    #[test]
    fn open_and_close() {
        let w = nightly(1, 5);
        assert!(valid(&w));
        assert!(!valid(&"0".repeat(CELLS)));
        assert!(is_open(&w, &tz(), ms("2026-11-17T02:20")));
        assert!(!is_open(&w, &tz(), ms("2026-11-17T14:20")));
        assert_eq!(opens_at(&w, &tz(), ms("2026-11-17T14:20")), Some(ms("2026-11-18T01:00")));
        assert_eq!(opens_at(&w, &tz(), ms("2026-11-17T02:20")), Some(ms("2026-11-17T02:20")));
        assert_eq!(closes_at(&w, &tz(), ms("2026-11-17T02:20")), Some(ms("2026-11-17T05:00")));
        assert_eq!(closes_at(&w, &tz(), ms("2026-11-17T14:20")), None);
        assert_eq!(closes_at(&"1".repeat(CELLS), &tz(), 0), None);
        assert_eq!(opens_at(&"0".repeat(CELLS), &tz(), 0), None);
    }

    #[test]
    fn dst() {
        // spring forward in Rome: 02:00 does not exist on 2026-03-29, the 02 cell is skipped
        let w = nightly(2, 3);
        assert_eq!(opens_at(&w, &tz(), ms("2026-03-29T01:30")), Some(ms("2026-03-30T02:00")));
        let w = nightly(2, 4);
        assert_eq!(opens_at(&w, &tz(), ms("2026-03-29T01:30")), Some(ms("2026-03-29T03:00")));
        // fall back: the repeated 02 hour is the same cell twice, open for two real hours
        let w = nightly(2, 3);
        let first = ms("2026-10-25T02:00");
        assert_eq!(closes_at(&w, &tz(), first), Some(first + 2 * 3_600_000));
    }

    #[test]
    fn interval_rules() {
        let h = 3_600_000u64;
        // never backed up: after the delay from the start
        assert_eq!(interval_due(None, 24 * h, 1_000, 600_000), 601_000);
        // the interval from the last good run, the delay still holds
        assert_eq!(interval_due(Some(10 * h), 24 * h, 0, 600_000), 34 * h);
        assert_eq!(interval_due(Some(0), h, 50 * h, 600_000), 50 * h + 600_000);
        // without a window: at once; with one: when it opens
        let w = nightly(22, 24);
        assert_eq!(start_at(None, &tz(), 5), Some(5));
        assert_eq!(start_at(Some(&w), &tz(), ms("2026-11-17T14:00")), Some(ms("2026-11-17T22:00")));
        assert_eq!(start_at(Some(&w), &tz(), ms("2026-11-17T23:10")), Some(ms("2026-11-17T23:10")));
        // every 6 h with a two-hour night window: one start a night (the next due falls in
        // the day), another one when the run is short and the window still open
        let (s, cut) = interval_starts(ms("2026-11-17T14:00"), 6 * h, 0, Some(&w), &tz(), ms("2026-11-20T00:00"), 10);
        assert!(!cut);
        assert_eq!(s, vec![ms("2026-11-17T22:00"), ms("2026-11-18T22:00"), ms("2026-11-19T22:00")]);
        let (s, _) = interval_starts(ms("2026-11-17T14:00"), h, 0, Some(&w), &tz(), ms("2026-11-18T00:00"), 10);
        assert_eq!(s, vec![ms("2026-11-17T22:00"), ms("2026-11-17T23:00")]);
        // the length of a run counts (from the end of the last good one)
        let (s, cut) = interval_starts(0, h, h, None, &tz(), 5 * h, 2);
        assert_eq!((s, cut), (vec![0, 2 * h], true));
    }

    /// A calendar plan catches a missed time up once (the way `Persistent=true` does), after
    /// the delay from the agent's start; inside the window when there is one.
    #[test]
    fn calendar_rules() {
        let h = 3_600_000u64;
        let daily = CalendarSpec::parse("*-*-* 02:00:00").unwrap();
        // backed up yesterday at 02:10, PC off at 02:00 today, on at 08:00 with 10 min delay:
        // due at 08:10, once
        let last = ms("2026-11-16T02:10");
        let start = ms("2026-11-17T08:00");
        assert_eq!(calendar_due(&daily, &tz(), Some(last), start, 600_000), start + 600_000);
        // after that run (08:12 end) the next is tomorrow at 02:00: no second catch-up
        assert_eq!(calendar_due(&daily, &tz(), Some(start + 720_000), start, 600_000), ms("2026-11-18T02:00"));
        // up all night: due at 02:00
        assert_eq!(calendar_due(&daily, &tz(), Some(last), ms("2026-11-16T09:00"), 600_000), ms("2026-11-17T02:00"));
        // never backed up: after the delay
        assert_eq!(calendar_due(&daily, &tz(), None, start, 600_000), start + 600_000);
        // a date in the past: never
        let gone = CalendarSpec::parse("2020-01-01 00:00:00").unwrap();
        assert_eq!(calendar_due(&gone, &tz(), Some(last), start, 0), u64::MAX);
        // next starts: daily, or inside a 22–24 window (02:00 is outside: the next opening)
        let (s, _) = calendar_starts(&daily, ms("2026-11-17T02:00"), 0, None, &tz(), ms("2026-11-19T12:00"), 10);
        assert_eq!(s, vec![ms("2026-11-17T02:00"), ms("2026-11-18T02:00"), ms("2026-11-19T02:00")]);
        let w = nightly(22, 24);
        let (s, _) = calendar_starts(&daily, ms("2026-11-17T02:00"), 0, Some(&w), &tz(), ms("2026-11-18T23:00"), 10);
        assert_eq!(s, vec![ms("2026-11-17T22:00"), ms("2026-11-18T22:00")]);
        // the interval an older agent is told
        assert_eq!(calendar_every_h(&daily, &tz(), last), 24);
        assert_eq!(calendar_every_h(&CalendarSpec::parse("Mon *-*-* 03:00:00").unwrap(), &tz(), last), 168);
        assert_eq!(calendar_every_h(&CalendarSpec::parse("*-*-* 00/6:00:00").unwrap(), &tz(), last), 6);
        let monthly = calendar_every_h(&CalendarSpec::parse("*-*-01 03:00:00").unwrap(), &tz(), last);
        assert!((672..=744).contains(&monthly), "{monthly}");
        // the shortest gap (a PC refuses less than an hour)
        assert_eq!(calendar_min_gap_ms(&CalendarSpec::parse("*:0/5").unwrap(), &tz(), last), Some(300_000));
        assert_eq!(calendar_min_gap_ms(&CalendarSpec::parse("*-*-* 08,20:00").unwrap(), &tz(), last), Some(12 * h));
        assert_eq!(calendar_min_gap_ms(&gone, &tz(), last), None);
        assert_eq!(plan_due(None, 24 * h, &tz(), Some(last), 0, 0), last + 24 * h);
        // at every start: after the delay, however recent the last run of an earlier boot;
        // not again in the same boot (an agent restart is not a boot)
        let boot = ms("2026-11-17T08:00");
        assert_eq!(boot_due(false, boot + 20_000, 600_000), boot + 620_000);
        assert_eq!(boot_due(true, boot + 3 * h, 600_000), u64::MAX);
        assert_eq!(plan_due(Some(&daily), 24 * h, &tz(), Some(last), 0, 0), ms("2026-11-17T02:00"));
    }

    #[test]
    fn agent_due_rules() {
        let h = 3_600_000;
        let ok = ms("2026-11-16T04:05");
        let spec = CalendarSpec::parse("*-*-* 04:00:00").unwrap();
        // set times: the next one after the last good run; a missed one at least the delay
        // after the start
        assert_eq!(agent_due(Some(&spec), 24 * h, &tz(), Some(ok), 0, 0, None), ms("2026-11-17T04:00"));
        assert_eq!(agent_due(Some(&spec), 24 * h, &tz(), Some(ok), ms("2026-11-18T09:00"), 600_000, None), ms("2026-11-18T09:10"));
        // an expression without any time left: the interval, so backups go on
        let past = CalendarSpec::parse("2020-01-01 00:00:00").unwrap();
        assert_eq!(agent_due(Some(&past), 24 * h, &tz(), Some(ok), 0, 0, None), ok + 24 * h);
        // also at every start: whichever comes first, nothing more once done in this boot
        let start = ms("2026-11-16T08:00");
        assert_eq!(agent_due(None, 24 * h, &tz(), Some(ok), start, 600_000, Some(false)), start + 600_000);
        assert_eq!(agent_due(None, 24 * h, &tz(), Some(ok), start, 600_000, Some(true)), ok + 24 * h);
    }
}
