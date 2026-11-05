//! btrbk snapshot names: `<prefix>.<timestamp>[_N]`.
//!
//! Ported from btrbk-nas (`snapshot.rs`, MIT, same author), without chrono: the timestamp is
//! one of btrbk's `timestamp_format` flavours (`short` `YYYYMMDD`, `long` `YYYYMMDDTHHMM`,
//! `long-iso` `YYYYMMDDTHHMMSS±hhmm`), with the `_N` suffix btrbk adds when a name is taken.
//! Times without an offset are local time of the host that took them (rynas uses `long`,
//! the format SMB `shadow_copy2` reads with `shadow:localtime = yes`).

use std::cmp::Ordering;

/// A parsed btrbk timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// Seconds east of UTC when the name carries an offset (`long-iso`).
    pub offset: Option<i32>,
    /// btrbk's `_N` suffix (0 without one).
    pub suffix: u64,
}

impl Stamp {
    /// Milliseconds since the epoch: with the offset when there is one, else as local time.
    pub fn epoch_ms(&self) -> i64 {
        match self.offset {
            Some(off) => (self.civil_utc_s() - off as i64) * 1000,
            None => local_to_epoch_s(self) * 1000,
        }
    }

    /// The written time read as if it were UTC (ordering key, tests).
    fn civil_utc_s(&self) -> i64 {
        days_from_civil(self.year, self.month, self.day) * 86400
            + (self.hour * 3600 + self.minute * 60 + self.second) as i64
    }

    /// Sort key: UTC when an offset is known, else the written time.
    fn key(&self) -> (i64, u64) {
        (
            self.civil_utc_s() - self.offset.unwrap_or(0) as i64,
            self.suffix,
        )
    }
}

/// Splits `photos.20261105T1200` into `("photos", "20261105T1200")`. The prefix may contain
/// dots; the part after the last dot must be a btrbk timestamp.
pub fn split_snapshot_name(name: &str) -> Option<(&str, &str)> {
    let (prefix, ts) = name.rsplit_once('.')?;
    (!prefix.is_empty() && parse_timestamp(ts).is_some()).then_some((prefix, ts))
}

fn digits(s: &str) -> Option<u32> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

fn date(s: &str) -> Option<(i32, u32, u32)> {
    if s.len() != 8 {
        return None;
    }
    let (y, m, d) = (
        digits(&s[..4])? as i32,
        digits(&s[4..6])?,
        digits(&s[6..8])?,
    );
    ((1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m)).then_some((y, m, d))
}

fn time(s: &str) -> Option<(u32, u32, u32)> {
    let (h, mi, sec) = match s.len() {
        4 => (digits(&s[..2])?, digits(&s[2..4])?, 0),
        6 => (digits(&s[..2])?, digits(&s[2..4])?, digits(&s[4..6])?),
        _ => return None,
    };
    (h < 24 && mi < 60 && sec < 60).then_some((h, mi, sec))
}

/// `Z`, `+0100`, `-0530` in seconds east of UTC.
fn parse_offset(text: &str) -> Option<i32> {
    if text == "Z" {
        return Some(0);
    }
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let d = &text[1..];
    if d.len() != 4 {
        return None;
    }
    let (h, m) = (digits(&d[..2])? as i32, digits(&d[2..])? as i32);
    (h <= 14 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

/// Parses a btrbk timestamp strictly.
pub fn parse_timestamp(ts: &str) -> Option<Stamp> {
    // legacy `YYYYMMDD_HHMMSS`: here the underscore is not a `_N` suffix
    if ts.len() == 15 && ts.as_bytes()[8] == b'_' {
        let (y, mo, d) = date(&ts[..8])?;
        let (h, mi, s) = time(&ts[9..])?;
        return Some(Stamp {
            year: y,
            month: mo,
            day: d,
            hour: h,
            minute: mi,
            second: s,
            offset: None,
            suffix: 0,
        });
    }
    let (stamp, suffix) = match ts.split_once('_') {
        Some((s, n)) => (s, digits(n).map(u64::from).or_else(|| n.parse().ok())?),
        None => (ts, 0),
    };
    let (base, offset) = match stamp.find(['+', '-', 'Z']) {
        Some(at) => (&stamp[..at], Some(parse_offset(&stamp[at..])?)),
        None => (stamp, None),
    };
    // an offset only follows the seconds of long-iso
    if offset.is_some() && base.len() != 15 {
        return None;
    }
    let (date_part, time_part) = match base.len() {
        8 => (base, None),
        13 | 15 if base.as_bytes()[8] == b'T' => (&base[..8], Some(&base[9..])),
        _ => return None,
    };
    let (y, mo, d) = date(date_part)?;
    let (h, mi, s) = match time_part {
        Some(t) => time(t)?,
        None => (0, 0, 0),
    };
    Some(Stamp {
        year: y,
        month: mo,
        day: d,
        hour: h,
        minute: mi,
        second: s,
        offset,
        suffix,
    })
}

/// Orders snapshot names oldest first: by time (UTC when an offset is known), then by the
/// `_N` suffix as a number; the name breaks ties.
pub fn compare_names(a: &str, b: &str) -> Ordering {
    fn key(name: &str) -> Option<(i64, u64)> {
        parse_timestamp(split_snapshot_name(name)?.1).map(|s| s.key())
    }
    key(a).cmp(&key(b)).then_with(|| a.cmp(b))
}

/// A valid snapshot name prefix (btrbk `snapshot_name`, job names).
pub fn valid_prefix(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 64
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@'))
        && !p.starts_with('.')
}

/// btrbk's `long` timestamp of `epoch_s` in local time: `YYYYMMDDTHHMM`.
pub fn long_timestamp(epoch_s: i64) -> String {
    let t = local_time(epoch_s);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}",
        t.year, t.month, t.day, t.hour, t.minute
    )
}

