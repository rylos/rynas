//! The state of the schedule editor and its translation to one `OnCalendar=` expression,
//! without the browser (tested natively). Every preset is a [`Recurrence`] of
//! `rynas-schedule`, so what the editor writes is exactly what systemd runs and what the
//! editor recognizes again when the task is opened.

use rynas_api_types::scheduler::CalendarSyntax;
use rynas_schedule::recurrence::{Invalid, MINUTE_STEPS};
use rynas_schedule::{MonthDay, Nth, Parsed, Recurrence, TimeOfDay, Weekdays, classify};

/// The presets of the editor, in the order of its list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preset {
    Minutes,
    Hours,
    Daily,
    Weekly,
    Monthly,
    MonthlyNth,
    Yearly,
    /// Any systemd calendar expression.
    Advanced,
    /// A crontab line, converted by the NAS.
    Cron,
}

impl Preset {
    /// (value, i18n key) for the select.
    pub const OPTIONS: &'static [(&'static str, &'static str)] = &[
        ("minutes", "sch.rep.minutes"),
        ("hours", "sch.rep.hours"),
        ("daily", "sch.rep.daily"),
        ("weekly", "sch.rep.weekdays"),
        ("monthly", "sch.rep.monthly"),
        ("monthly_nth", "sch.rep.monthly_nth"),
        ("yearly", "sch.rep.yearly"),
        ("advanced", "sch.rep.custom"),
        ("cron", "sch.rep.cron"),
    ];

    pub fn code(self) -> &'static str {
        match self {
            Preset::Minutes => "minutes",
            Preset::Hours => "hours",
            Preset::Daily => "daily",
            Preset::Weekly => "weekly",
            Preset::Monthly => "monthly",
            Preset::MonthlyNth => "monthly_nth",
            Preset::Yearly => "yearly",
            Preset::Advanced => "advanced",
            Preset::Cron => "cron",
        }
    }

    pub fn parse(s: &str) -> Preset {
        match s {
            "minutes" => Preset::Minutes,
            "hours" => Preset::Hours,
            "weekly" => Preset::Weekly,
            "monthly" => Preset::Monthly,
            "monthly_nth" => Preset::MonthlyNth,
            "yearly" => Preset::Yearly,
            "advanced" => Preset::Advanced,
            "cron" => Preset::Cron,
            _ => Preset::Daily,
        }
    }
}

/// What the editor holds: the fields of every preset at once, so that switching presets
/// keeps what was typed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Draft {
    pub preset: Preset,
    /// [`Preset::Minutes`]: one of [`MINUTE_STEPS`].
    pub every_min: u8,
    /// [`Preset::Hours`]: 1-23.
    pub every_h: u8,
    /// [`Preset::Hours`]: minute past the hour.
    pub minute: u8,
    /// Hours of the day of the "every N" presets (0 and 23: all day).
    pub from: u8,
    pub to: u8,
    /// Weekdays of [`Preset::Weekly`] and of the "every N" presets.
    pub days: Weekdays,
    /// Times of the day as the time inputs give them (`HH:MM`).
    pub times: Vec<String>,
    /// [`Preset::Monthly`]: "1, 15".
    pub month_days: String,
    /// [`Preset::Monthly`]: the last day of the month instead of `month_days`.
    pub last_day: bool,
    pub nth: Nth,
    /// [`Preset::MonthlyNth`]: 0 = Monday.
    pub weekday: u8,
    /// [`Preset::Yearly`].
    pub month: u8,
    pub day: u8,
    pub advanced: String,
    pub cron: String,
    /// The stored expression a preset was opened from, with the fields it opened as: saved
    /// as it was (`hourly` stays `hourly`) until the fields change, so opening and saving
    /// never rewrites the timer file nor restarts it.
    pub stored: Option<(String, Box<Draft>)>,
}

impl Default for Draft {
    fn default() -> Self {
        Draft {
            preset: Preset::Daily,
            every_min: 15,
            every_h: 6,
            minute: 0,
            from: 0,
            to: 23,
            days: Weekdays::MON_FRI,
            times: vec!["03:00".into()],
            month_days: "1".into(),
            last_day: false,
            nth: Nth::First,
            weekday: 6,
            month: 1,
            day: 1,
            advanced: String::new(),
            cron: String::new(),
            stored: None,
        }
    }
}

/// Why the editor cannot make one expression; each is an i18n key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DraftError {
    NoTimes,
    BadTime,
    /// The times are not every combination of their hours and minutes.
    NotAGrid,
    NoDays,
    /// The hours of an "every N hours" range hold fewer than two runs.
    HourRange,
    MonthDays,
    BadDate,
    Empty,
    /// The advanced expression is not valid (the detail is the parser's).
    Invalid,
}

