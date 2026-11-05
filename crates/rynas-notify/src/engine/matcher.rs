//! Rules: validation, compilation (regular expressions) and evaluation, quiet hours.

use rynas_api_types::notify::{MatchMode, Notification, NotifyRule, NotifySeverity, QuietWindow};

/// Broken-down local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    /// 1 = Monday … 7 = Sunday.
    pub weekday: u8,
}

fn from_tm(tm: &libc::tm) -> LocalTime {
    LocalTime {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u8,
        day: tm.tm_mday as u8,
        hour: tm.tm_hour as u8,
        minute: tm.tm_min as u8,
        weekday: if tm.tm_wday == 0 { 7 } else { tm.tm_wday as u8 },
    }
}

/// The system's local time (TZ, /etc/localtime).
pub fn local_time(ms: u64) -> LocalTime {
    let t = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r only writes the tm we own; a zeroed tm is a valid value.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        from_tm(&tm)
    }
}

/// UTC (tests, and systems without a time zone).
pub fn utc_time(ms: u64) -> LocalTime {
    let t = (ms / 1000) as libc::time_t;
    // SAFETY: as above.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::gmtime_r(&t, &mut tm);
        from_tm(&tm)
    }
}

/// "HH:MM" → minutes after midnight.
pub fn parse_hhmm(s: &str) -> Option<u16> {
    let (h, m) = s.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// When the quiet windows active at `now` end (ms), or `None` when none is active.
/// Chained windows (one ending when the next starts) are followed.
pub fn quiet_until(windows: &[QuietWindow], now: u64, local: fn(u64) -> LocalTime) -> Option<u64> {
    let mut at = now;
    let mut found = None;
    // a chain longer than a week means "always quiet": stop after 8 hops
    for _ in 0..8 {
        let lt = local(at);
        let m = u64::from(lt.hour) * 60 + u64::from(lt.minute);
        let minute_start = at - at % 60_000;
        let mut end: Option<u64> = None;
        for w in windows {
            let (Some(s), Some(e)) = (parse_hhmm(&w.start), parse_hhmm(&w.end)) else {
                continue;
            };
            let (s, e) = (u64::from(s), u64::from(e));
            let on = |d: u8| w.days.is_empty() || w.days.contains(&d);
            let prev_day = if lt.weekday == 1 { 7 } else { lt.weekday - 1 };
            let left = if s < e {
                (on(lt.weekday) && m >= s && m < e).then(|| e - m)
            } else if s > e {
                if on(lt.weekday) && m >= s {
                    Some(24 * 60 - m + e)
                } else if on(prev_day) && m < e {
                    Some(e - m)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(l) = left {
                let t = minute_start + l * 60_000;
                end = Some(end.map_or(t, |x: u64| x.max(t)));
            }
        }
        match end {
            Some(t) if t > at => {
                found = Some(t);
                at = t;
            }
            _ => break,
        }
    }
    found
}

/// A rule ready for evaluation.
pub struct Compiled {
    pub rule: NotifyRule,
    source: Option<regex_lite::Regex>,
    fields: Vec<Option<regex_lite::Regex>>,
}

/// Largest compiled regular expression (a hostile pattern cannot eat memory).
const REGEX_SIZE: usize = 1 << 16;

fn regex(p: &str) -> Result<regex_lite::Regex, String> {
    if p.len() > 512 {
        return Err("regular expression longer than 512 characters".into());
    }
    regex_lite::RegexBuilder::new(p)
        .size_limit(REGEX_SIZE)
        .build()
        .map_err(|e| format!("invalid regular expression {p:?}: {e}"))
}

impl Compiled {
    pub fn new(rule: NotifyRule) -> Result<Self, String> {
        let source = rule.source.as_deref().map(regex).transpose()?;
        let fields = rule
            .fields
            .iter()
            .map(|f| f.regex.then(|| regex(&f.value)).transpose())
            .collect::<Result<_, _>>()?;
        Ok(Self {
            rule,
            source,
            fields,
        })
    }

    /// Whether the event (as stored) is selected by the rule's conditions.
    pub fn matches(&self, n: &Notification) -> bool {
        let r = &self.rule;
        let mut conds: Vec<bool> = Vec::new();
        if r.min_severity > NotifySeverity::Info {
            conds.push(n.severity >= r.min_severity);
        }
        if !r.categories.is_empty() {
            conds.push(r.categories.contains(&n.category));
        }
        if let Some(re) = &self.source {
            conds.push(re.is_match(&n.source));
        }
        for (f, re) in r.fields.iter().zip(&self.fields) {
            conds.push(match (n.fields.get(&f.field), re) {
                (Some(v), Some(re)) => re.is_match(v),
                (Some(v), None) => *v == f.value,
                (None, _) => false,
            });
        }
        let hit = conds.is_empty()
            || match r.mode {
                MatchMode::All => conds.iter().all(|c| *c),
                MatchMode::Any => conds.iter().any(|c| *c),
            };
        hit != r.invert
    }
}

/// Checks a rule from the API (targets are checked by the engine).
pub fn validate(r: &NotifyRule) -> Result<(), String> {
    let name = r.name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err("name: 1 to 100 printable characters".into());
    }
    if r.categories.len() > 9 || r.fields.len() > 16 || r.quiet_hours.len() > 14 {
        return Err("too many conditions".into());
    }
    for f in &r.fields {
        if f.field.is_empty() || f.field.len() > 64 || f.value.len() > 512 {
            return Err("fields: field name 1 to 64 characters, value at most 512".into());
        }
    }
    for w in &r.quiet_hours {
        if parse_hhmm(&w.start).is_none() || parse_hhmm(&w.end).is_none() {
            return Err(format!(
                "quiet_hours: times are \"HH:MM\" ({:?}–{:?})",
                w.start, w.end
            ));
        }
        if w.start == w.end {
            return Err("quiet_hours: start and end are the same".into());
        }
        if w.days.iter().any(|d| !(1..=7).contains(d)) {
            return Err("quiet_hours: days are 1 (Monday) to 7 (Sunday)".into());
        }
    }
    if r.targets.is_empty() || r.targets.len() > 32 {
        return Err("targets: 1 to 32".into());
    }
    if r.group_window_s > 86_400 {
        return Err("group_window_s: at most one day".into());
    }
    if r.repeat_interval_s != 0 && !(300..=30 * 86_400).contains(&r.repeat_interval_s) {
        return Err("repeat_interval_s: 0 (off) or 5 minutes to 30 days".into());
    }
    Compiled::new(r.clone()).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::notify::{FieldMatch, NotifyCategory};

    // 2025-11-05 was a Wednesday
    const WED_0000: u64 = 1_762_300_800_000;
    const H: u64 = 3_600_000;

    fn ev(sev: NotifySeverity, cat: NotifyCategory, source: &str) -> Notification {
        Notification {
            severity: sev,
            category: cat,
            source: source.into(),
            fields: [("disk".to_string(), "sda".to_string())].into(),
            ..Default::default()
        }
    }

    fn rule() -> NotifyRule {
        NotifyRule {
            name: "r".into(),
            enabled: true,
            targets: vec![1],
            ..Default::default()
        }
    }

    #[test]
    fn conditions_mode_and_invert() {
        let mut r = rule();
        let c = Compiled::new(r.clone()).unwrap();
        assert!(c.matches(&ev(NotifySeverity::Info, NotifyCategory::App, "x")));
        r.min_severity = NotifySeverity::Error;
        r.categories = vec![NotifyCategory::Storage];
        let c = Compiled::new(r.clone()).unwrap();
        assert!(c.matches(&ev(
            NotifySeverity::Critical,
            NotifyCategory::Storage,
            "storage"
        )));
        assert!(!c.matches(&ev(
            NotifySeverity::Warning,
            NotifyCategory::Storage,
            "storage"
        )));
        assert!(!c.matches(&ev(NotifySeverity::Critical, NotifyCategory::Ups, "ups")));
        r.mode = MatchMode::Any;
        let c = Compiled::new(r.clone()).unwrap();
        assert!(c.matches(&ev(NotifySeverity::Critical, NotifyCategory::Ups, "ups")));
        assert!(!c.matches(&ev(NotifySeverity::Info, NotifyCategory::Ups, "ups")));
        r.invert = true;
        let c = Compiled::new(r.clone()).unwrap();
        assert!(c.matches(&ev(NotifySeverity::Info, NotifyCategory::Ups, "ups")));
        let mut r = rule();
        r.source = Some("^synodeb:".into());
        r.fields = vec![FieldMatch {
            field: "disk".into(),
            value: "sd[a-d]".into(),
            regex: true,
        }];
        let c = Compiled::new(r.clone()).unwrap();
        assert!(c.matches(&ev(
            NotifySeverity::Info,
            NotifyCategory::App,
            "synodeb:smartd"
        )));
        assert!(!c.matches(&ev(NotifySeverity::Info, NotifyCategory::App, "storage")));
        r.fields[0] = FieldMatch {
            field: "disk".into(),
            value: "sdb".into(),
            regex: false,
        };
        assert!(!Compiled::new(r).unwrap().matches(&ev(
            NotifySeverity::Info,
            NotifyCategory::App,
            "synodeb:x"
        )));
    }

    #[test]
    fn validation() {
        let mut r = rule();
        assert!(validate(&r).is_ok());
        r.source = Some("(".into());
        assert!(validate(&r).is_err());
        r.source = None;
        r.quiet_hours = vec![QuietWindow {
            days: vec![8],
            start: "23:00".into(),
            end: "07:00".into(),
        }];
        assert!(validate(&r).is_err());
        r.quiet_hours[0].days = vec![1];
        r.quiet_hours[0].end = "7:00".into();
        assert!(validate(&r).is_err());
        r.quiet_hours.clear();
        r.repeat_interval_s = 60;
        assert!(validate(&r).is_err());
        r.repeat_interval_s = 0;
        r.targets.clear();
        assert!(validate(&r).is_err());
    }

    #[test]
    fn quiet_windows() {
        let night = vec![QuietWindow {
            days: vec![],
            start: "23:00".into(),
            end: "07:00".into(),
        }];
        // Wednesday 00:00 is inside the window started on Tuesday: ends at 07:00
        assert_eq!(
            quiet_until(&night, WED_0000, utc_time),
            Some(WED_0000 + 7 * H)
        );
        assert_eq!(quiet_until(&night, WED_0000 + 12 * H, utc_time), None);
        assert_eq!(
            quiet_until(&night, WED_0000 + 23 * H + 30 * 60_000, utc_time),
            Some(WED_0000 + 31 * H)
        );
        // only windows starting on Monday: Wednesday night is not quiet
        let mon = vec![QuietWindow {
            days: vec![1],
            start: "23:00".into(),
            end: "07:00".into(),
        }];
        assert_eq!(quiet_until(&mon, WED_0000, utc_time), None);
        // chained: 07:00-09:00 follows the night
        let mut chain = night.clone();
        chain.push(QuietWindow {
            days: vec![3],
            start: "07:00".into(),
            end: "09:00".into(),
        });
        assert_eq!(
            quiet_until(&chain, WED_0000 + H, utc_time),
            Some(WED_0000 + 9 * H)
        );
        assert_eq!(utc_time(WED_0000).weekday, 3);
    }
}
