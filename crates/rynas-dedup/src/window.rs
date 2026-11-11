//! The weekly window: 168 hours, Sunday first (`window[day * 24 + hour]`), in local time.
//!
//! The scheduler task fires at the start of each block of `1` hours (one calendar
//! expression: the union of the start days and start hours, so it may also fire inside a
//! block or outside the window; a run checks the window itself). A run stops at the end of
//! the block it started in ([`block_end`]). Arithmetic is on civil hours converted with the
//! system time zone (DST: a missing hour counts as its neighbour, a repeated one once).

use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::{ToSpan, Zoned};
use rynas_api_types::dedup::WINDOW_CELLS;

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn on(w: &[u8], i: usize) -> bool {
    w.get(i % WINDOW_CELLS) == Some(&b'1')
}

fn cell_of(dt: &DateTime) -> usize {
    dt.weekday().to_sunday_zero_offset() as usize * 24 + dt.hour() as usize
}

/// The starts of the blocks (`(day, hour)`, day 0 = Sunday); empty for an empty window and
/// for a window that is on all week (no start).
pub fn block_starts(window: &str) -> Vec<(usize, usize)> {
    let w = window.as_bytes();
    (0..WINDOW_CELLS)
        .filter(|&i| on(w, i) && !on(w, i + WINDOW_CELLS - 1))
        .map(|i| (i / 24, i % 24))
        .collect()
}

/// The `OnCalendar=` expression of the task: days and hours of the block starts. A window
/// that is on all week fires every Sunday at midnight.
pub fn on_calendar(window: &str) -> String {
    let starts = block_starts(window);
    if starts.is_empty() {
        return "Sun *-*-* 00:00:00".into();
    }
    let mut days: Vec<usize> = starts.iter().map(|s| s.0).collect();
    days.sort_unstable();
    days.dedup();
    let mut hours: Vec<usize> = starts.iter().map(|s| s.1).collect();
    hours.sort_unstable();
    hours.dedup();
    let hours = hours.iter().map(|h| format!("{h:02}")).collect::<Vec<_>>().join(",");
    if days.len() == 7 {
        format!("*-*-* {hours}:00:00")
    } else {
        let days = days.iter().map(|d| DAYS[*d]).collect::<Vec<_>>().join(",");
        format!("{days} *-*-* {hours}:00:00")
    }
}

fn hour_start(now: &Zoned) -> DateTime {
    let d = now.datetime();
    DateTime::new(d.year(), d.month(), d.day(), d.hour(), 0, 0, 0).unwrap_or(d)
}

fn zoned(dt: DateTime, tz: &TimeZone) -> Option<Zoned> {
    tz.to_zoned(dt).ok()
}

/// `now` is inside the window.
pub fn inside(window: &str, now: &Zoned) -> bool {
    on(window.as_bytes(), cell_of(&now.datetime()))
}

/// The end of the block `now` is in (the start of the first hour that is off); `None`
/// outside the window. A window on all week ends a week later.
pub fn block_end(window: &str, now: &Zoned) -> Option<Zoned> {
    let w = window.as_bytes();
    let mut dt = hour_start(now);
    if !on(w, cell_of(&dt)) {
        return None;
    }
    for _ in 0..WINDOW_CELLS {
        dt = dt.checked_add(1.hour()).ok()?;
        if !on(w, cell_of(&dt)) {
            return zoned(dt, now.time_zone());
        }
    }
    zoned(dt, now.time_zone())
}

/// The start of the next block after `now` (`None` for a window without starts).
pub fn next_start(window: &str, now: &Zoned) -> Option<Zoned> {
    let w = window.as_bytes();
    let mut dt = hour_start(now);
    for _ in 0..=WINDOW_CELLS {
        dt = dt.checked_add(1.hour()).ok()?;
        let c = cell_of(&dt);
        if on(w, c) && !on(w, c + WINDOW_CELLS - 1) {
            return zoned(dt, now.time_zone());
        }
    }
    None
}

/// When a run started now stops by default: the end of the current block, else the end of
/// the next one.
pub fn default_until(window: &str, now: &Zoned) -> Option<Zoned> {
    block_end(window, now).or_else(|| next_start(window, now).and_then(|s| block_end(window, &s)))
}

pub fn ms(z: &Zoned) -> u64 {
    z.timestamp().as_millisecond().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::dedup::default_window;

    fn at(s: &str, tz: &str) -> Zoned {
        let dt: DateTime = s.parse().unwrap();
        dt.to_zoned(TimeZone::get(tz).unwrap()).unwrap()
    }

    fn window(cells: &[(usize, std::ops::Range<usize>)]) -> String {
        let mut w = vec![b'0'; WINDOW_CELLS];
        for (d, hs) in cells {
            for h in hs.clone() {
                w[d * 24 + h] = b'1';
            }
        }
        String::from_utf8(w).unwrap()
    }

    #[test]
    fn calendar_of_the_default_and_of_mixed_windows() {
        assert_eq!(on_calendar(&default_window()), "*-*-* 01:00:00");
        // Fri 22:00 to Mon 06:00: one block starting on Friday
        let wk = window(&[(5, 22..24), (6, 0..24), (0, 0..24), (1, 0..6)]);
        assert_eq!(block_starts(&wk), [(5, 22)]);
        assert_eq!(on_calendar(&wk), "Fri *-*-* 22:00:00");
        let two = window(&[(1, 1..3), (3, 13..15)]);
        assert_eq!(on_calendar(&two), "Mon,Wed *-*-* 01,13:00:00");
        assert_eq!(on_calendar(&"1".repeat(168)), "Sun *-*-* 00:00:00");
    }

    #[test]
    fn block_end_and_next_start() {
        let w = default_window();
        // Wednesday 2026-11-11 02:30 → ends 06:00 the same day
        let now = at("2026-11-11T02:30", "Europe/Rome");
        assert!(inside(&w, &now));
        assert_eq!(block_end(&w, &now).unwrap().datetime().to_string(), "2026-11-11T06:00:00");
        // 12:00 → outside; next start 01:00 tomorrow, default end 06:00 tomorrow
        let noon = at("2026-11-11T12:00", "Europe/Rome");
        assert!(!inside(&w, &noon) && block_end(&w, &noon).is_none());
        assert_eq!(next_start(&w, &noon).unwrap().datetime().to_string(), "2026-11-12T01:00:00");
        assert_eq!(default_until(&w, &noon).unwrap().datetime().to_string(), "2026-11-12T06:00:00");
        // a block across midnight and a weekend
        let wk = window(&[(5, 22..24), (6, 0..24), (0, 0..24), (1, 0..6)]);
        let sat = at("2026-11-14T10:00", "Europe/Rome");
        assert_eq!(block_end(&wk, &sat).unwrap().datetime().to_string(), "2026-11-16T06:00:00");
    }

    #[test]
    fn daylight_saving_changes() {
        // 2026-03-29 in Rome: 02:00 does not exist; a block 01–04 still ends at 04:00 local
        let w = window(&[(0, 1..4)]);
        let now = at("2026-03-29T01:10", "Europe/Rome");
        let end = block_end(&w, &now).unwrap();
        assert_eq!(end.datetime().to_string(), "2026-03-29T04:00:00");
        assert_eq!(now.timestamp().duration_until(end.timestamp()).as_secs(), 110 * 60);
        // 2026-10-25: 02:00 happens twice; the block 01–04 is one hour longer
        let now = at("2026-10-25T01:00", "Europe/Rome");
        let end = block_end(&w, &now).unwrap();
        assert_eq!(now.timestamp().duration_until(end.timestamp()).as_secs(), 4 * 3600);
    }
}
