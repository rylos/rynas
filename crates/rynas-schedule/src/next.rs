//! When a calendar expression elapses next, computed the way Debian trixie's systemd
//! (257.13: `calendar_spec_next_usec()`, `find_next()` in `src/shared/calendarspec.c`) does,
//! including the way glibc's `mktime()` resolves local times around DST changes. Checked
//! against `systemd-analyze calendar --base-time` (golden file `tests/golden/next.txt`: nine
//! zones around their 2026 changes, among them half-hour shifts, a negative DST and a
//! change at midnight).
//!
//! What happens around a DST change:
//!
//! - a wall-clock time that does not exist (spring forward, 02:30 in Europe) is **skipped**
//!   that day; later times of the same day still happen (`*:00/20` goes 01:40 → 03:00).
//!   systemd 262 does the same;
//! - a wall-clock time that exists twice (fall back) happens **once**, at its first
//!   occurrence, when computed from before it; `hourly` skips the repeated hour;
//! - computed from inside the repeated hour (its second occurrence), a time later in that
//!   hour happens at its second occurrence: a timer without `Persistent=` started there (a
//!   boot, a restart of the timer) runs that time a second time that night. With
//!   `Persistent=` the base is the last trigger, and it does not;
//! - `Persistent=` catch-up is this computation from the last trigger: a skipped time was
//!   never due, so it is not caught up after a boot either;
//! - in a zone whose change is at midnight (America/Havana) some expressions make systemd
//!   give up ("Infinite loop in calendar calculation"): the timer then has no next elapse.
//!   [`CalendarSpec::next_after`] returns `None` there too.

use jiff::civil::{Date, DateTime, Time};
use jiff::tz::{AmbiguousOffset, Offset, TimeZone};
use jiff::{SignedDuration, Timestamp};

use crate::spec::{CalendarSpec, Component, Error, MAX_YEAR, USEC_PER_SEC};

const MAX_ITERATIONS: usize = 1000;

/// glibc's `struct tm` (year in full, month 0-11), `isdst` -1 = unknown. The DST flag is the
/// time zone data's (negative in some zones: Europe/Dublin has DST in winter), as glibc sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tm {
    year: i64,
    mon: i64,
    mday: i64,
    hour: i64,
    min: i64,
    sec: i64,
    isdst: i8,
    wday: i64,
}

/// `mktime()`/`localtime()` of one zone, with glibc's remembered offset (the first guess of
/// the next `mktime()`, which decides between the two instants of a repeated time).
struct Clock<'a> {
    tz: &'a TimeZone,
    guess: Offset,
}

fn civil(tm: &Tm) -> Option<DateTime> {
    let y = tm.year.checked_add(tm.mon.div_euclid(12))?;
    let m = tm.mon.rem_euclid(12) + 1;
    let d0 = Date::new(i16::try_from(y).ok()?, m as i8, 1).ok()?;
    let secs = tm
        .mday
        .checked_sub(1)?
        .checked_mul(86_400)?
        .checked_add(tm.hour.checked_mul(3600)?)?
        .checked_add(tm.min.checked_mul(60)?)?
        .checked_add(tm.sec)?;
    d0.to_datetime(Time::midnight()).checked_add(SignedDuration::from_secs(secs)).ok()
}

