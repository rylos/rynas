//! The preset schedules of the schedule editor: each one is exactly one `OnCalendar=`
//! expression (written in systemd's normalized form, so the text stored is the text systemd
//! shows), and [`Recurrence::from_calendar`] recognizes those expressions again. Anything
//! else stays an advanced expression.

use serde::{Deserialize, Serialize};

use crate::spec::{ALL_WEEKDAYS, CalendarSpec, Component, Error};

/// A wall-clock time of the day (local time of the NAS).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct TimeOfDay {
    pub hour: u8,
    pub minute: u8,
}

impl TimeOfDay {
    pub const fn new(hour: u8, minute: u8) -> Self {
        TimeOfDay { hour, minute }
    }

    /// "HH:MM" (also "H:M").
    pub fn parse(s: &str) -> Option<Self> {
        let (h, m) = s.split_once(':')?;
        let (hour, minute) = (h.parse().ok()?, m.parse().ok()?);
        (hour < 24 && minute < 60).then_some(TimeOfDay { hour, minute })
    }
}

impl std::fmt::Display for TimeOfDay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

/// A set of weekdays, bit 0 = Monday … bit 6 = Sunday. Empty and full both mean every day.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Weekdays(pub u8);

impl Weekdays {
    pub const EVERY_DAY: Weekdays = Weekdays(0);
    pub const MON_FRI: Weekdays = Weekdays(0x1f);
    pub const SAT_SUN: Weekdays = Weekdays(0x60);

    /// From day numbers 0 = Monday … 6 = Sunday (others ignored).
    pub fn of(days: &[u8]) -> Weekdays {
        Weekdays(days.iter().filter(|d| **d < 7).fold(0, |b, d| b | (1 << d))).canonical()
    }

    /// Every day is the empty set.
    pub fn canonical(self) -> Weekdays {
        let b = self.0 & ALL_WEEKDAYS;
        Weekdays(if b == ALL_WEEKDAYS { 0 } else { b })
    }

    pub fn every_day(self) -> bool {
        self.canonical().0 == 0
    }

    pub fn contains(self, day: u8) -> bool {
        self.every_day() || (day < 7 && self.0 & (1 << day) != 0)
    }

    /// The days in the set, 0 = Monday; all seven for every day.
    pub fn days(self) -> Vec<u8> {
        (0..7).filter(|d| self.contains(*d)).collect()
    }

    fn spec(self) -> Option<u8> {
        let b = self.canonical().0;
        (b != 0).then_some(b)
    }
}

/// Which weekday of the month.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Nth {
    First,
    Second,
    Third,
    Fourth,
    Last,
}

impl Nth {
    pub const ALL: [Nth; 5] = [Nth::First, Nth::Second, Nth::Third, Nth::Fourth, Nth::Last];

    fn first_day(self) -> Option<i32> {
        match self {
            Nth::First => Some(1),
            Nth::Second => Some(8),
            Nth::Third => Some(15),
            Nth::Fourth => Some(22),
            Nth::Last => None,
        }
    }
}

/// Days of the month of [`Recurrence::Monthly`].
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonthDay {
    /// These days (1-31; a month without the day skips it, as systemd does).
    Days(Vec<u8>),
    /// The last day of every month.
    Last,
}

/// A preset schedule.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Recurrence {
    /// At one or more times of the day, every day or on chosen weekdays. The times must be
    /// every combination of their hours and minutes (08:00 and 20:00; 08:15, 08:45, 20:15,
    /// 20:45), so they are one expression.
    Daily { days: Weekdays, times: Vec<TimeOfDay> },
    /// Every `every` minutes (a divisor of 60) through the hours `from` to `to` (0 and 23:
    /// all day), every day or on chosen weekdays.
    Minutes { every: u8, from: u8, to: u8, days: Weekdays },
    /// Every `every` hours at minute `minute`, from hour `from` to hour `to` (0 and 23: all
    /// day, starting at midnight), every day or on chosen weekdays.
    Hours { every: u8, minute: u8, from: u8, to: u8, days: Weekdays },
    /// On days of the month at the times.
    Monthly { day: MonthDay, times: Vec<TimeOfDay> },
    /// On the first … fourth or last `weekday` (0 = Monday) of the month at the times.
    MonthlyNth { nth: Nth, weekday: u8, times: Vec<TimeOfDay> },
    /// Every year on `month`-`day` at the times.
    Yearly { month: u8, day: u8, times: Vec<TimeOfDay> },
}