impl DraftError {
    pub fn key(self) -> &'static str {
        match self {
            DraftError::NoTimes => "sch.e.no_times",
            DraftError::BadTime => "sch.e.bad_time",
            DraftError::NotAGrid => "sch.e.not_grid",
            DraftError::NoDays => "sch.e.no_days",
            DraftError::HourRange => "sch.e.hour_range",
            DraftError::MonthDays => "sch.e.month_days",
            DraftError::BadDate => "sch.e.bad_date",
            DraftError::Empty => "sch.e.empty",
            DraftError::Invalid => "sch.e.invalid",
        }
    }
}

impl From<Invalid> for DraftError {
    fn from(e: Invalid) -> Self {
        match e {
            Invalid::NoTimes => DraftError::NoTimes,
            Invalid::TimesNotAGrid => DraftError::NotAGrid,
            Invalid::BadTime => DraftError::BadTime,
            Invalid::BadMinutes | Invalid::BadHours => DraftError::HourRange,
            Invalid::BadDay | Invalid::BadWeekday | Invalid::BadMonth => DraftError::BadDate,
        }
    }
}

fn hhmm(t: TimeOfDay) -> String {
    t.to_string()
}

impl Draft {
    /// The editor showing a stored calendar trigger: its preset when it is one, else the
    /// advanced field with the text as stored (or the cron line of a request).
    pub fn from_calendar(on_calendar: &str, cron: Option<&str>) -> Draft {
        let mut d = Draft::default();
        if on_calendar.trim().is_empty() {
            if let Some(c) = cron.filter(|c| !c.trim().is_empty()) {
                d.preset = Preset::Cron;
                d.cron = c.trim().to_string();
            }
            return d;
        }
        match classify(on_calendar) {
            Parsed::Preset(r) => {
                d.set_recurrence(&r);
                d.stored = Some((on_calendar.trim().to_string(), Box::new(d.clone())));
            }
            Parsed::Advanced(_) | Parsed::Invalid(_) => {
                d.preset = Preset::Advanced;
                d.advanced = on_calendar.trim().to_string();
            }
        }
        d
    }

    /// Shows a preset (the other fields keep their values).
    pub fn set_recurrence(&mut self, r: &Recurrence) {
        let times = |t: &[TimeOfDay]| t.iter().copied().map(hhmm).collect::<Vec<_>>();
        match r {
            Recurrence::Daily { days, times: t } => {
                self.preset = if days.every_day() { Preset::Daily } else { Preset::Weekly };
                if !days.every_day() {
                    self.days = *days;
                }
                self.times = times(t);
            }
            Recurrence::Minutes { every, from, to, days } => {
                self.preset = Preset::Minutes;
                (self.every_min, self.from, self.to, self.days) = (*every, *from, *to, *days);
            }
            Recurrence::Hours { every, minute, from, to, days } => {
                self.preset = Preset::Hours;
                (self.every_h, self.minute, self.from, self.to, self.days) = (*every, *minute, *from, *to, *days);
            }
            Recurrence::Monthly { day, times: t } => {
                self.preset = Preset::Monthly;
                match day {
                    MonthDay::Days(d) => {
                        self.last_day = false;
                        self.month_days = d.iter().map(u8::to_string).collect::<Vec<_>>().join(", ");
                    }
                    MonthDay::Last => self.last_day = true,
                }
                self.times = times(t);
            }
            Recurrence::MonthlyNth { nth, weekday, times: t } => {
                self.preset = Preset::MonthlyNth;
                (self.nth, self.weekday) = (*nth, *weekday);
                self.times = times(t);
            }
            Recurrence::Yearly { month, day, times: t } => {
                self.preset = Preset::Yearly;
                (self.month, self.day) = (*month, *day);
                self.times = times(t);
            }
        }
    }

    fn parsed_times(&self) -> Result<Vec<TimeOfDay>, DraftError> {
        let filled: Vec<&String> = self.times.iter().filter(|t| !t.trim().is_empty()).collect();
        if filled.is_empty() {
            return Err(DraftError::NoTimes);
        }
        filled.iter().map(|t| TimeOfDay::parse(t.trim()).ok_or(DraftError::BadTime)).collect()
    }

    fn weekdays(&self) -> Result<Weekdays, DraftError> {
        if self.days.canonical().0 == 0 && self.days.0 == 0 {
            // no day ticked
            return Err(DraftError::NoDays);
        }
        Ok(self.days.canonical())
    }

