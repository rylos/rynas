//! Rules: validation, conflicts, next occurrences in local time.
//!
//! Times are wall-clock times of the system time zone. A rule time that does not exist on a
//! DST spring-forward day (02:30 in Europe) happens at the first valid time after the gap
//! (03:30), a time that exists twice on the fall-back day happens at its first occurrence
//! (jiff's "compatible" disambiguation, the same as systemd's `OnCalendar=`).

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use rynas_api_types::power::{PowerAction, PowerRule, PowerSchedule, ShutdownPolicy, Weekday};

/// Most rules in a schedule (M1).
pub const MAX_RULES: usize = 50;
/// A shutdown must come at least this long after a startup.
pub const MIN_UP_MIN: u32 = 10;
pub const MAX_WAIT_MIN: u32 = 720;
const WEEK_MIN: u32 = 7 * 1440;

/// `[a-z0-9][a-z0-9-]{0,31}`.
pub fn valid_rule_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The scheduler task of a shutdown rule.
pub fn task_id(rule: &str) -> String {
    format!("power-off-{rule}")
}

fn hhmm(r: &PowerRule) -> String {
    format!("{:02}:{:02}", r.hour, r.minute)
}

/// Normalizes and validates a schedule: days sorted and unique, the shutdown policy only on
/// shutdown rules (defaulted there), ids unique, conflicts between enabled rules refused.
pub fn normalize(s: PowerSchedule) -> Result<PowerSchedule, String> {
    if s.rules.len() > MAX_RULES {
        return Err(format!("at most {MAX_RULES} rules"));
    }
    let mut rules: Vec<PowerRule> = Vec::with_capacity(s.rules.len());
    for mut r in s.rules {
        r.id = r.id.trim().to_string();
        if !valid_rule_id(&r.id) {
            return Err(format!(
                "invalid rule id {:?}: 1-32 lowercase letters, digits and '-', starting with a letter or digit",
                r.id
            ));
        }
        if rules.iter().any(|x| x.id == r.id) {
            return Err(format!("rule {} is defined twice", r.id));
        }
        if r.days.is_empty() {
            return Err(format!("rule {}: choose at least one day", r.id));
        }
        r.days.sort();
        r.days.dedup();
        if r.hour > 23 || r.minute > 59 {
            return Err(format!("rule {}: invalid time {}", r.id, hhmm(&r)));
        }
        match r.action {
            PowerAction::Startup => r.shutdown = None,
            PowerAction::Shutdown => {
                let p = r.shutdown.get_or_insert_with(ShutdownPolicy::default);
                if !(1..=MAX_WAIT_MIN).contains(&p.max_wait_min) {
                    return Err(format!(
                        "rule {}: the wait is 1 to {MAX_WAIT_MIN} minutes",
                        r.id
                    ));
                }
            }
        }
        rules.push(r);
    }
    conflicts(&rules)?;
    Ok(PowerSchedule { rules })
}

/// Minutes since Monday 00:00 of each occurrence of a rule in a week.
fn week_minutes(r: &PowerRule) -> impl Iterator<Item = u32> + '_ {
    r.days
        .iter()
        .map(move |d| d.index() as u32 * 1440 + r.hour as u32 * 60 + r.minute as u32)
}

fn day_time(m: u32) -> String {
    let d = Weekday::ALL[(m / 1440) as usize % 7].systemd();
    format!("{d} {:02}:{:02}", (m % 1440) / 60, m % 60)
}