impl Clock<'_> {
    fn info(&self, ts: Timestamp) -> (Offset, bool) {
        let i = self.tz.to_offset_info(ts);
        (i.offset(), i.dst().is_dst())
    }

    fn dst(&self, ts: Timestamp) -> bool {
        self.info(ts).1
    }

    fn localtime(&self, ts: Timestamp) -> Tm {
        let (off, dst) = self.info(ts);
        let dt = off.to_datetime(ts);
        Tm {
            year: i64::from(dt.year()),
            mon: i64::from(dt.month()) - 1,
            mday: i64::from(dt.day()),
            hour: i64::from(dt.hour()),
            min: i64::from(dt.minute()),
            sec: i64::from(dt.second()),
            isdst: i8::from(dst),
            wday: i64::from(dt.weekday().to_sunday_zero_offset()),
        }
    }

    fn at(dt: DateTime, off: Offset) -> Option<Timestamp> {
        off.to_timestamp(dt).ok()
    }

    /// The offset of a time near `ts` with the DST flag `dst` (glibc probes a week at a time,
    /// before and after, for about 8.5 years).
    fn neighbour(&self, ts: Timestamp, dst: bool) -> Option<Offset> {
        const STRIDE: i64 = 601_200;
        const BOUND: i64 = 536_454_000 / 2 + STRIDE;
        let mut delta = STRIDE;
        while delta < BOUND {
            for dir in [-1, 1] {
                let Ok(ot) = ts.checked_add(SignedDuration::from_secs(dir * delta)) else { continue };
                let (o, d) = self.info(ot);
                if d == dst {
                    return Some(o);
                }
            }
            delta += STRIDE;
        }
        None
    }

    /// glibc's `mktime()` (checked against it on Debian trixie): normalizes `tm` in place
    /// (fields, isdst, weekday) and returns the instant.
    ///
    /// - a local time that exists once, with an `isdst` it does not have: the offset of a
    ///   nearby time with that flag is used, so the local time moves by the difference;
    /// - a local time that exists twice: the one with the asked flag, else the one of the
    ///   offset of the previous call (glibc's remembered guess);
    /// - a local time that does not exist: the instant the gap's length away, the one whose
    ///   flag differs from the asked one, or with none asked the one with DST.
    fn mktime(&mut self, tm: &mut Tm) -> Option<Timestamp> {
        let dt = civil(tm)?;
        let ask = tm.isdst;
        let (ts, used) = match self.tz.to_ambiguous_timestamp(dt).offset() {
            AmbiguousOffset::Unambiguous { offset } => {
                let ts = Self::at(dt, offset)?;
                let other = (ask >= 0 && self.dst(ts) != (ask != 0)).then(|| self.neighbour(ts, ask != 0)).flatten();
                match other {
                    Some(o) => (Self::at(dt, o)?, o),
                    None => (ts, offset),
                }
            }
            AmbiguousOffset::Fold { before, after } => {
                let (tb, ta) = (Self::at(dt, before)?, Self::at(dt, after)?);
                let (db, da) = (self.dst(tb), self.dst(ta));
                let pick = if ask >= 0 && db != da {
                    if db == (ask != 0) { before } else { after }
                } else if self.guess == before || self.guess == after {
                    self.guess
                } else if db != da {
                    if db { before } else { after }
                } else if self.tz.to_offset(Self::at(dt, self.guess)?) == after {
                    after
                } else {
                    before
                };
                (if pick == before { tb } else { ta }, pick)
            }
            AmbiguousOffset::Gap { before, after } => {
                // forward: dt read with the offset before the gap lands after it, and back
                let (fwd, back) = (Self::at(dt, before)?, Self::at(dt, after)?);
                let (df, db) = (self.dst(fwd), self.dst(back));
                let prefer = |d: bool| if ask < 0 { d } else { d != (ask != 0) };
                if prefer(db) && !prefer(df) { (back, after) } else { (fwd, before) }
            }
        };
        self.guess = used;
        *tm = self.localtime(ts);
        Some(ts)
    }
}

/// `tm_within_bounds()`: 1 = no normalization, 0 = normalized (then `tm` holds the normalized
/// value with the units below the changed one reset), -1 = error or going backwards.
fn within_bounds(clock: &mut Clock, tm: &mut Tm) -> i32 {
    if tm.year > i64::from(MAX_YEAR) {
        return -1;
    }
    let mut t = *tm;
    if clock.mktime(&mut t).is_none() {
        return -1;
    }
    let cmp = if t.year != tm.year {
        let c = t.year.cmp(&tm.year);
        t.mon = 0;
        t.mday = 1;
        c
    } else if t.mon != tm.mon {
        let c = t.mon.cmp(&tm.mon);
        t.mday = 1;
        c
    } else if t.mday != tm.mday {
        let c = t.mday.cmp(&tm.mday);
        t.hour = 0;
        c
    } else if t.hour != tm.hour {
        let c = t.hour.cmp(&tm.hour);
        t.min = 0;
        c
    } else if t.min != tm.min {
        let c = t.min.cmp(&tm.min);
        t.sec = 0;
        c
    } else {
        t.sec.cmp(&tm.sec)
    };
    match cmp {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => {
            *tm = t;
            0
        }
        std::cmp::Ordering::Equal => 1,
    }
}

fn matches_weekday(clock: &mut Clock, bits: Option<u8>, tm: &Tm) -> bool {
    let Some(bits) = bits else { return true };
    let mut t = *tm;
    if clock.mktime(&mut t).is_none() {
        return false;
    }
    let k = if t.wday == 0 { 6 } else { t.wday - 1 };
    bits & (1 << k) != 0
}