/// Why a [`Recurrence`] is not one valid expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    NoTimes,
    /// The times are not every combination of their hours and minutes.
    TimesNotAGrid,
    BadTime,
    /// `every` minutes does not divide an hour.
    BadMinutes,
    BadHours,
    BadDay,
    BadWeekday,
    BadMonth,
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Invalid::NoTimes => "choose at least one time",
            Invalid::TimesNotAGrid => "the times are not one expression (every hour with every minute)",
            Invalid::BadTime => "invalid time",
            Invalid::BadMinutes => "the minutes must divide an hour (1, 2, 3, 4, 5, 6, 10, 12, 15, 20 or 30)",
            Invalid::BadHours => "invalid hours",
            Invalid::BadDay => "invalid day",
            Invalid::BadWeekday => "invalid weekday",
            Invalid::BadMonth => "invalid month",
        })
    }
}

impl std::error::Error for Invalid {}

/// The minute steps a [`Recurrence::Minutes`] may use.
pub const MINUTE_STEPS: [u8; 11] = [1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30];

fn values(v: impl IntoIterator<Item = u8>) -> Vec<Component> {
    let mut c: Vec<Component> = v.into_iter().map(|x| Component::value(i32::from(x))).collect();
    c.sort();
    c.dedup();
    c
}

fn zero_seconds() -> Vec<Component> {
    vec![Component::value(0)]
}

/// Hours and minutes of a grid of times.
fn grid(times: &[TimeOfDay]) -> Result<(Vec<u8>, Vec<u8>), Invalid> {
    if times.is_empty() {
        return Err(Invalid::NoTimes);
    }
    if times.iter().any(|t| t.hour > 23 || t.minute > 59) {
        return Err(Invalid::BadTime);
    }
    let mut h: Vec<u8> = times.iter().map(|t| t.hour).collect();
    let mut m: Vec<u8> = times.iter().map(|t| t.minute).collect();
    h.sort_unstable();
    h.dedup();
    m.sort_unstable();
    m.dedup();
    let mut t = times.to_vec();
    t.sort_unstable();
    t.dedup();
    if t.len() != h.len() * m.len() {
        return Err(Invalid::TimesNotAGrid);
    }
    Ok((h, m))
}