    /// The "every N" presets: the weekday chips are optional (none = every day).
    fn optional_days(&self) -> Weekdays {
        self.days.canonical()
    }

    /// The range of an "every N hours" preset: the last hour is the last run in the range,
    /// so a range that does not end on a run is shortened to the run before.
    pub fn hour_range(&self) -> Result<(u8, u8), DraftError> {
        let (every, from, to) = (self.every_h.max(1), self.from.min(23), self.to.min(23));
        if from == 0 && to == 23 {
            return Ok((0, 23));
        }
        if to <= from {
            return Err(DraftError::HourRange);
        }
        let last = from + (to - from) / every * every;
        if last == from {
            return Err(DraftError::HourRange);
        }
        Ok((from, last))
    }

    /// The preset as a [`Recurrence`] (`None` for the advanced and cron fields).
    pub fn recurrence(&self) -> Result<Option<Recurrence>, DraftError> {
        Ok(Some(match self.preset {
            Preset::Advanced | Preset::Cron => return Ok(None),
            Preset::Minutes => {
                if self.from > self.to {
                    return Err(DraftError::HourRange);
                }
                let every = if MINUTE_STEPS.contains(&self.every_min) { self.every_min } else { 15 };
                Recurrence::Minutes { every, from: self.from, to: self.to.min(23), days: self.optional_days() }
            }
            Preset::Hours => {
                let (from, to) = self.hour_range()?;
                Recurrence::Hours { every: self.every_h.clamp(1, 23), minute: self.minute.min(59), from, to, days: self.optional_days() }
            }
            Preset::Daily => Recurrence::Daily { days: Weekdays::EVERY_DAY, times: self.parsed_times()? },
            Preset::Weekly => Recurrence::Daily { days: self.weekdays()?, times: self.parsed_times()? },
            Preset::Monthly => {
                let day = if self.last_day {
                    MonthDay::Last
                } else {
                    let days: Option<Vec<u8>> = self
                        .month_days
                        .split([',', ' ', ';'])
                        .filter(|s| !s.is_empty())
                        .map(|s| s.parse::<u8>().ok().filter(|d| (1..=31).contains(d)))
                        .collect();
                    match days {
                        Some(d) if !d.is_empty() => MonthDay::Days(d),
                        _ => return Err(DraftError::MonthDays),
                    }
                };
                Recurrence::Monthly { day, times: self.parsed_times()? }
            }
            Preset::MonthlyNth => Recurrence::MonthlyNth { nth: self.nth, weekday: self.weekday % 7, times: self.parsed_times()? },
            Preset::Yearly => Recurrence::Yearly { month: self.month, day: self.day, times: self.parsed_times()? },
        }))
    }

    /// The stored expression, while the fields are still those it opened as.
    fn untouched(&self) -> Option<&str> {
        let (text, opened) = self.stored.as_ref()?;
        let mut now = self.clone();
        now.stored = None;
        // a look at the advanced field and back is not an edit of the preset
        now.advanced.clone_from(&opened.advanced);
        now.cron.clone_from(&opened.cron);
        (now == **opened).then_some(text.as_str())
    }

    /// The expression to store and its syntax, or what is wrong. The advanced field is
    /// checked with systemd's grammar here; the NAS checks it again (time zone names, and
    /// `systemd-analyze` when saving).
    pub fn expression(&self) -> Result<(String, CalendarSyntax), DraftError> {
        if let Some(text) = self.untouched() {
            return Ok((text.to_string(), CalendarSyntax::Calendar));
        }
        match self.preset {
            Preset::Advanced => {
                let e = self.advanced.trim();
                if e.is_empty() {
                    return Err(DraftError::Empty);
                }
                match classify(e) {
                    Parsed::Invalid(_) => Err(DraftError::Invalid),
                    _ => Ok((e.to_string(), CalendarSyntax::Calendar)),
                }
            }
            Preset::Cron => {
                let e = self.cron.trim();
                if e.is_empty() { Err(DraftError::Empty) } else { Ok((e.to_string(), CalendarSyntax::Cron)) }
            }
            _ => {
                let r = self.recurrence()?.expect("a preset");
                Ok((r.to_calendar()?, CalendarSyntax::Calendar))
            }
        }
    }

    /// Switching to the advanced field starts from the preset's expression.
    pub fn switch_to(&mut self, preset: Preset) {
        if preset == Preset::Advanced
            && self.preset != Preset::Advanced
            && let Ok((e, CalendarSyntax::Calendar)) = self.expression()
        {
            self.advanced = e;
        }
        if preset == Preset::Weekly && self.days.canonical().0 == 0 {
            self.days = Weekdays::MON_FRI;
        }
        self.preset = preset;
    }