fn end_of_month(clock: &mut Clock, tm: &Tm, day: i32) -> i64 {
    let mut t = *tm;
    t.mon += 1;
    t.mday = 1 - i64::from(day);
    if clock.mktime(&mut t).is_none() || t.mon != tm.mon {
        return -1;
    }
    t.mday
}

/// `find_matching_component()`: -1 no match, 0 unchanged, 1 changed.
fn matching(clock: &mut Clock, chain: &[Component], eom: bool, tm: &Tm, val: &mut i64) -> i32 {
    if chain.is_empty() {
        return 0;
    }
    let mut d: Option<i64> = None;
    for c in chain {
        let (mut start, mut stop) = (i64::from(c.start), i64::from(c.stop));
        if eom {
            start = end_of_month(clock, tm, c.start);
            stop = end_of_month(clock, tm, c.stop);
            if stop > 0 {
                std::mem::swap(&mut start, &mut stop);
            }
        }
        let repeat = i64::from(c.repeat);
        if start >= *val {
            if d.is_none_or(|d| start < d) {
                d = Some(start);
            }
        } else if repeat > 0 {
            let k = start + (*val - start + repeat - 1) / repeat * repeat;
            if d.is_none_or(|d| k < d) && (stop < 0 || k <= stop) {
                d = Some(k);
            }
        }
    }
    let Some(d) = d else { return -1 };
    let r = i32::from(*val != d);
    *val = d;
    r
}

fn find_next(spec: &CalendarSpec, clock: &mut Clock, tm: &mut Tm, usec: &mut i64) -> Result<(), Error> {
    let mut c = *tm;
    let mut tm_usec = *usec;
    let reset_from_hour = |c: &mut Tm, u: &mut i64| {
        c.hour = 0;
        c.min = 0;
        c.sec = 0;
        *u = 0;
    };
    // 257.x: once stuck at or before the start, mktime() decides the DST side from then on
    let mut invalidate_dst = false;
    for _ in 0..MAX_ITERATIONS {
        let _ = clock.mktime(&mut c);
        if !invalidate_dst {
            c.isdst = -1;
        }

        let mut v = c.year;
        let r = matching(clock, &spec.year, false, &c, &mut v);
        c.year = v;
        if r > 0 {
            c.mon = 0;
            c.mday = 1;
            reset_from_hour(&mut c, &mut tm_usec);
        }
        if r < 0 || within_bounds(clock, &mut c) <= 0 {
            return Err(Error::Invalid);
        }

        let mut v = c.mon + 1;
        let mut r = matching(clock, &spec.month, false, &c, &mut v);
        c.mon = v - 1;
        if r > 0 {
            c.mday = 1;
            reset_from_hour(&mut c, &mut tm_usec);
        }
        if r >= 0 {
            r = within_bounds(clock, &mut c);
        }
        if r < 0 {
            c.year += 1;
            c.mon = 0;
            c.mday = 1;
            reset_from_hour(&mut c, &mut tm_usec);
            continue;
        }
        if r == 0 {
            continue;
        }

        let mut v = c.mday;
        let mut r = matching(clock, &spec.day, spec.end_of_month, &c, &mut v);
        c.mday = v;
        if r > 0 {
            reset_from_hour(&mut c, &mut tm_usec);
        }
        if r >= 0 {
            r = within_bounds(clock, &mut c);
        }
        if r < 0 {
            c.mon += 1;
            c.mday = 1;
            reset_from_hour(&mut c, &mut tm_usec);
            continue;
        }
        if r == 0 {
            continue;
        }

        if !matches_weekday(clock, spec.weekdays, &c) {
            c.mday += 1;
            reset_from_hour(&mut c, &mut tm_usec);
            continue;
        }

        let mut v = c.hour;
        let mut r = matching(clock, &spec.hour, false, &c, &mut v);
        c.hour = v;
        if r > 0 {
            c.min = 0;
            c.sec = 0;
            tm_usec = 0;
        }
        if r >= 0 {
            r = within_bounds(clock, &mut c);
        }
        if r < 0 {
            c.mday += 1;
            reset_from_hour(&mut c, &mut tm_usec);
            continue;
        }
        if r == 0 {
            // the hour may be missing around a time zone change: again from the normalized time
            continue;
        }

        let mut v = c.min;
        let mut r = matching(clock, &spec.minute, false, &c, &mut v);
        c.min = v;
        if r > 0 {
            c.sec = 0;
            tm_usec = 0;
        }
        if r >= 0 {
            r = within_bounds(clock, &mut c);
        }
        if r < 0 {
            c.hour += 1;
            c.min = 0;
            c.sec = 0;
            tm_usec = 0;
            continue;
        }
        if r == 0 {
            continue;
        }

        let mut v = c.sec * i64::from(USEC_PER_SEC) + tm_usec;
        let mut r = matching(clock, &spec.second, false, &c, &mut v);
        tm_usec = v % i64::from(USEC_PER_SEC);
        c.sec = v / i64::from(USEC_PER_SEC);
        if r >= 0 {
            r = within_bounds(clock, &mut c);
        }
        if r < 0 {
            c.min += 1;
            c.sec = 0;
            tm_usec = 0;
            continue;
        }
        if r == 0 {
            continue;
        }

        let fields = |t: &Tm| (t.year, t.mon, t.mday, t.hour, t.min, t.sec);
        let mut order = fields(tm).cmp(&fields(&c));
        if order.is_eq() {
            order = (*usec).cmp(&(tm_usec + 1));
        }
        if order.is_ge() {
            // stuck: advance and let mktime() find the DST side
            invalidate_dst = true;
            c.hour += 1;
            continue;
        }

        *tm = c;
        *usec = tm_usec;
        return Ok(());
    }
    // systemd: "Infinite loop in calendar calculation"
    Err(Error::Invalid)
}

