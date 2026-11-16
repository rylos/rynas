//! systemd calendar expressions (`OnCalendar=`): parse, validate and normalize them the way
//! systemd 257 does (`src/shared/calendarspec.c`), so the normalized text is the same as the
//! "Normalized form" of `systemd-analyze calendar` (golden file `tests/golden/normalize.txt`).
//!
//! Not supported (an [`Error::Unsupported`]): a time zone *abbreviation* as suffix (`CET`,
//! `EST`): systemd reads it against the local zone's names, which a pure parser cannot know.
//! IANA names (`Europe/Rome`) and `UTC` are supported.

use std::fmt;

pub const USEC_PER_SEC: i32 = 1_000_000;
pub const MIN_YEAR: i32 = 1970;
pub const MAX_YEAR: i32 = 2199;
/// All seven weekday bits (Monday = bit 0).
pub const ALL_WEEKDAYS: u8 = 0x7f;
const COMPONENTS_MAX: usize = 240;

/// Why an expression is not accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// systemd refuses it too.
    Invalid,
    /// systemd may accept it, this parser does not (see the module documentation).
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid => f.write_str("invalid calendar expression"),
            Error::Unsupported(what) => write!(f, "calendar expression not supported here: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// One element of a chain: `start`, `start..stop`, `start/repeat`, `start..stop/repeat`.
/// `stop` is -1 when absent, `repeat` 0. Seconds are in microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Component {
    pub start: i32,
    pub stop: i32,
    pub repeat: i32,
}

impl Component {
    pub const fn value(v: i32) -> Self {
        Component { start: v, stop: -1, repeat: 0 }
    }
    pub const fn range(start: i32, stop: i32) -> Self {
        Component { start, stop, repeat: 1 }
    }
    pub const fn repeat(start: i32, repeat: i32) -> Self {
        Component { start, stop: -1, repeat }
    }
}

/// A parsed calendar expression. An empty chain is `*` (the seconds chain of `*` is
/// `0/1s`, as in systemd).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CalendarSpec {
    /// Bits Monday = 0 … Sunday = 6; `None` = every day.
    pub weekdays: Option<u8>,
    pub year: Vec<Component>,
    pub month: Vec<Component>,
    pub day: Vec<Component>,
    pub hour: Vec<Component>,
    pub minute: Vec<Component>,
    /// Microseconds within the minute.
    pub second: Vec<Component>,
    /// The day chain counts from the end of the month (`~`).
    pub end_of_month: bool,
    pub utc: bool,
    /// IANA time zone suffix.
    pub timezone: Option<String>,
}

const DAY_NAMES: [(&str, u8); 14] = [
    ("Monday", 0),
    ("Mon", 0),
    ("Tuesday", 1),
    ("Tue", 1),
    ("Wednesday", 2),
    ("Wed", 2),
    ("Thursday", 3),
    ("Thu", 3),
    ("Friday", 4),
    ("Fri", 4),
    ("Saturday", 5),
    ("Sat", 5),
    ("Sunday", 6),
    ("Sun", 6),
];

/// Short English weekday names, Monday first (as systemd writes them).
pub const WEEKDAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// A plausible IANA zone name (`Area/Location`), used when no time zone database is at hand.
pub fn plausible_zone(s: &str) -> bool {
    !s.is_empty()
        && s.contains('/')
        && s.split('/').all(|p| {
            p.as_bytes().first().is_some_and(|b| b.is_ascii_alphabetic())
                && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"_+-".contains(&b))
        })
}

fn ends_with_no_case<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let n = s.len().checked_sub(suffix.len())?;
    (s.is_char_boundary(n) && s[n..].eq_ignore_ascii_case(suffix)).then(|| &s[..n])
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn at(&self, k: usize) -> u8 {
        self.s.get(self.i + k).copied().unwrap_or(0)
    }
    fn cur(&self) -> u8 {
        self.at(0)
    }
    fn skip_spaces(&mut self) {
        while self.cur() == b' ' {
            self.i += 1;
        }
    }
}

type R<T> = Result<T, Error>;

