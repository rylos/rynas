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
    let mut out = Vec::new();
    if every_ms == 0 {
        return (out, false);
    }
    let mut due = first_due_ms;
    while let Some(at) = start_at(window, tz, due) {
        if at > to_ms {
            break;
        }
        if out.len() >= max {
            return (out, true);
        }
        out.push(at);
        due = at.saturating_add(length_ms).saturating_add(every_ms);
    }
    (out, false)
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
}