impl CalendarSpec {
    fn zone<'a>(&self, tz: &'a TimeZone, own: &'a mut Option<TimeZone>) -> Option<&'a TimeZone> {
        if self.utc {
            *own = Some(TimeZone::UTC);
        } else if let Some(name) = &self.timezone {
            *own = Some(TimeZone::get(name).ok()?);
        } else {
            return Some(tz);
        }
        own.as_ref()
    }

    /// One `calendar_spec_next_usec()`; `guess` is glibc's remembered `mktime()` offset,
    /// carried from one call to the next as in one process.
    fn next_with(&self, tz: &TimeZone, after: Timestamp, guess: &mut Offset) -> Option<Timestamp> {
        let us = after.as_microsecond().checked_add(1)?;
        let mut tm_usec = us.rem_euclid(1_000_000);
        let base = Timestamp::from_second(us.div_euclid(1_000_000)).ok()?;
        let mut clock = Clock { tz, guess: *guess };
        let mut tm = clock.localtime(base);
        let found = find_next(self, &mut clock, &mut tm, &mut tm_usec);
        let t = found.ok().and_then(|_| clock.mktime(&mut tm));
        *guess = clock.guess;
        Timestamp::from_microsecond(t?.as_microsecond().checked_add(tm_usec)?).ok()
    }

    /// The first elapse strictly after `after`, in `tz` (or the expression's own zone);
    /// `None` when it never elapses again or systemd would give up on it.
    ///
    /// glibc remembers the offset of its last `mktime()` and uses it to choose between the
    /// two instants of a repeated local time when nothing else decides (a fall-back hour
    /// whose two offsets have the same DST flag, rare). Like a fresh `systemd-analyze`, the
    /// computation starts from offset 0; in PID 1 it is whatever the last timer left.
    pub fn next_after(&self, tz: &TimeZone, after: Timestamp) -> Option<Timestamp> {
        let mut own = None;
        let tz = self.zone(tz, &mut own)?;
        let mut guess = Offset::UTC;
        self.next_with(tz, after, &mut guess)
    }

    /// The next `n` elapses after `from` (fewer when it stops elapsing), each from the one
    /// before, as `systemd-analyze calendar --iterations` computes them.
    pub fn next_runs(&self, tz: &TimeZone, from: Timestamp, n: usize) -> Vec<Timestamp> {
        let mut own = None;
        let Some(tz) = self.zone(tz, &mut own) else { return Vec::new() };
        let mut guess = Offset::UTC;
        let mut out = Vec::with_capacity(n);
        let mut at = from;
        while out.len() < n {
            let Some(t) = self.next_with(tz, at, &mut guess) else { break };
            out.push(t);
            at = t;
        }
        out
    }
}

