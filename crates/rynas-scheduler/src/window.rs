//! Weekly-window triggers (`TaskTrigger::Window`): the timer's calendar lines and the gate's
//! rule of one start per block.
//!
//! The window is 168 cells `0`/`1`, Sunday first (`window[day * 24 + hour]`), in the NAS's
//! local time, like every weekly grid of rynas. The timer gets one `OnCalendar=` line per
//! hour at which some block starts, with exactly the days it starts on (before S3 one
//! expression made of every start day times every start hour, which also fired inside
//! blocks and outside the window). A block of two hours or more also gets its second hour:
//! systemd skips a time that does not exist (the spring-forward night), so a block starting
//! in the missing hour would not run that night; its second hour starts it instead. On every
//! other day that second start is not due ([`start_due`]): one start per block goes through
//! the gate, at the block's start, or in its second hour when its start did not exist.

use jiff::civil::DateTime;
use jiff::tz::{AmbiguousOffset, TimeZone};
use jiff::{Timestamp, ToSpan};

const CELLS: usize = 168;
const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn on(w: &[u8], i: usize) -> bool {
    w.get(i % CELLS) == Some(&b'1')
}

fn cell_of(dt: &DateTime) -> usize {
    dt.weekday().to_sunday_zero_offset() as usize * 24 + dt.hour() as usize
}

/// The cells where a block starts (on, the hour before off).
fn starts(w: &[u8]) -> Vec<usize> {
    (0..CELLS).filter(|&i| on(w, i) && !on(w, i + CELLS - 1)).collect()
}

/// The `OnCalendar=` lines of a window, sorted by hour. A window that is on all week has no
/// block start: it fires every Sunday at midnight (as before).
pub fn calendars(window: &str) -> Vec<String> {
    let w = window.as_bytes();
    let starts = starts(w);
    if starts.is_empty() {
        return vec!["Sun *-*-* 00:00:00".into()];
    }
    // hour → days (bit per Sunday-first day)
    let mut by_hour = [0u8; 24];
    for &i in &starts {
        by_hour[i % 24] |= 1 << (i / 24);
        let second = (i + 1) % CELLS;
        if on(w, second) {
            by_hour[second % 24] |= 1 << (second / 24);
        }
    }
    by_hour
        .iter()
        .enumerate()
        .filter(|(_, d)| **d != 0)
        .map(|(h, d)| {
            if *d == 0x7f {
                format!("*-*-* {h:02}:00:00")
            } else {
                let days: Vec<&str> = (0..7).filter(|x| d & (1 << x) != 0).map(|x| DAYS[x]).collect();
                format!("{} *-*-* {h:02}:00:00", days.join(","))
            }
        })
        .collect()
}

fn hour_start(dt: DateTime) -> DateTime {
    DateTime::new(dt.year(), dt.month(), dt.day(), dt.hour(), 0, 0, 0).unwrap_or(dt)
}

/// The civil start of the block that `now` is in; `None` outside the window. A window on all
/// week starts a week back.
fn block_start(w: &[u8], now: DateTime) -> Option<DateTime> {
    let mut dt = hour_start(now);
    if !on(w, cell_of(&dt)) {
        return None;
    }
    for _ in 0..CELLS {
        let prev = dt.checked_sub(1.hour()).ok()?;
        if !on(w, cell_of(&prev)) {
            break;
        }
        dt = prev;
    }
    Some(dt)
}