    /// Ticks or unticks a weekday (0 = Monday).
    pub fn toggle_day(&mut self, day: u8, on: bool) {
        // what the chips show: no bit = every day, except in the weekly preset
        let shown = match self.days.0 & 0x7f {
            0 if self.preset != Preset::Weekly => 0x7f,
            b => b,
        };
        let bits = if on { shown | (1 << day) } else { shown & !(1 << day) };
        // all seven ticked stays "all seven" (not the empty "every day" set) while editing
        self.days = Weekdays(bits);
    }

    /// The weekday is ticked (every day ticks them all).
    pub fn day_on(&self, day: u8) -> bool {
        let b = self.days.0 & 0x7f;
        b == 0 && self.preset != Preset::Weekly || b & (1 << day) != 0
    }

    pub fn add_time(&mut self) {
        let next = self
            .times
            .iter()
            .filter_map(|t| TimeOfDay::parse(t.trim()))
            .max()
            .map(|t| TimeOfDay::new((t.hour + 12) % 24, t.minute))
            .unwrap_or(TimeOfDay::new(3, 0));
        self.times.push(hhmm(next));
    }

    pub fn remove_time(&mut self, i: usize) {
        if self.times.len() > 1 && i < self.times.len() {
            self.times.remove(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(d: &Draft) -> String {
        d.expression().unwrap().0
    }

    #[test]
    fn stored_expressions_open_as_their_preset_and_save_the_same() {
        for (stored, preset) in [
            ("*-*-* 03:00:00", Preset::Daily),
            ("*-*-* 08,20:00:00", Preset::Daily),
            ("Mon..Fri *-*-* 02:30:00", Preset::Weekly),
            ("*-*-* *:00/15:00", Preset::Minutes),
            ("Mon..Fri *-*-* 08..18:00/15:00", Preset::Minutes),
            ("*-*-* 00/6:30:00", Preset::Hours),
            ("*-*-* 08..20/4:00:00", Preset::Hours),
            ("*-*-01,15 04:00:00", Preset::Monthly),
            ("*-*~01 23:00:00", Preset::Monthly),
            ("Sun *-*-01..07 02:30:00", Preset::MonthlyNth),
            ("*-12-25 00:00:00", Preset::Yearly),
        ] {
            let d = Draft::from_calendar(stored, None);
            assert_eq!(d.preset, preset, "{stored}");
            assert_eq!(expr(&d), stored, "{stored} saves unchanged");
        }
        // other spellings of a preset open as the preset and save as stored until edited (no
        // timer file change, no restart on open and save)
        for (stored, preset) in [
            ("hourly", Preset::Hours),
            ("Mon..Fri 02:30", Preset::Weekly),
            ("*-*-* 08..19/3:00:00", Preset::Hours),
            ("daily", Preset::Daily),
        ] {
            let d = Draft::from_calendar(stored, None);
            assert_eq!((d.preset, expr(&d).as_str()), (preset, stored), "{stored}");
            // a look at the advanced field and back is not an edit
            let mut look = d.clone();
            look.switch_to(Preset::Advanced);
            assert_eq!(look.advanced, stored);
            look.switch_to(preset);
            assert_eq!(expr(&look), stored);
        }
        // an edit saves the preset's normalized expression
        let mut d = Draft::from_calendar("hourly", None);
        d.minute = 15;
        assert_eq!(expr(&d), "*-*-* *:15:00");
        d.minute = 0;
        assert_eq!(expr(&d), "hourly", "back to what it opened as");
        let mut d = Draft::from_calendar("Mon..Fri 02:30", None);
        d.times = vec!["02:45".into()];
        assert_eq!(expr(&d), "Mon..Fri *-*-* 02:45:00");
        // anything else stays as typed in the advanced field
        let d = Draft::from_calendar("quarterly", None);
        assert_eq!((d.preset, d.advanced.as_str()), (Preset::Advanced, "quarterly"));
        assert_eq!(expr(&d), "quarterly");
        let d = Draft::from_calendar("", Some(" 0 4 * * * "));
        assert_eq!(d.preset, Preset::Cron);
        assert_eq!(d.expression().unwrap(), ("0 4 * * *".into(), CalendarSyntax::Cron));
    }

    #[test]
    fn presets_make_one_expression() {
        let mut d = Draft { preset: Preset::Weekly, days: Weekdays::of(&[0, 2]), times: vec!["07:30".into(), "19:30".into()], ..Draft::default() };
        assert_eq!(expr(&d), "Mon,Wed *-*-* 07,19:30:00");
        d.preset = Preset::Daily;
        assert_eq!(expr(&d), "*-*-* 07,19:30:00");
        d.preset = Preset::Minutes;
        d.every_min = 10;
        (d.from, d.to) = (8, 18);
        assert_eq!(expr(&d), "Mon,Wed *-*-* 08..18:00/10:00");
        d.preset = Preset::Hours;
        d.every_h = 4;
        d.minute = 5;
        // 08..18 every 4 h: the last run is 16:05
        assert_eq!(expr(&d), "Mon,Wed *-*-* 08..16/4:05:00");
        d.days = Weekdays(0);
        (d.from, d.to) = (0, 23);
        assert_eq!(expr(&d), "*-*-* 00/4:05:00");
        d.preset = Preset::MonthlyNth;
        (d.nth, d.weekday) = (Nth::Last, 4);
        assert_eq!(expr(&d), "Fri *-*~07/1 07,19:30:00");
        d.preset = Preset::Monthly;
        d.month_days = "15, 1".into();
        assert_eq!(expr(&d), "*-*-01,15 07,19:30:00");
        d.last_day = true;
        assert_eq!(expr(&d), "*-*~01 07,19:30:00");
        d.preset = Preset::Yearly;
        (d.month, d.day) = (2, 29);
        assert_eq!(expr(&d), "*-02-29 07,19:30:00");
    }

    #[test]
    fn errors_name_what_to_fix() {
        let base = Draft::default();
        let err = |d: Draft| d.expression().unwrap_err();
        assert_eq!(err(Draft { times: vec![String::new()], ..base.clone() }), DraftError::NoTimes);
        assert_eq!(err(Draft { times: vec!["25:00".into()], ..base.clone() }), DraftError::BadTime);
        assert_eq!(err(Draft { times: vec!["08:00".into(), "20:30".into()], ..base.clone() }), DraftError::NotAGrid);
        assert_eq!(err(Draft { preset: Preset::Weekly, days: Weekdays(0), ..base.clone() }), DraftError::NoDays);
        assert_eq!(err(Draft { preset: Preset::Hours, every_h: 6, from: 8, to: 12, ..base.clone() }), DraftError::HourRange);
        assert_eq!(err(Draft { preset: Preset::Hours, from: 12, to: 8, ..base.clone() }), DraftError::HourRange);
        assert_eq!(err(Draft { preset: Preset::Monthly, month_days: "0, 32".into(), ..base.clone() }), DraftError::MonthDays);
        assert_eq!(err(Draft { preset: Preset::Yearly, month: 2, day: 30, ..base.clone() }), DraftError::BadDate);
        assert_eq!(err(Draft { preset: Preset::Advanced, advanced: "  ".into(), ..base.clone() }), DraftError::Empty);
        assert_eq!(err(Draft { preset: Preset::Advanced, advanced: "Mon..Fry 25:00".into(), ..base.clone() }), DraftError::Invalid);
        assert_eq!(err(Draft { preset: Preset::Cron, ..base }), DraftError::Empty);
        // empty extra time inputs are ignored
        let d = Draft { times: vec!["04:00".into(), " ".into()], ..Draft::default() };
        assert_eq!(expr(&d), "*-*-* 04:00:00");
    }

    #[test]
    fn editing_helpers() {
        let mut d = Draft::default();
        d.add_time();
        assert_eq!(d.times, ["03:00", "15:00"]);
        d.remove_time(0);
        d.remove_time(0);
        assert_eq!(d.times, ["15:00"], "the last time stays");
        d.switch_to(Preset::Advanced);
        assert_eq!(d.advanced, "*-*-* 15:00:00");
        // weekly starts from Monday-Friday when no day is chosen
        let mut w = Draft { days: Weekdays(0), ..Draft::default() };
        w.switch_to(Preset::Weekly);
        assert_eq!(w.days, Weekdays::MON_FRI);
        w.toggle_day(5, true);
        w.toggle_day(6, true);
        assert!((0..7).all(|x| w.day_on(x)));
        assert_eq!(expr(&w), "*-*-* 03:00:00", "every weekday ticked = every day");
        w.toggle_day(0, false);
        assert!(!w.day_on(0) && w.day_on(1));
        for x in 1..7 {
            w.toggle_day(x, false);
        }
        assert_eq!(w.expression().unwrap_err(), DraftError::NoDays);
        // the "every N" presets: no chip ticked = every day
        let m = Draft { preset: Preset::Minutes, days: Weekdays(0), ..Draft::default() };
        assert!(m.day_on(3));
        assert_eq!(expr(&m), "*-*-* *:00/15:00");
    }
}