fn parse_weekdays(p: &mut P, c: &mut CalendarSpec) -> R<()> {
    let mut bits: u8 = 0;
    let mut l: i32 = -1;
    let mut first = true;
    loop {
        let rest = &p.s[p.i..];
        let hit = DAY_NAMES.iter().find(|(name, _)| {
            rest.len() >= name.len() && rest[..name.len()].eq_ignore_ascii_case(name.as_bytes())
        });
        let Some(&(name, nr)) = hit else {
            if first {
                return Ok(());
            }
            return Err(Error::Invalid);
        };
        let skip = name.len();
        if !matches!(p.at(skip), 0 | b'-' | b'.' | b',' | b' ') {
            return Err(Error::Invalid);
        }
        bits |= 1 << nr;
        if l >= 0 {
            if l > i32::from(nr) {
                return Err(Error::Invalid);
            }
            for j in (l + 1)..i32::from(nr) {
                bits |= 1 << j;
            }
        }
        p.i += skip;
        c.weekdays = Some(c.weekdays.unwrap_or(0) | bits);
        match p.cur() {
            0 => return Ok(()),
            b' ' => {
                p.skip_spaces();
                return Ok(());
            }
            b'.' => {
                if l >= 0 || p.at(1) != b'.' {
                    return Err(Error::Invalid);
                }
                l = i32::from(nr);
                p.i += 2;
            }
            b'-' => {
                if l >= 0 {
                    return Err(Error::Invalid);
                }
                l = i32::from(nr);
                p.i += 1;
            }
            b',' => {
                l = -1;
                p.i += 1;
            }
            _ => {}
        }
        if matches!(p.cur(), 0 | b' ') {
            p.skip_spaces();
            return if l < 0 { Ok(()) } else { Err(Error::Invalid) };
        }
        first = false;
    }
}

/// strtoul on a run of digits (the caller checked the first one).
fn parse_number(p: &mut P) -> R<u64> {
    let start = p.i;
    let mut v: u64 = 0;
    while p.cur().is_ascii_digit() {
        v = v.checked_mul(10).and_then(|v| v.checked_add(u64::from(p.cur() - b'0'))).ok_or(Error::Invalid)?;
        p.i += 1;
    }
    if p.i == start {
        return Err(Error::Invalid);
    }
    Ok(v)
}

fn parse_decimal(p: &mut P, usec: bool) -> R<i32> {
    if !p.cur().is_ascii_digit() {
        return Err(Error::Invalid);
    }
    let save = p.i;
    let mut v = parse_number(p)?;
    if usec {
        v = v.checked_mul(USEC_PER_SEC as u64).ok_or(Error::Invalid)?;
        // one "." is a decimal point, ".." a range
        if p.cur() == b'.' && p.at(1) != b'.' {
            p.i += 1;
            // parse_fractional_part_u(6): digits, the first 6 count, the rest are dropped
            let mut add: u64 = 0;
            let mut n = 0;
            if !p.cur().is_ascii_digit() {
                p.i = save;
                return Err(Error::Invalid);
            }
            while p.cur().is_ascii_digit() {
                if n < 6 {
                    add = add * 10 + u64::from(p.cur() - b'0');
                    n += 1;
                }
                p.i += 1;
            }
            while n < 6 {
                add *= 10;
                n += 1;
            }
            v += add;
        }
    }
    i32::try_from(v).map_err(|_| Error::Invalid)
}

fn parse_components(p: &mut P, usec: bool, out: &mut Vec<Component>) -> R<()> {
    loop {
        if out.len() > COMPONENTS_MAX {
            return Err(Error::Invalid);
        }
        let start = parse_decimal(p, usec)?;
        let mut stop = -1;
        let mut repeat = 0;
        if p.cur() == b'.' && p.at(1) == b'.' {
            p.i += 2;
            stop = parse_decimal(p, usec)?;
            repeat = if usec { USEC_PER_SEC } else { 1 };
        }
        if p.cur() == b'/' {
            p.i += 1;
            repeat = parse_decimal(p, usec)?;
            if repeat == 0 {
                return Err(Error::Invalid);
            }
        } else {
            if start > i32::MAX - repeat {
                return Err(Error::Invalid);
            }
            if usec && stop >= 0 && start + repeat > stop {
                return Err(Error::Invalid);
            }
        }
        if !matches!(p.cur(), 0 | b' ' | b',' | b'-' | b'~' | b':') {
            return Err(Error::Invalid);
        }
        out.push(Component { start, stop, repeat });
        if p.cur() == b',' {
            p.i += 1;
            continue;
        }
        return Ok(());
    }
}

fn parse_chain(p: &mut P, usec: bool) -> R<Vec<Component>> {
    if p.cur() == b'*' {
        p.i += 1;
        return Ok(if usec { vec![Component::repeat(0, USEC_PER_SEC)] } else { Vec::new() });
    }
    let mut v = Vec::new();
    parse_components(p, usec, &mut v)?;
    Ok(v)
}

