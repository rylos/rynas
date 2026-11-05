//! Calendar expressions: a cheap syntactic check of `OnCalendar=` values (the real check is
//! `systemd-analyze calendar`, see [`crate::probe`]), the conversion of crontab schedules to
//! systemd calendar events, and the parser of `systemd-analyze calendar` output.

use rynas_api_types::scheduler::TaskTrigger;

/// Longest `OnCalendar=` accepted.
pub const MAX_CALENDAR: usize = 256;

/// Characters that may appear in a calendar event (weekday and month names, numbers,
/// `*-:/,.~+`, spaces, time zone names such as `Europe/Rome` or `America/New_York`). No `%`
/// (unit specifiers), `$`, quotes, backslashes or newlines can reach a unit file.
pub fn plausible_calendar(s: &str) -> Result<(), String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("the calendar expression is empty".into());
    }
    if t.len() > MAX_CALENDAR {
        return Err(format!(
            "calendar expressions are at most {MAX_CALENDAR} characters"
        ));
    }
    if let Some(c) = t
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || " :*/,.~+-_".contains(*c)))
    {
        return Err(format!(
            "invalid character {c:?} in the calendar expression"
        ));
    }
    Ok(())
}

/// What a crontab schedule becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Converted {
    Calendar(String),
    Boot,
}

impl Converted {
    pub fn trigger(self) -> TaskTrigger {
        match self {
            Converted::Calendar(c) => TaskTrigger::Calendar {
                on_calendar: c,
                cron: None,
                persistent: true,
                randomized_delay_s: 0,
            },
            Converted::Boot => TaskTrigger::Boot { delay_s: 60 },
        }
    }
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

struct Field {
    name: &'static str,
    min: u32,
    max: u32,
    names: &'static [&'static str],
    /// First value of `*/n` (`0` for minutes and hours, `1` for days and months).
    names_offset: u32,
}

const MINUTE: Field = Field {
    name: "minute",
    min: 0,
    max: 59,
    names: &[],
    names_offset: 0,
};
const HOUR: Field = Field {
    name: "hour",
    min: 0,
    max: 23,
    names: &[],
    names_offset: 0,
};
const DOM: Field = Field {
    name: "day of month",
    min: 1,
    max: 31,
    names: &[],
    names_offset: 0,
};
const MONTH: Field = Field {
    name: "month",
    min: 1,
    max: 12,
    names: &MONTHS,
    names_offset: 1,
};
const DOW: Field = Field {
    name: "day of week",
    min: 0,
    max: 7,
    names: &DAYS,
    names_offset: 0,
};

impl Field {
    fn value(&self, s: &str) -> Result<u32, String> {
        let low = s.to_ascii_lowercase();
        if let Some(i) = self.names.iter().position(|n| *n == low) {
            return Ok(i as u32 + self.names_offset);
        }
        let v: u32 = s
            .parse()
            .map_err(|_| format!("invalid {} {s:?}", self.name))?;
        if v < self.min || v > self.max {
            return Err(format!(
                "{} {v} is out of range {}-{}",
                self.name, self.min, self.max
            ));
        }
        Ok(v)
    }