/// Startup and shutdown at the same minute, or a shutdown less than [`MIN_UP_MIN`] minutes
/// after a startup (also across midnight and the end of the week). Disabled rules are not
/// checked.
pub fn conflicts(rules: &[PowerRule]) -> Result<(), String> {
    let on = |a: PowerAction| rules.iter().filter(move |r| r.enabled && r.action == a);
    for up in on(PowerAction::Startup) {
        for down in on(PowerAction::Shutdown) {
            for s in week_minutes(up) {
                for t in week_minutes(down) {
                    let delta = (t + WEEK_MIN - s) % WEEK_MIN;
                    if delta == 0 {
                        return Err(format!(
                            "rules {} and {} start up and shut down at the same time ({})",
                            up.id,
                            down.id,
                            day_time(s)
                        ));
                    }
                    if delta < MIN_UP_MIN {
                        return Err(format!(
                            "rule {} shuts down at {}, less than {MIN_UP_MIN} minutes after the startup of rule {} ({})",
                            down.id,
                            day_time(t),
                            up.id,
                            day_time(s)
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// systemd `OnCalendar=` of a rule (systemd timers run in local time and handle DST).
pub fn on_calendar(r: &PowerRule) -> String {
    let time = format!("{:02}:{:02}:00", r.hour, r.minute);
    if r.days.len() == 7 {
        format!("*-*-* {time}")
    } else {
        let days: Vec<&str> = r.days.iter().map(|d| d.systemd()).collect();
        format!("{} *-*-* {time}", days.join(","))
    }
}

fn weekday_of(d: Date) -> Weekday {
    Weekday::ALL[d.weekday().to_monday_zero_offset() as usize]
}

/// The instant of `hour:minute` on civil day `d` in `tz` (gap: after it; fold: first).
pub fn instant(d: Date, hour: u8, minute: u8, tz: &TimeZone) -> Option<Timestamp> {
    let dt = d.at(hour as i8, minute as i8, 0, 0);
    tz.to_ambiguous_timestamp(dt).compatible().ok()
}

/// The next occurrence strictly after `now` of an enabled rule with `action`, and its rule.
pub fn next(
    rules: &[PowerRule],
    action: PowerAction,
    now: Timestamp,
    tz: &TimeZone,
) -> Option<(Timestamp, String)> {
    let today = now.to_zoned(tz.clone()).date();
    let mut best: Option<(Timestamp, String)> = None;
    // today and the next 7 days cover a whole week even when today's time already passed
    for offset in 0..=7i64 {
        let Ok(day) = today.checked_add(jiff::Span::new().days(offset)) else {
            continue;
        };
        let wd = weekday_of(day);
        for r in rules
            .iter()
            .filter(|r| r.enabled && r.action == action && r.days.contains(&wd))
        {
            let Some(t) = instant(day, r.hour, r.minute, tz) else {
                continue;
            };
            if t > now && best.as_ref().is_none_or(|(b, _)| t < *b) {
                best = Some((t, r.id.clone()));
            }
        }
        if best.is_some() {
            // later days only give later times
            break;
        }
    }
    best
}

/// Seconds an RTC alarm must be ahead of now to be sure it fires after the poweroff.
pub const MIN_LEAD_S: i64 = 120;

/// When to wake: the next startup, but never closer than [`MIN_LEAD_S`] (a startup due in a
/// few seconds is late rather than missed for a week).
pub fn wake_time(rules: &[PowerRule], now: Timestamp, tz: &TimeZone) -> Option<(i64, String)> {
    let (t, rule) = next(rules, PowerAction::Startup, now, tz)?;
    Some((t.as_second().max(now.as_second() + MIN_LEAD_S), rule))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::power::BusyPolicy;

    fn rule(id: &str, action: PowerAction, days: &[Weekday], h: u8, m: u8) -> PowerRule {
        PowerRule {
            id: id.into(),
            enabled: true,
            action,
            days: days.to_vec(),
            hour: h,
            minute: m,
            shutdown: None,
        }
    }

    fn tz(name: &str) -> TimeZone {
        TimeZone::get(name).unwrap()
    }

    fn at(s: &str, tz: &TimeZone) -> Timestamp {
        s.parse::<jiff::civil::DateTime>()
            .unwrap()
            .to_zoned(tz.clone())
            .unwrap()
            .timestamp()
    }

    fn local(t: Timestamp, tz: &TimeZone) -> String {
        t.to_zoned(tz.clone())
            .strftime("%a %Y-%m-%d %H:%M %Z")
            .to_string()
    }

    use PowerAction::{Shutdown, Startup};
    use Weekday::*;

    #[test]
    fn validation() {
        let ok = normalize(PowerSchedule {
            rules: vec![
                rule("up", Startup, &[Fri, Mon, Mon], 7, 0),
                rule("down", Shutdown, &[Mon], 23, 30),
            ],
        })
        .unwrap();
        assert_eq!(ok.rules[0].days, vec![Mon, Fri]);
        assert_eq!(ok.rules[0].shutdown, None);
        let p = ok.rules[1].shutdown.unwrap();
        assert_eq!((p.when_busy, p.max_wait_min), (BusyPolicy::Wait, 30));
        let bad = |rules: Vec<PowerRule>| normalize(PowerSchedule { rules }).unwrap_err();
        assert!(bad(vec![rule("Up", Startup, &[Mon], 7, 0)]).contains("invalid rule id"));
        assert!(bad(vec![rule("a", Startup, &[], 7, 0)]).contains("at least one day"));
        assert!(bad(vec![rule("a", Startup, &[Mon], 24, 0)]).contains("invalid time"));
        assert!(
            bad(vec![
                rule("a", Startup, &[Mon], 7, 0),
                rule("a", Startup, &[Tue], 7, 0)
            ])
            .contains("twice")
        );
        let mut w = rule("a", Shutdown, &[Mon], 7, 0);
        w.shutdown = Some(ShutdownPolicy {
            max_wait_min: 0,
            ..Default::default()
        });
        assert!(bad(vec![w]).contains("wait"));
        assert!(
            bad((0..51)
                .map(|i| rule(&format!("r{i}"), Startup, &[Mon], 1, 0))
                .collect())
            .contains("at most")
        );
    }

    #[test]
    fn conflict_rules() {
        let c = |a: PowerRule, b: PowerRule| conflicts(&[a, b]);
        // same minute
        assert!(
            c(
                rule("u", Startup, &[Mon], 7, 0),
                rule("d", Shutdown, &[Mon, Tue], 7, 0)
            )
            .unwrap_err()
            .contains("same time")
        );
        // 9 minutes after: refused; 10: fine; before the startup: fine
        assert!(
            c(
                rule("u", Startup, &[Mon], 7, 0),
                rule("d", Shutdown, &[Mon], 7, 9)
            )
            .unwrap_err()
            .contains("less than 10 minutes")
        );
        assert!(
            c(
                rule("u", Startup, &[Mon], 7, 0),
                rule("d", Shutdown, &[Mon], 7, 10)
            )
            .is_ok()
        );
        assert!(
            c(
                rule("u", Startup, &[Mon], 7, 0),
                rule("d", Shutdown, &[Mon], 6, 55)
            )
            .is_ok()
        );
        // across midnight and the end of the week (Sun 23:55 → Mon 00:03)
        assert!(
            c(
                rule("u", Startup, &[Sun], 23, 55),
                rule("d", Shutdown, &[Mon], 0, 3)
            )
            .is_err()
        );
        // a disabled rule does not conflict
        let mut off = rule("d", Shutdown, &[Mon], 7, 0);
        off.enabled = false;
        assert!(c(rule("u", Startup, &[Mon], 7, 0), off).is_ok());
    }

    #[test]
    fn calendars() {
        assert_eq!(
            on_calendar(&rule("d", Shutdown, &[Mon, Wed], 23, 5)),
            "Mon,Wed *-*-* 23:05:00"
        );
        assert_eq!(
            on_calendar(&rule("d", Shutdown, &Weekday::ALL, 1, 0)),
            "*-*-* 01:00:00"
        );
    }

    #[test]
    fn next_times_by_weekday() {
        let z = tz("Europe/Rome");
        let rules = vec![
            rule("work", Startup, &[Mon, Tue, Wed, Thu, Fri], 7, 0),
            rule("weekend", Startup, &[Sat, Sun], 9, 30),
            rule("night", Shutdown, &Weekday::ALL, 23, 30),
        ];
        // Wednesday 2026-11-04 10:00: next startup Thursday 07:00, shutdown today 23:30
        let now = at("2026-11-04T10:00", &z);
        let (t, r) = next(&rules, Startup, now, &z).unwrap();
        assert_eq!(
            (local(t, &z), r.as_str()),
            ("Thu 2026-11-05 07:00 CET".into(), "work")
        );
        let (t, _) = next(&rules, Shutdown, now, &z).unwrap();
        assert_eq!(local(t, &z), "Wed 2026-11-04 23:30 CET");
        // Friday after the startup: the weekend rule
        let (t, r) = next(&rules, Startup, at("2026-11-06T07:00", &z), &z).unwrap();
        assert_eq!(
            (local(t, &z), r.as_str()),
            ("Sat 2026-11-07 09:30 CET".into(), "weekend")
        );
        // exactly at a rule's time: strictly after, so the next one
        let (t, _) = next(&rules, Shutdown, at("2026-11-04T23:30", &z), &z).unwrap();
        assert_eq!(local(t, &z), "Thu 2026-11-05 23:30 CET");
        // a single weekly rule whose time passed today: same weekday next week
        let weekly = vec![rule("w", Startup, &[Wed], 8, 0)];
        let (t, _) = next(&weekly, Startup, now, &z).unwrap();
        assert_eq!(local(t, &z), "Wed 2026-11-11 08:00 CET");
        // nothing enabled
        let mut off = weekly.clone();
        off[0].enabled = false;
        assert_eq!(next(&off, Startup, now, &z), None);
        assert_eq!(
            next(&rules, Startup, now, &TimeZone::UTC).map(|x| x.1),
            Some("work".into())
        );
    }

    #[test]
    fn dst_transitions() {
        let z = tz("Europe/Rome");
        // spring forward: 2026-03-29 02:00 → 03:00; a 02:30 rule happens at 03:30 CEST
        let r = vec![rule("odd", Startup, &[Sun], 2, 30)];
        let (t, _) = next(&r, Startup, at("2026-03-28T12:00", &z), &z).unwrap();
        assert_eq!(local(t, &z), "Sun 2026-03-29 03:30 CEST");
        // the 07:00 rule on the day after is 07:00 CEST (UTC+2), not 07:00 CET
        let r = vec![rule("work", Startup, &[Mon], 7, 0)];
        let (t, _) = next(&r, Startup, at("2026-03-28T12:00", &z), &z).unwrap();
        assert_eq!(local(t, &z), "Mon 2026-03-30 07:00 CEST");
        assert_eq!(t.as_second() % 86_400, 5 * 3600, "05:00 UTC");
        // fall back: 2026-10-25 03:00 → 02:00; a 02:30 rule happens once, at the first 02:30
        let r = vec![rule("odd", Startup, &[Sun], 2, 30)];
        let (t, _) = next(&r, Startup, at("2026-10-24T12:00", &z), &z).unwrap();
        assert_eq!(local(t, &z), "Sun 2026-10-25 02:30 CEST");
        // ... and after it passed (second 02:30, CET), the next week
        let (t2, _) = next(&r, Startup, t + jiff::SignedDuration::from_secs(3600), &z).unwrap();
        assert_eq!(local(t2, &z), "Sun 2026-11-01 02:30 CET");
        // across the change, computed from before it: the wall clock time stays 07:00
        let r = vec![rule("work", Startup, &[Mon], 7, 0)];
        let (t, _) = next(&r, Startup, at("2026-10-24T12:00", &z), &z).unwrap();
        assert_eq!(local(t, &z), "Mon 2026-10-26 07:00 CET");
        // US rules differ (2026-03-08)
        let ny = tz("America/New_York");
        let r = vec![rule("odd", Startup, &[Sun], 2, 15)];
        let (t, _) = next(&r, Startup, at("2026-03-07T12:00", &ny), &ny).unwrap();
        assert_eq!(local(t, &ny), "Sun 2026-03-08 03:15 EDT");
    }

    #[test]
    fn wake_lead() {
        let z = tz("Europe/Rome");
        let r = vec![rule("up", Startup, &Weekday::ALL, 7, 0)];
        // shutting down at 06:59:30: wake at 07:01:30 rather than tomorrow
        let now = at("2026-11-04T06:59:30", &z);
        let (w, _) = wake_time(&r, now, &z).unwrap();
        assert_eq!(w, now.as_second() + MIN_LEAD_S);
        let now = at("2026-11-04T05:00", &z);
        let (w, _) = wake_time(&r, now, &z).unwrap();
        assert_eq!(w, at("2026-11-04T07:00", &z).as_second());
        assert_eq!(wake_time(&[], now, &z), None);
    }
}