/// A timer start of a window task at `now_ms` is due: inside a block, at the block's start
/// hour (or in its second hour when the start hour did not exist that day), and no run of the
/// task started in this block yet (`last_start_ms`). Every other start (the second hour on a
/// normal day, a start systemd queued behind a run that lasted to the block's end, a timer
/// restarted inside a repeated hour) is not due.
pub fn start_due(window: &str, tz: &TimeZone, now_ms: u64, last_start_ms: Option<u64>) -> bool {
    let Ok(now) = Timestamp::from_millisecond(now_ms as i64) else {
        return false;
    };
    let w = window.as_bytes();
    let local = now.to_zoned(tz.clone()).datetime();
    let Some(start) = block_start(w, local) else {
        return false;
    };
    let amb = tz.to_ambiguous_zoned(start);
    let gap = matches!(amb.offset(), AmbiguousOffset::Gap { .. });
    // a start in the gap counts from the moment after the jump (jiff's compatible choice)
    let Ok(start_z) = amb.compatible() else {
        return false;
    };
    let start_ms = start_z.timestamp().as_millisecond().max(0) as u64;
    if last_start_ms.is_some_and(|l| l >= start_ms) {
        return false;
    }
    let hour = hour_start(local);
    hour == start || (gap && start.checked_add(1.hour()).is_ok_and(|s| s == hour))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(cells: &[(usize, std::ops::Range<usize>)]) -> String {
        let mut w = vec![b'0'; CELLS];
        for (d, hs) in cells {
            for h in hs.clone() {
                w[d * 24 + h] = b'1';
            }
        }
        String::from_utf8(w).unwrap()
    }

    fn ms(s: &str, tz: &TimeZone) -> u64 {
        let dt: DateTime = s.parse().unwrap();
        tz.to_ambiguous_zoned(dt).compatible().unwrap().timestamp().as_millisecond() as u64
    }

    #[test]
    fn calendar_lines_are_the_exact_starts() {
        // every night 01:00-06:00 (the deduplication default)
        let mut d = Vec::new();
        for day in 0..7 {
            d.push((day, 1..6));
        }
        assert_eq!(calendars(&window(&d)), ["*-*-* 01:00:00", "*-*-* 02:00:00"]);
        // Mon 01-03 and Wed 13-15: no Monday 13:00 nor Wednesday 01:00 any more
        assert_eq!(
            calendars(&window(&[(1, 1..3), (3, 13..15)])),
            ["Mon *-*-* 01:00:00", "Mon *-*-* 02:00:00", "Wed *-*-* 13:00:00", "Wed *-*-* 14:00:00"]
        );
        // a one-hour block has no second hour; a block across midnight
        assert_eq!(calendars(&window(&[(2, 5..6)])), ["Tue *-*-* 05:00:00"]);
        assert_eq!(calendars(&window(&[(6, 23..24), (0, 0..2)])), ["Sun *-*-* 00:00:00", "Sat *-*-* 23:00:00"]);
        assert_eq!(calendars(&"1".repeat(168)), ["Sun *-*-* 00:00:00"]);
    }

    #[test]
    fn one_start_per_block() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let w = window(&[(3, 1..6)]);
        // Wednesday 2026-11-11: the 01:00 start is due, the 02:00 one is not
        assert!(start_due(&w, &tz, ms("2026-11-11T01:00:01", &tz), None));
        assert!(!start_due(&w, &tz, ms("2026-11-11T02:00:00", &tz), None));
        // a run already started in this block: a later start (queued behind it) is not due
        let ran = Some(ms("2026-11-11T01:00:02", &tz));
        assert!(!start_due(&w, &tz, ms("2026-11-11T01:30:00", &tz), ran));
        // the run of last week does not count; outside the window nothing is due
        let last_week = Some(ms("2026-11-04T01:00:02", &tz));
        assert!(start_due(&w, &tz, ms("2026-11-11T01:00:01", &tz), last_week));
        assert!(!start_due(&w, &tz, ms("2026-11-11T06:00:00", &tz), None));
    }

    #[test]
    fn a_block_starting_in_the_missing_hour_starts_an_hour_later() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        // Sunday 2026-03-29: 02:00 does not exist; a block 02-05 starts at 03:00
        let w = window(&[(0, 2..5)]);
        assert!(start_due(&w, &tz, ms("2026-03-29T03:00:00", &tz), None));
        // a week later 02:00 exists: its 03:00 start is not due
        assert!(!start_due(&w, &tz, ms("2026-04-05T03:00:00", &tz), None));
        assert!(start_due(&w, &tz, ms("2026-04-05T02:00:00", &tz), None));
        // fall back (2026-10-25): 02:00 happens twice, one start
        let first = ms("2026-10-25T02:00:00", &tz);
        assert!(start_due(&w, &tz, first, None));
        assert!(!start_due(&w, &tz, first + 3_600_000, Some(first)));
    }
}