    /// The set of values of one cron field (`None`: every value, `*`).
    fn values(&self, f: &str) -> Result<Option<Vec<u32>>, String> {
        if f == "*" {
            return Ok(None);
        }
        let mut out = Vec::new();
        for item in f.split(',') {
            let (range, step) = match item.split_once('/') {
                Some((r, s)) => {
                    let s: u32 = s
                        .parse()
                        .ok()
                        .filter(|s| *s > 0)
                        .ok_or_else(|| format!("invalid step in {item:?}"))?;
                    (r, s)
                }
                None => (item, 1),
            };
            let (a, b) = if range == "*" {
                (self.min, if self.max == 7 { 6 } else { self.max })
            } else if let Some((a, b)) = range.split_once('-') {
                (self.value(a)?, self.value(b)?)
            } else {
                let a = self.value(range)?;
                // `a/n` (vixie cron): from a to the end
                (a, if step > 1 { self.max } else { a })
            };
            if a > b {
                return Err(format!("invalid range {range:?} in the {}", self.name));
            }
            out.extend((a..=b).step_by(step as usize));
        }
        if self.max == 7 {
            // 7 is Sunday too
            for v in &mut out {
                if *v == 7 {
                    *v = 0;
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(Some(out))
    }

    /// The systemd form of a cron field.
    fn systemd(&self, f: &str) -> Result<String, String> {
        // `*/n` keeps its compact systemd form (`0/15`)
        if let Some(step) = f.strip_prefix("*/")
            && self.max != 7
        {
            let s: u32 = step
                .parse()
                .ok()
                .filter(|s| *s > 0)
                .ok_or_else(|| format!("invalid step in {f:?}"))?;
            let start = if self.min == 0 { 0 } else { 1 };
            return Ok(format!("{start:02}/{s}"));
        }
        Ok(match self.values(f)? {
            None => "*".into(),
            Some(v) => v
                .iter()
                .map(|x| format!("{x:02}"))
                .collect::<Vec<_>>()
                .join(","),
        })
    }
}

/// Converts a crontab schedule (five fields or an `@` shortcut) to a systemd trigger.
/// Refuses what has no exact equivalent: a day of month and a day of week both restricted
/// (cron runs when either matches, systemd when both do).
pub fn cron_to_calendar(expr: &str) -> Result<Converted, String> {
    let e = expr.trim();
    let shortcut = match e {
        "@reboot" => return Ok(Converted::Boot),
        "@yearly" | "@annually" => Some("*-01-01 00:00:00"),
        "@monthly" => Some("*-*-01 00:00:00"),
        "@weekly" => Some("Sun *-*-* 00:00:00"),
        "@daily" | "@midnight" => Some("*-*-* 00:00:00"),
        "@hourly" => Some("*-*-* *:00:00"),
        _ if e.starts_with('@') => return Err(format!("unknown cron shortcut {e:?}")),
        _ => None,
    };
    if let Some(s) = shortcut {
        return Ok(Converted::Calendar(s.into()));
    }
    let f: Vec<&str> = e.split_whitespace().collect();
    if f.len() != 5 {
        return Err(
            "a cron schedule has five fields: minute hour day-of-month month day-of-week".into(),
        );
    }
    let (min, hour, dom, mon, dow) = (f[0], f[1], f[2], f[3], f[4]);
    let dow_set = DOW.values(dow)?;
    let dom_set = DOM.values(dom)?;
    if dow_set.as_ref().is_some_and(|d| d.len() < 7) && dom_set.is_some() {
        return Err(
            "cron runs when the day of month OR the day of week matches: no exact systemd equivalent, use two tasks"
                .into(),
        );
    }
    let mut out = String::new();
    if let Some(days) = dow_set.filter(|d| d.len() < 7) {
        out.push_str(
            &days
                .iter()
                .map(|d| DAY_NAMES[*d as usize])
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push(' ');
    }
    out.push_str(&format!(
        "*-{}-{} {}:{}:00",
        MONTH.systemd(mon)?,
        DOM.systemd(dom)?,
        HOUR.systemd(hour)?,
        MINUTE.systemd(min)?
    ));
    Ok(Converted::Calendar(out))
}

/// One expression as `systemd-analyze calendar` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Analyzed {
    pub original: String,
    pub normalized: String,
    /// Next elapses, milliseconds since the epoch (UTC).
    pub next_ms: Vec<u64>,
}

/// `Thu 2026-11-05 01:30:00 UTC` (the weekday is optional) to epoch milliseconds; only UTC
/// times are read (systemd prints an `(in UTC):` line when the local zone is another).
fn parse_utc(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let (date, time, zone) = match parts.as_slice() {
        [_, d, t, z] | [d, t, z] => (*d, *t, *z),
        _ => return None,
    };
    if zone != "UTC" {
        return None;
    }
    let d: Vec<u32> = date.split('-').filter_map(|x| x.parse().ok()).collect();
    let t: Vec<u32> = time.split(':').filter_map(|x| x.parse().ok()).collect();
    if d.len() != 3 || t.len() != 3 {
        return None;
    }
    let days = days_from_civil(d[0] as i32, d[1], d[2]);
    let s = days * 86400 + (t[0] * 3600 + t[1] * 60 + t[2]) as i64;
    u64::try_from(s).ok().map(|s| s * 1000)
}

fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parses `systemd-analyze calendar --iterations=N EXPR...` output, one block per
/// expression.
pub fn parse_analyze(out: &str) -> Vec<Analyzed> {
    let mut res: Vec<Analyzed> = Vec::new();
    // the elapse waiting for a possible `(in UTC):` line
    let mut pending: Option<String> = None;
    let flush = |res: &mut Vec<Analyzed>, p: &mut Option<String>| {
        if let (Some(v), Some(a)) = (p.take(), res.last_mut())
            && let Some(ms) = parse_utc(&v)
        {
            a.next_ms.push(ms);
        }
    };
    for line in out.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "Original form" => {
                flush(&mut res, &mut pending);
                res.push(Analyzed {
                    original: v.into(),
                    ..Default::default()
                });
            }
            // "Original form" is only printed when it differs from the normalized one
            "Normalized form" => match res.last_mut() {
                Some(a) if a.normalized.is_empty() && a.next_ms.is_empty() && pending.is_none() => {
                    a.normalized = v.into()
                }
                _ => {
                    flush(&mut res, &mut pending);
                    res.push(Analyzed {
                        original: v.into(),
                        normalized: v.into(),
                        ..Default::default()
                    });
                }
            },
            "Next elapse" => {
                flush(&mut res, &mut pending);
                pending = (v != "never").then(|| v.to_string());
            }
            k if k.starts_with("Iteration #") => {
                flush(&mut res, &mut pending);
                pending = Some(v.to_string());
            }
            "(in UTC)" => {
                pending = None;
                if let (Some(a), Some(ms)) = (res.last_mut(), parse_utc(v)) {
                    a.next_ms.push(ms);
                }
            }
            _ => {}
        }
    }
    flush(&mut res, &mut pending);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal(e: &str) -> String {
        match cron_to_calendar(e).unwrap() {
            Converted::Calendar(c) => c,
            Converted::Boot => "boot".into(),
        }
    }

    #[test]
    fn converts_cron() {
        assert_eq!(cal("0 4 * * *"), "*-*-* 04:00:00");
        assert_eq!(cal("*/15 * * * *"), "*-*-* *:00/15:00");
        assert_eq!(cal("30 2 * * 1-5"), "Mon,Tue,Wed,Thu,Fri *-*-* 02:30:00");
        assert_eq!(cal("0 3 1 * *"), "*-*-01 03:00:00");
        assert_eq!(cal("0 0 * jan,jul sun"), "Sun *-01,07-* 00:00:00");
        assert_eq!(cal("5,35 */6 * * 0,7"), "Sun *-*-* 00/6:05,35:00");
        assert_eq!(cal("0 8-18/2 * * *"), "*-*-* 08,10,12,14,16,18:00:00");
        assert_eq!(cal("0 0 * * *"), "*-*-* 00:00:00");
        assert_eq!(
            cal("0 0 * * 0-6"),
            "*-*-* 00:00:00",
            "every weekday is no restriction"
        );
        assert_eq!(cal("@daily"), "*-*-* 00:00:00");
        assert_eq!(cal("@weekly"), "Sun *-*-* 00:00:00");
        assert_eq!(cal("@reboot"), "boot");
        for bad in [
            "0 4 * *",
            "61 * * * *",
            "0 4 1 * 1",
            "* * * * 8",
            "*/0 * * * *",
            "5-1 * * * *",
            "@sometimes",
            "a b c d e",
        ] {
            assert!(cron_to_calendar(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn plausible_expressions() {
        for ok in [
            "hourly",
            "Mon..Fri 02:30",
            "*-*-* 04:00:00 Europe/Rome",
            "*:0/15",
            "Sat,Sun 1~3",
        ] {
            assert!(plausible_calendar(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "daily\nExecStart=/bin/sh",
            "%h",
            "$X",
            "a\"b",
            &"x".repeat(300),
        ] {
            assert!(plausible_calendar(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn parses_systemd_analyze() {
        let utc = "  Original form: Mon..Fri 02:30
Normalized form: Mon..Fri *-*-* 02:30:00
    Next elapse: Thu 2026-11-05 02:30:00 UTC
       From now: 51min left
   Iteration #2: Fri 2026-11-06 02:30:00 UTC
       From now: 24h left

  Original form: daily
Normalized form: *-*-* 00:00:00
    Next elapse: Fri 2026-11-06 00:00:00 UTC
       From now: 22h left
";
        let r = parse_analyze(utc);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].normalized, "Mon..Fri *-*-* 02:30:00");
        assert_eq!(r[0].next_ms, [1_793_845_800_000, 1_793_932_200_000]);
        assert_eq!(r[1].next_ms, [1_793_923_200_000]);

        let rome = "  Original form: 03:00
Normalized form: *-*-* 03:00:00
    Next elapse: Fri 2026-11-06 03:00:00 CET
       (in UTC): Fri 2026-11-06 02:00:00 UTC
       From now: 23h left
   Iteration #2: Sat 2026-11-07 03:00:00 CET
       (in UTC): Sat 2026-11-07 02:00:00 UTC
       From now: 1 day 23h left
";
        let r = parse_analyze(rome);
        assert_eq!(r[0].next_ms, [1_793_930_400_000, 1_794_016_800_000]);
        let same = "Normalized form: *-*-* *:00/30:00\n    Next elapse: Thu 2026-11-05 02:30:00 UTC\n       From now: 15min left\n\nNormalized form: *-*-* 00:00:00\n    Next elapse: Fri 2026-11-06 00:00:00 UTC\n";
        let r = parse_analyze(same);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].original, "*-*-* *:00/30:00");
        assert_eq!(r[0].next_ms, [1_793_845_800_000]);
        assert_eq!(r[1].next_ms, [1_793_923_200_000]);
        let never = "  Original form: 2020-01-01\nNormalized form: 2020-01-01 00:00:00\n    Next elapse: never\n";
        assert!(parse_analyze(never)[0].next_ms.is_empty());
    }
}