/// Ok(true): `@timestamp`, nothing may follow.
fn parse_date(p: &mut P, c: &mut CalendarSpec) -> R<bool> {
    if p.cur() == 0 {
        return Ok(false);
    }
    if p.cur() == b'@' {
        p.i += 1;
        if !p.cur().is_ascii_digit() {
            return Err(Error::Invalid);
        }
        let v = parse_number(p)?;
        let v = i64::try_from(v).map_err(|_| Error::Invalid)?;
        from_timestamp(c, v)?;
        return Ok(true);
    }
    let mut t = P { s: p.s, i: p.i };
    let first = parse_chain(&mut t, false)?;
    if matches!(t.cur(), 0 | b':') {
        // a time, not a date
        return Ok(false);
    }
    match t.cur() {
        b'~' => c.end_of_month = true,
        b'-' => {}
        _ => return Err(Error::Invalid),
    }
    t.i += 1;
    let second = parse_chain(&mut t, false)?;
    if matches!(t.cur(), 0 | b' ') {
        t.skip_spaces();
        p.i = t.i;
        c.month = first;
        c.day = second;
        return Ok(false);
    } else if c.end_of_month {
        return Err(Error::Invalid);
    }
    match t.cur() {
        b'~' => c.end_of_month = true,
        b'-' => {}
        _ => return Err(Error::Invalid),
    }
    t.i += 1;
    let third = parse_chain(&mut t, false)?;
    if !matches!(t.cur(), 0 | b' ') {
        return Err(Error::Invalid);
    }
    t.skip_spaces();
    p.i = t.i;
    c.year = first;
    c.month = second;
    c.day = third;
    Ok(false)
}

fn parse_time(p: &mut P, c: &mut CalendarSpec) -> R<()> {
    if p.cur() == 0 {
        c.hour = vec![Component::value(0)];
        c.minute = vec![Component::value(0)];
        c.second = vec![Component::value(0)];
        return Ok(());
    }
    let h = parse_chain(p, false)?;
    if p.cur() != b':' {
        return Err(Error::Invalid);
    }
    p.i += 1;
    let m = parse_chain(p, false)?;
    let s = if p.cur() == 0 {
        vec![Component::value(0)]
    } else {
        if p.cur() != b':' {
            return Err(Error::Invalid);
        }
        p.i += 1;
        let s = parse_chain(p, true)?;
        if p.cur() != 0 {
            return Err(Error::Invalid);
        }
        s
    };
    c.hour = h;
    c.minute = m;
    c.second = s;
    Ok(())
}

fn from_timestamp(c: &mut CalendarSpec, secs: i64) -> R<()> {
    // civil from days (Howard Hinnant's algorithm), UTC
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let year = i32::try_from(y).map_err(|_| Error::Invalid)?;
    c.utc = true;
    c.year = vec![Component::value(year)];
    c.month = vec![Component::value(m as i32)];
    c.day = vec![Component::value(d as i32)];
    c.hour = vec![Component::value((rem / 3600) as i32)];
    c.minute = vec![Component::value((rem % 3600 / 60) as i32)];
    c.second = vec![Component::value((rem % 60) as i32 * USEC_PER_SEC)];
    Ok(())
}

fn normalize_chain(c: &mut Vec<Component>) {
    for i in c.iter_mut() {
        if i.stop > i.start && i.repeat > 0 {
            i.stop -= (i.stop - i.start) % i.repeat;
        }
        if (i.stop > i.start && i.repeat > 0 && i.start + i.repeat > i.stop) || i.start == i.stop {
            i.repeat = 0;
            i.stop = -1;
        }
    }
    c.sort();
    c.dedup();
}

fn fix_year(c: &mut [Component]) {
    for i in c {
        for v in [&mut i.start, &mut i.stop] {
            if (0..70).contains(v) {
                *v += 2000;
            } else if (70..100).contains(v) {
                *v += 1900;
            }
        }
    }
}

fn chain_valid(c: &[Component], from: i32, to: i32, end_of_month: bool) -> bool {
    let to = if end_of_month { to - 3 } else { to };
    c.iter().all(|c| {
        if c.start < from || c.start > to || c.repeat > to - from {
            return false;
        }
        if c.stop >= 0 {
            c.stop >= from && c.stop <= to && c.start + c.repeat <= c.stop
        } else if end_of_month {
            c.start - c.repeat >= from
        } else {
            c.start + c.repeat <= to
        }
    })
}