/// Parses `expr` (zone suffixes checked against the time zone database) and gives its next
/// `n` elapses after `from` in `tz`, as systemd's timers would.
pub fn next_runs(expr: &str, tz: &TimeZone, from: Timestamp, n: usize) -> Result<Vec<Timestamp>, Error> {
    let spec = CalendarSpec::parse_with(expr, |z| TimeZone::get(z).is_ok())?;
    Ok(spec.next_runs(tz, from, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz(name: &str) -> TimeZone {
        TimeZone::get(name).unwrap()
    }

    fn at(s: &str, z: &TimeZone) -> Timestamp {
        s.parse::<DateTime>().unwrap().to_zoned(z.clone()).unwrap().timestamp()
    }

    fn local(v: &[Timestamp], z: &TimeZone) -> Vec<String> {
        v.iter().map(|t| t.to_zoned(z.clone()).strftime("%a %d %H:%M %Z").to_string()).collect()
    }

    fn runs(expr: &str, z: &TimeZone, from: Timestamp, n: usize) -> Vec<String> {
        local(&next_runs(expr, z, from, n).unwrap(), z)
    }

    #[test]
    fn spring_forward_skips_the_missing_time() {
        let z = tz("Europe/Rome");
        let sat = at("2026-03-28T12:00", &z);
        assert_eq!(runs("*-*-* 02:30", &z, sat, 2), ["Mon 30 02:30 CEST", "Tue 31 02:30 CEST"]);
        assert_eq!(runs("Sun *-*-* 02:30", &z, sat, 1), ["Sun 05 02:30 CEST"]);
        // later times of the day still happen
        assert_eq!(runs("*-*-* 02,03:30", &z, sat, 1), ["Sun 29 03:30 CEST"]);
        assert_eq!(runs("*-*-* *:00/20", &z, at("2026-03-29T01:30", &z), 2), ["Sun 29 01:40 CET", "Sun 29 03:00 CEST"]);
        // a half-hour change (Lord Howe Island, 02:00 → 02:30)
        let lh = tz("Australia/Lord_Howe");
        assert_eq!(runs("*-*-* 02:15", &lh, at("2026-10-03T12:00", &lh), 1), ["Mon 05 02:15 +11"]);
    }

    #[test]
    fn fall_back_runs_once() {
        let z = tz("Europe/Rome");
        let sat = at("2026-10-24T12:00", &z);
        assert_eq!(runs("*-*-* 02:30", &z, sat, 2), ["Sun 25 02:30 CEST", "Mon 26 02:30 CET"]);
        assert_eq!(
            runs("hourly", &z, at("2026-10-25T00:30", &z), 4),
            ["Sun 25 01:00 CEST", "Sun 25 02:00 CEST", "Sun 25 03:00 CET", "Sun 25 04:00 CET"]
        );
        // from inside the repeated hour (a timer started there without Persistent=)
        let second = at("2026-10-25T01:10", &TimeZone::UTC);
        assert_eq!(runs("*-*-* 02:30", &z, second, 1), ["Sun 25 02:30 CET"]);
        // Persistent=: the base is the last trigger, the first 02:30
        let first = next_runs("*-*-* 02:30", &z, sat, 1).unwrap()[0];
        assert_eq!(runs("*-*-* 02:30", &z, first, 1), ["Mon 26 02:30 CET"]);
    }

    #[test]
    fn persistent_catch_up_after_the_gap() {
        // last trigger Saturday 02:30, NAS off until Sunday 10:00: the next elapse from the
        // last trigger is Monday, so Sunday's boot has nothing to catch up
        let z = tz("Europe/Rome");
        let next = next_runs("*-*-* 02:30", &z, at("2026-03-28T02:30", &z), 1).unwrap()[0];
        assert!(next > at("2026-03-29T10:00", &z));
    }

    #[test]
    fn zone_suffix_and_utc() {
        let z = tz("Europe/Rome");
        let from = at("2026-11-16T12:00", &z);
        assert_eq!(runs("*-*-* 12:00 UTC", &z, from, 1), ["Mon 16 13:00 CET"]);
        assert_eq!(runs("*-*-* 12:00 Asia/Tokyo", &z, from, 1), ["Tue 17 04:00 CET"]);
        assert_eq!(next_runs("*-*-* 12:00 Not/AZone", &z, from, 1), Err(Error::Invalid));
        assert_eq!(runs("2025-01-01", &z, from, 1), Vec::<String>::new(), "never again");
    }
}