fn days_in_month(month: u8) -> u8 {
    match month {
        2 => 29,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `from..to/every` hours, `None` = all day.
fn hour_range(every: u8, from: u8, to: u8) -> Result<Option<Component>, Invalid> {
    if every == 0 || every > 23 || from > 23 || to > 23 || from > to {
        return Err(Invalid::BadHours);
    }
    if from == 0 && to == 23 {
        return Ok(None);
    }
    if from == to || !(to - from).is_multiple_of(every) {
        return Err(Invalid::BadHours);
    }
    Ok(Some(Component { start: i32::from(from), stop: i32::from(to), repeat: i32::from(every) }))
}

impl Recurrence {
    /// The times of the day of the presets that have them.
    pub fn times(&self) -> &[TimeOfDay] {
        match self {
            Recurrence::Daily { times, .. }
            | Recurrence::Monthly { times, .. }
            | Recurrence::MonthlyNth { times, .. }
            | Recurrence::Yearly { times, .. } => times,
            Recurrence::Minutes { .. } | Recurrence::Hours { .. } => &[],
        }
    }

    /// The calendar expression, or why this is not one.
    pub fn to_spec(&self) -> Result<CalendarSpec, Invalid> {
        let mut c = CalendarSpec { second: zero_seconds(), ..CalendarSpec::default() };
        match self {
            Recurrence::Daily { days, times } => {
                let (h, m) = grid(times)?;
                c.weekdays = days.spec();
                c.hour = values(h);
                c.minute = values(m);
            }
            Recurrence::Minutes { every, from, to, days } => {
                if !MINUTE_STEPS.contains(every) {
                    return Err(Invalid::BadMinutes);
                }
                if from > to || *to > 23 {
                    return Err(Invalid::BadHours);
                }
                c.weekdays = days.spec();
                c.hour = match (from, to) {
                    (0, 23) => Vec::new(),
                    (f, t) if f == t => values([*f]),
                    (f, t) => vec![Component::range(i32::from(*f), i32::from(*t))],
                };
                c.minute = if *every == 1 { Vec::new() } else { vec![Component::repeat(0, i32::from(*every))] };
            }
            Recurrence::Hours { every, minute, from, to, days } => {
                if *minute > 59 {
                    return Err(Invalid::BadTime);
                }
                c.weekdays = days.spec();
                c.hour = match hour_range(*every, *from, *to)? {
                    None if *every == 1 => Vec::new(),
                    None => vec![Component::repeat(0, i32::from(*every))],
                    Some(r) if *every == 1 => vec![Component::range(r.start, r.stop)],
                    Some(r) => vec![r],
                };
                c.minute = values([*minute]);
            }
            Recurrence::Monthly { day, times } => {
                let (h, m) = grid(times)?;
                match day {
                    MonthDay::Days(d) => {
                        if d.is_empty() || d.iter().any(|d| !(1..=31).contains(d)) {
                            return Err(Invalid::BadDay);
                        }
                        c.day = values(d.iter().copied());
                    }
                    MonthDay::Last => {
                        c.end_of_month = true;
                        c.day = values([1]);
                    }
                }
                c.hour = values(h);
                c.minute = values(m);
            }
            Recurrence::MonthlyNth { nth, weekday, times } => {
                let (h, m) = grid(times)?;
                if *weekday > 6 {
                    return Err(Invalid::BadWeekday);
                }
                c.weekdays = Some(1 << weekday);
                match nth.first_day() {
                    Some(d) => c.day = vec![Component::range(d, d + 6)],
                    None => {
                        c.end_of_month = true;
                        c.day = vec![Component::repeat(7, 1)];
                    }
                }
                c.hour = values(h);
                c.minute = values(m);
            }
            Recurrence::Yearly { month, day, times } => {
                let (h, m) = grid(times)?;
                if !(1..=12).contains(month) {
                    return Err(Invalid::BadMonth);
                }
                if *day == 0 || *day > days_in_month(*month) {
                    return Err(Invalid::BadDay);
                }
                c.month = values([*month]);
                c.day = values([*day]);
                c.hour = values(h);
                c.minute = values(m);
            }
        }
        Ok(c)
    }

    /// The `OnCalendar=` text (systemd's normalized form).
    pub fn to_calendar(&self) -> Result<String, Invalid> {
        self.to_spec().map(|c| c.to_string())
    }

    /// The preset a parsed expression is, if any (in its canonical form: lists sorted,
    /// every day as an empty weekday set).
    pub fn from_spec(c: &CalendarSpec) -> Option<Recurrence> {
        if !c.year.is_empty() || c.utc || c.timezone.is_some() || c.second != zero_seconds() {
            return None;
        }
        let days = Weekdays(c.weekdays.unwrap_or(0));
        let singles = |chain: &[Component]| -> Option<Vec<u8>> {
            if chain.is_empty() {
                return None;
            }
            chain
                .iter()
                .map(|x| (x.stop < 0 && x.repeat == 0).then(|| u8::try_from(x.start).ok()).flatten())
                .collect()
        };
        let times = |c: &CalendarSpec| -> Option<Vec<TimeOfDay>> {
            let h = singles(&c.hour)?;
            let m = singles(&c.minute)?;
            Some(h.iter().flat_map(|h| m.iter().map(move |m| TimeOfDay::new(*h, *m))).collect())
        };
        let single = |chain: &[Component]| -> Option<u8> {
            match singles(chain)?.as_slice() {
                [v] => Some(*v),
                _ => None,
            }
        };
        // month, day and weekday
        if !c.month.is_empty() {
            if c.weekdays.is_some() || c.end_of_month {
                return None;
            }
            let month = single(&c.month)?;
            let day = single(&c.day)?;
            let r = Recurrence::Yearly { month, day, times: times(c)? };
            return r.to_spec().is_ok().then_some(r);
        }
        if !c.day.is_empty() {
            if let Some(w) = c.weekdays {
                if w.count_ones() != 1 {
                    return None;
                }
                let weekday = w.trailing_zeros() as u8;
                let nth = match (c.end_of_month, c.day.as_slice()) {
                    (true, [Component { start: 7, stop: -1, repeat: 1 }])
                    | (true, [Component { start: 1, stop: 7, repeat: 1 }]) => Nth::Last,
                    (false, [Component { start, stop, repeat: 1 }]) if *stop == start + 6 => {
                        *Nth::ALL.iter().find(|n| n.first_day() == Some(*start))?
                    }
                    _ => return None,
                };
                return Some(Recurrence::MonthlyNth { nth, weekday, times: times(c)? });
            }
            let day = if c.end_of_month {
                (c.day == values([1])).then_some(MonthDay::Last)?
            } else {
                MonthDay::Days(singles(&c.day)?)
            };
            return Some(Recurrence::Monthly { day, times: times(c)? });
        }
        if c.end_of_month {
            return None;
        }
        let days = days.canonical();
        // the time of day
        if let Some(t) = times(c) {
            return Some(Recurrence::Daily { days, times: t });
        }
        let hour_span = |chain: &[Component]| -> Option<(u8, u8, u8)> {
            match chain {
                [] => Some((1, 0, 23)),
                [Component { start: 0, stop: -1, repeat }] if *repeat > 0 => Some((u8::try_from(*repeat).ok()?, 0, 23)),
                [Component { start, stop, repeat }] if *stop > *start && *repeat > 0 => {
                    Some((u8::try_from(*repeat).ok()?, u8::try_from(*start).ok()?, u8::try_from(*stop).ok()?))
                }
                [Component { start, stop: -1, repeat: 0 }] => {
                    let h = u8::try_from(*start).ok()?;
                    Some((1, h, h))
                }
                _ => None,
            }
        };
        // every N minutes
        let minutes = match c.minute.as_slice() {
            [] => Some(1),
            [Component { start: 0, stop: -1, repeat }] => u8::try_from(*repeat).ok().filter(|r| MINUTE_STEPS.contains(r)),
            _ => None,
        };
        if let Some(every) = minutes {
            let (step, from, to) = hour_span(&c.hour)?;
            if step != 1 {
                return None;
            }
            return Some(Recurrence::Minutes { every, from, to, days });
        }
        // every N hours at a minute
        let minute = single(&c.minute)?;
        let (every, from, to) = hour_span(&c.hour)?;
        let r = Recurrence::Hours { every, minute, from, to, days };
        r.to_spec().is_ok().then_some(r)
    }

    /// The preset an expression is, if any.
    pub fn from_calendar(expr: &str) -> Option<Recurrence> {
        Self::from_spec(&CalendarSpec::parse(expr).ok()?)
    }
}

/// What an expression typed in the advanced field is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Parsed {
    /// One of the presets.
    Preset(Recurrence),
    /// Valid, but no preset shows it; the normalized expression.
    Advanced(String),
    Invalid(Error),
}

/// Classifies an expression of the advanced field.
pub fn classify(expr: &str) -> Parsed {
    match CalendarSpec::parse(expr) {
        Ok(c) => match Recurrence::from_spec(&c) {
            Some(r) => Parsed::Preset(r),
            None => Parsed::Advanced(c.to_string()),
        },
        Err(e) => Parsed::Invalid(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u8, m: u8) -> TimeOfDay {
        TimeOfDay::new(h, m)
    }

    fn rt(r: Recurrence, text: &str) {
        let s = r.to_calendar().unwrap_or_else(|e| panic!("{r:?}: {e}"));
        assert_eq!(s, text, "{r:?}");
        assert_eq!(crate::spec::normalize(&s).unwrap(), s, "normalized form");
        assert_eq!(Recurrence::from_calendar(&s), Some(r), "{s}");
    }

    #[test]
    fn presets_round_trip() {
        rt(Recurrence::Daily { days: Weekdays::EVERY_DAY, times: vec![t(2, 30)] }, "*-*-* 02:30:00");
        rt(Recurrence::Daily { days: Weekdays::EVERY_DAY, times: vec![t(8, 0), t(20, 0)] }, "*-*-* 08,20:00:00");
        rt(
            Recurrence::Daily { days: Weekdays::EVERY_DAY, times: vec![t(8, 15), t(8, 45), t(20, 15), t(20, 45)] },
            "*-*-* 08,20:15,45:00",
        );
        rt(Recurrence::Daily { days: Weekdays::MON_FRI, times: vec![t(7, 0)] }, "Mon..Fri *-*-* 07:00:00");
        rt(Recurrence::Daily { days: Weekdays::of(&[0, 2]), times: vec![t(23, 5)] }, "Mon,Wed *-*-* 23:05:00");
        rt(Recurrence::Daily { days: Weekdays::SAT_SUN, times: vec![t(9, 30)] }, "Sat,Sun *-*-* 09:30:00");
        rt(Recurrence::Minutes { every: 15, from: 0, to: 23, days: Weekdays::EVERY_DAY }, "*-*-* *:00/15:00");
        rt(Recurrence::Minutes { every: 1, from: 0, to: 23, days: Weekdays::EVERY_DAY }, "*-*-* *:*:00");
        rt(Recurrence::Minutes { every: 10, from: 8, to: 18, days: Weekdays::MON_FRI }, "Mon..Fri *-*-* 08..18:00/10:00");
        rt(Recurrence::Minutes { every: 30, from: 9, to: 9, days: Weekdays::EVERY_DAY }, "*-*-* 09:00/30:00");
        rt(Recurrence::Hours { every: 1, minute: 15, from: 0, to: 23, days: Weekdays::EVERY_DAY }, "*-*-* *:15:00");
        rt(Recurrence::Hours { every: 6, minute: 0, from: 0, to: 23, days: Weekdays::EVERY_DAY }, "*-*-* 00/6:00:00");
        rt(Recurrence::Hours { every: 3, minute: 30, from: 8, to: 20, days: Weekdays::EVERY_DAY }, "*-*-* 08..20/3:30:00");
        rt(Recurrence::Hours { every: 1, minute: 0, from: 8, to: 18, days: Weekdays::of(&[5]) }, "Sat *-*-* 08..18:00:00");
        rt(Recurrence::Monthly { day: MonthDay::Days(vec![1]), times: vec![t(4, 0)] }, "*-*-01 04:00:00");
        rt(Recurrence::Monthly { day: MonthDay::Days(vec![1, 15]), times: vec![t(4, 0)] }, "*-*-01,15 04:00:00");
        rt(Recurrence::Monthly { day: MonthDay::Last, times: vec![t(23, 0)] }, "*-*~01 23:00:00");
        rt(Recurrence::MonthlyNth { nth: Nth::First, weekday: 0, times: vec![t(2, 30)] }, "Mon *-*-01..07 02:30:00");
        rt(Recurrence::MonthlyNth { nth: Nth::Third, weekday: 6, times: vec![t(2, 30)] }, "Sun *-*-15..21 02:30:00");
        rt(Recurrence::MonthlyNth { nth: Nth::Last, weekday: 4, times: vec![t(18, 0)] }, "Fri *-*~07/1 18:00:00");
        rt(Recurrence::Yearly { month: 12, day: 25, times: vec![t(0, 0)] }, "*-12-25 00:00:00");
        rt(Recurrence::Yearly { month: 2, day: 29, times: vec![t(12, 0)] }, "*-02-29 12:00:00");
    }

    #[test]
    fn shortcuts_and_systemd_forms() {
        let daily = Recurrence::Daily { days: Weekdays::EVERY_DAY, times: vec![t(0, 0)] };
        assert_eq!(Recurrence::from_calendar("daily"), Some(daily));
        assert_eq!(
            Recurrence::from_calendar("weekly"),
            Some(Recurrence::Daily { days: Weekdays::of(&[0]), times: vec![t(0, 0)] })
        );
        assert_eq!(
            Recurrence::from_calendar("hourly"),
            Some(Recurrence::Hours { every: 1, minute: 0, from: 0, to: 23, days: Weekdays::EVERY_DAY })
        );
        assert_eq!(
            Recurrence::from_calendar("monthly"),
            Some(Recurrence::Monthly { day: MonthDay::Days(vec![1]), times: vec![t(0, 0)] })
        );
        assert_eq!(
            Recurrence::from_calendar("yearly"),
            Some(Recurrence::Yearly { month: 1, day: 1, times: vec![t(0, 0)] })
        );
        assert_eq!(
            Recurrence::from_calendar("Mon..Sun 2:30"),
            Some(Recurrence::Daily { days: Weekdays::EVERY_DAY, times: vec![t(2, 30)] })
        );
        // what the power schedule, the dedup window and the old builder write
        assert!(matches!(Recurrence::from_calendar("Mon,Wed *-*-* 23:05:00"), Some(Recurrence::Daily { .. })));
        assert!(matches!(Recurrence::from_calendar("Sun,Wed *-*-* 01,13:00:00"), Some(Recurrence::Daily { .. })));
        assert!(matches!(Recurrence::from_calendar("*-*-* *:00/15:00"), Some(Recurrence::Minutes { every: 15, .. })));
        assert_eq!(
            Recurrence::from_calendar("Sun *-*~01..07 02:30"),
            Some(Recurrence::MonthlyNth { nth: Nth::Last, weekday: 6, times: vec![t(2, 30)] })
        );
    }

    #[test]
    fn advanced_expressions() {
        for e in [
            "quarterly",
            "*-*-* *:00/25:00",
            "*-*-* 02:30:15",
            "*-*-* 02:30 UTC",
            "*-*-* 02:30 Europe/Rome",
            "2027-*-* 00:00",
            "*-*-1/2 00:00",
            "*-*-* 08..17/2:00/30",
            "Mon,Fri *-*-01 00:00",
            "*-*~03 00:00",
            "*-*-* 00,12:*",
        ] {
            assert!(matches!(classify(e), Parsed::Advanced(_)), "{e}: {:?}", classify(e));
        }
        assert_eq!(classify("*-*-* 25:00"), Parsed::Invalid(Error::Invalid));
        assert_eq!(classify("*-*-* 2:30"), Parsed::Preset(Recurrence::Daily { days: Weekdays(0), times: vec![t(2, 30)] }));
        assert_eq!(classify("*-*-* 02:30:15 "), Parsed::Invalid(Error::Invalid));
        assert_eq!(classify("*-*-1/2 0:0"), Parsed::Advanced("*-*-01/2 00:00:00".into()));
        assert_eq!(
            classify("Mon *-*-* 00/6:30"),
            Parsed::Preset(Recurrence::Hours { every: 6, minute: 30, from: 0, to: 23, days: Weekdays::of(&[0]) })
        );
    }

    #[test]
    fn invalid_presets() {
        let bad = |r: Recurrence| r.to_calendar().unwrap_err();
        assert_eq!(bad(Recurrence::Daily { days: Weekdays(0), times: vec![] }), Invalid::NoTimes);
        assert_eq!(bad(Recurrence::Daily { days: Weekdays(0), times: vec![t(8, 15), t(20, 45)] }), Invalid::TimesNotAGrid);
        assert_eq!(bad(Recurrence::Daily { days: Weekdays(0), times: vec![t(24, 0)] }), Invalid::BadTime);
        assert_eq!(bad(Recurrence::Minutes { every: 25, from: 0, to: 23, days: Weekdays(0) }), Invalid::BadMinutes);
        assert_eq!(bad(Recurrence::Hours { every: 3, minute: 0, from: 8, to: 18, days: Weekdays(0) }), Invalid::BadHours);
        assert_eq!(bad(Recurrence::Hours { every: 1, minute: 0, from: 8, to: 8, days: Weekdays(0) }), Invalid::BadHours);
        assert_eq!(bad(Recurrence::Monthly { day: MonthDay::Days(vec![32]), times: vec![t(0, 0)] }), Invalid::BadDay);
        assert_eq!(bad(Recurrence::MonthlyNth { nth: Nth::First, weekday: 7, times: vec![t(0, 0)] }), Invalid::BadWeekday);
        assert_eq!(bad(Recurrence::Yearly { month: 2, day: 30, times: vec![t(0, 0)] }), Invalid::BadDay);
        assert_eq!(bad(Recurrence::Yearly { month: 13, day: 1, times: vec![t(0, 0)] }), Invalid::BadMonth);
    }

    #[test]
    fn weekdays_and_times() {
        assert!(Weekdays(0x7f).every_day() && Weekdays(0).every_day());
        assert_eq!(Weekdays::of(&[0, 1, 2, 3, 4, 5, 6]), Weekdays(0));
        assert_eq!(Weekdays::MON_FRI.days(), vec![0, 1, 2, 3, 4]);
        assert!(Weekdays::SAT_SUN.contains(6) && !Weekdays::SAT_SUN.contains(0));
        assert_eq!(TimeOfDay::parse("7:05"), Some(t(7, 5)));
        assert_eq!(TimeOfDay::parse("24:00"), None);
        assert_eq!(t(7, 5).to_string(), "07:05");
    }

    #[test]
    fn json() {
        let r = Recurrence::Hours { every: 2, minute: 0, from: 0, to: 23, days: Weekdays::MON_FRI };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"kind":"hours","every":2,"minute":0,"from":0,"to":23,"days":31}"#);
        assert_eq!(serde_json::from_str::<Recurrence>(&json).unwrap(), r);
        let m = Recurrence::Monthly { day: MonthDay::Last, times: vec![t(1, 2)] };
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, r#"{"kind":"monthly","day":"last","times":[{"hour":1,"minute":2}]}"#);
        assert_eq!(serde_json::from_str::<Recurrence>(&json).unwrap(), m);
    }

}