impl CalendarSpec {
    /// Parses with [`plausible_zone`] for a time zone suffix (no time zone database).
    pub fn parse(expr: &str) -> Result<CalendarSpec, Error> {
        Self::parse_with(expr, plausible_zone)
    }

    /// Parses; `is_zone` says whether the last word is a valid IANA time zone.
    pub fn parse_with(expr: &str, is_zone: impl Fn(&str) -> bool) -> Result<CalendarSpec, Error> {
        let mut c = CalendarSpec::default();
        let mut text: &str = expr;
        if let Some(rest) = ends_with_no_case(text, " UTC") {
            c.utc = true;
            text = rest;
        } else if let Some(sp) = text.rfind(' ') {
            let last = &text[sp + 1..];
            if is_zone(last) {
                c.timezone = Some(last.to_string());
                text = &text[..sp];
            } else if last.len() >= 3
                && last.len() <= 6
                && last.bytes().all(|b| b.is_ascii_uppercase())
                && sp > 0
            {
                return Err(Error::Unsupported("time zone abbreviation"));
            }
        }
        if text.is_empty() {
            return Err(Error::Invalid);
        }
        let s = |v: &[i32]| v.iter().map(|&x| Component::value(x)).collect::<Vec<_>>();
        let lower = text.to_ascii_lowercase();
        match lower.as_str() {
            "minutely" => c.second = s(&[0]),
            "hourly" => {
                c.minute = s(&[0]);
                c.second = s(&[0]);
            }
            "daily" => {
                (c.hour, c.minute, c.second) = (s(&[0]), s(&[0]), s(&[0]));
            }
            "monthly" => {
                c.day = s(&[1]);
                (c.hour, c.minute, c.second) = (s(&[0]), s(&[0]), s(&[0]));
            }
            "annually" | "yearly" | "anually" => {
                c.month = s(&[1]);
                c.day = s(&[1]);
                (c.hour, c.minute, c.second) = (s(&[0]), s(&[0]), s(&[0]));
            }
            "weekly" => {
                c.weekdays = Some(1);
                (c.hour, c.minute, c.second) = (s(&[0]), s(&[0]), s(&[0]));
            }
            "quarterly" => {
                c.month = s(&[1, 4, 7, 10]);
                c.day = s(&[1]);
                (c.hour, c.minute, c.second) = (s(&[0]), s(&[0]), s(&[0]));
            }
            "biannually" | "bi-annually" | "semiannually" | "semi-annually" => {
                c.month = s(&[1, 7]);
                c.day = s(&[1]);
                (c.hour, c.minute, c.second) = (s(&[0]), s(&[0]), s(&[0]));
            }
            _ => {
                let mut p = P { s: text.as_bytes(), i: 0 };
                parse_weekdays(&mut p, &mut c)?;
                if !parse_date(&mut p, &mut c)? {
                    parse_time(&mut p, &mut c)?;
                }
                if p.cur() != 0 {
                    return Err(Error::Invalid);
                }
            }
        }
        c.normalize();
        if !c.valid() {
            return Err(Error::Invalid);
        }
        Ok(c)
    }

    fn normalize(&mut self) {
        if self.timezone.as_deref() == Some("UTC") {
            self.utc = true;
            self.timezone = None;
        }
        if matches!(self.weekdays, Some(b) if b == 0 || b >= ALL_WEEKDAYS) {
            self.weekdays = None;
        }
        if self.end_of_month && self.day.is_empty() {
            self.end_of_month = false;
        }
        fix_year(&mut self.year);
        for c in [&mut self.year, &mut self.month, &mut self.day, &mut self.hour, &mut self.minute, &mut self.second] {
            normalize_chain(c);
        }
    }

    fn valid(&self) -> bool {
        chain_valid(&self.year, MIN_YEAR, MAX_YEAR, false)
            && chain_valid(&self.month, 1, 12, false)
            && chain_valid(&self.day, 1, 31, self.end_of_month)
            && chain_valid(&self.hour, 0, 23, false)
            && chain_valid(&self.minute, 0, 59, false)
            && chain_valid(&self.second, 0, 60 * USEC_PER_SEC - 1, false)
    }

    /// The normalized text (systemd's "Normalized form").
    pub fn normalized(&self) -> String {
        self.to_string()
    }
}