/// `YYYYMMDDTHHMMSS` in local time, for names rynas gives to renamed files.
pub fn stamp_seconds(epoch_s: i64) -> String {
    let t = local_time(epoch_s);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// `YYYY-MM-DD HHMM` in local time (restored copies).
pub fn human_minute(epoch_s: i64) -> String {
    let t = local_time(epoch_s);
    format!(
        "{:04}-{:02}-{:02} {:02}{:02}",
        t.year, t.month, t.day, t.hour, t.minute
    )
}

/// Civil date to days since 1970-01-01 (proleptic Gregorian; Howard Hinnant's algorithm).
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        _ => 28,
    }
}

fn local_time(epoch_s: i64) -> Stamp {
    // SAFETY: localtime_r only writes into the struct we pass
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t: libc::time_t = epoch_s as libc::time_t;
    let ok = !unsafe { libc::localtime_r(&t, &mut tm) }.is_null();
    if !ok {
        tm = unsafe { std::mem::zeroed() };
        tm.tm_year = 70;
        tm.tm_mday = 1;
    }
    Stamp {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
        offset: None,
        suffix: 0,
    }
}

fn local_to_epoch_s(s: &Stamp) -> i64 {
    // SAFETY: mktime reads and normalizes the struct we pass
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = s.year - 1900;
    tm.tm_mon = s.month as i32 - 1;
    tm.tm_mday = s.day as i32;
    tm.tm_hour = s.hour as i32;
    tm.tm_min = s.minute as i32;
    tm.tm_sec = s.second as i32;
    tm.tm_isdst = -1;
    let t = unsafe { libc::mktime(&mut tm) };
    if t == -1 { s.civil_utc_s() } else { t as i64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_names() {
        assert_eq!(
            split_snapshot_name("@home.20260803T0000"),
            Some(("@home", "20260803T0000"))
        );
        assert_eq!(
            split_snapshot_name("docs.20261105T0137_1"),
            Some(("docs", "20261105T0137_1"))
        );
        assert_eq!(
            split_snapshot_name("my.data.20250901"),
            Some(("my.data", "20250901"))
        );
        assert_eq!(
            split_snapshot_name("@.20260101T120000+0100_1"),
            Some(("@", "20260101T120000+0100_1"))
        );
        for bad in ["scripts.d", ".20250901", "test-acl", "docs.20261305T0000"] {
            assert_eq!(split_snapshot_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn parses_strictly() {
        let p = |t| parse_timestamp(t).map(|s| (s.year, s.month, s.day, s.hour, s.minute));
        assert_eq!(p("20260101"), Some((2026, 1, 1, 0, 0)));
        assert_eq!(p("20260101T1200"), Some((2026, 1, 1, 12, 0)));
        assert_eq!(p("20260101T120000+0100_2"), Some((2026, 1, 1, 12, 0)));
        assert_eq!(p("20260101_120000"), Some((2026, 1, 1, 12, 0)));
        assert_eq!(p("20260101_3"), Some((2026, 1, 1, 0, 0)));
        assert_eq!(parse_timestamp("20260101T1200_7").unwrap().suffix, 7);
        for bad in [
            "20260101T120000+01",
            "20260101T120000+0160",
            "20260101T120000+1500",
            "20260101+0100",
            "20260101T1200-0100",
            "20260101_",
            "20260101_x",
            "2026-01-01",
            "20260230",
            "20260101T2460",
        ] {
            assert_eq!(parse_timestamp(bad), None, "{bad}");
        }
    }

    #[test]
    fn epoch_with_offset() {
        let s = parse_timestamp("20260101T120000+0100").unwrap();
        // 2026-01-01 11:00:00 UTC
        assert_eq!(s.epoch_ms(), 1_767_265_200_000);
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    #[test]
    fn local_round_trip() {
        let now = 1_793_842_674;
        let name = format!("docs.{}", long_timestamp(now));
        let (_, ts) = split_snapshot_name(&name).unwrap();
        let back = parse_timestamp(ts).unwrap().epoch_ms() / 1000;
        assert_eq!(back, now - now % 60);
        assert_eq!(stamp_seconds(now).len(), 15);
    }

    #[test]
    fn sorts_by_time_then_suffix() {
        let mut names = vec![
            "@.20260101T120000+0100_10",
            "@.20260101T120000+0100_9",
            "@.20260101T120000+0100",
            "@.20260101T103000-0100",
            "@.20251231",
        ];
        names.sort_by(|a, b| compare_names(b, a));
        assert_eq!(
            names,
            [
                "@.20260101T103000-0100",
                "@.20260101T120000+0100_10",
                "@.20260101T120000+0100_9",
                "@.20260101T120000+0100",
                "@.20251231",
            ]
        );
    }

    #[test]
    fn prefixes() {
        assert!(valid_prefix("docs") && valid_prefix("@rootfs") && valid_prefix("a.b-c_d"));
        assert!(!valid_prefix("") && !valid_prefix(".x") && !valid_prefix("a/b"));
    }
}