fn format_weekdays(f: &mut String, bits: u8) {
    let mut l: i32 = -1;
    let mut need_comma = false;
    let mut x: i32 = 0;
    while x < 7 {
        if bits & (1 << x) != 0 {
            if l < 0 {
                if need_comma {
                    f.push(',');
                } else {
                    need_comma = true;
                }
                f.push_str(WEEKDAY_NAMES[x as usize]);
                l = x;
            }
        } else if l >= 0 {
            if x > l + 1 {
                f.push_str(if x > l + 2 { ".." } else { "," });
                f.push_str(WEEKDAY_NAMES[x as usize - 1]);
            }
            l = -1;
        }
        x += 1;
    }
    if l >= 0 && x > l + 1 {
        f.push_str(if x > l + 2 { ".." } else { "," });
        f.push_str(WEEKDAY_NAMES[x as usize - 1]);
    }
}

fn format_chain(f: &mut String, space: usize, c: &[Component], usec: bool) {
    let d = if usec { USEC_PER_SEC } else { 1 };
    let star = c.is_empty() || (usec && c.iter().any(|c| c.start == 0 && c.stop < 0 && c.repeat == USEC_PER_SEC));
    if star {
        f.push('*');
        return;
    }
    for (k, c) in c.iter().enumerate() {
        if k > 0 {
            f.push(',');
        }
        f.push_str(&format!("{:0space$}", c.start / d));
        if c.start % d > 0 {
            f.push_str(&format!(".{:06}", c.start % d));
        }
        if c.stop > 0 {
            f.push_str(&format!("..{:0space$}", c.stop / d));
        }
        if c.stop > 0 && c.stop % d > 0 {
            f.push_str(&format!(".{:06}", c.stop % d));
        }
        if c.repeat > 0 && !(c.stop > 0 && c.repeat == d) {
            f.push_str(&format!("/{}", c.repeat / d));
        }
        if c.repeat % d > 0 {
            f.push_str(&format!(".{:06}", c.repeat % d));
        }
    }
}

impl fmt::Display for CalendarSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        if let Some(b) = self.weekdays
            && b > 0
            && b < ALL_WEEKDAYS
        {
            format_weekdays(&mut s, b);
            s.push(' ');
        }
        format_chain(&mut s, 4, &self.year, false);
        s.push('-');
        format_chain(&mut s, 2, &self.month, false);
        s.push(if self.end_of_month { '~' } else { '-' });
        format_chain(&mut s, 2, &self.day, false);
        s.push(' ');
        format_chain(&mut s, 2, &self.hour, false);
        s.push(':');
        format_chain(&mut s, 2, &self.minute, false);
        s.push(':');
        format_chain(&mut s, 2, &self.second, true);
        if self.utc {
            s.push_str(" UTC");
        } else if let Some(tz) = &self.timezone {
            s.push(' ');
            s.push_str(tz);
        }
        f.write_str(&s)
    }
}

/// The normalized form of `expr` (as `systemd-analyze calendar` prints it).
pub fn normalize(expr: &str) -> Result<String, Error> {
    CalendarSpec::parse(expr).map(|c| c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        assert_eq!(normalize("daily").unwrap(), "*-*-* 00:00:00");
        assert_eq!(normalize("Mon,Tue,Wed *-*-* 2:30").unwrap(), "Mon..Wed *-*-* 02:30:00");
        assert_eq!(normalize("*-*-* 08..18/3:00").unwrap(), "*-*-* 08..17/3:00:00");
        assert_eq!(normalize("Sun *-*~07/1 02:30").unwrap(), "Sun *-*~07/1 02:30:00");
        assert_eq!(normalize("*-*-* 02:30 utc").unwrap(), "*-*-* 02:30:00 UTC");
        assert_eq!(normalize("*-*-* 02:30 Europe/Rome").unwrap(), "*-*-* 02:30:00 Europe/Rome");
        assert_eq!(normalize("@0").unwrap(), "1970-01-01 00:00:00 UTC");
        assert_eq!(normalize("*-*-* 02:30 CET"), Err(Error::Unsupported("time zone abbreviation")));
        assert_eq!(normalize("*-*-* 24:00"), Err(Error::Invalid));
        assert_eq!(normalize(""), Err(Error::Invalid));
        assert!(plausible_zone("America/Argentina/Buenos_Aires"));
        assert!(!plausible_zone("02:30") && !plausible_zone("UTC") && !plausible_zone("a//b"));
    }
}
