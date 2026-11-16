//! The Task Scheduler's calendar builder (moved unchanged from the web interface): what the
//! "Repeat" fields edit, to and from the `OnCalendar=` (or cron) text.

/// English short weekday names, Monday first.
pub const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repeat {
    Minutes,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Custom,
    Cron,
}

impl Repeat {
    pub const ALL: [Repeat; 7] =
        [Repeat::Minutes, Repeat::Hourly, Repeat::Daily, Repeat::Weekly, Repeat::Monthly, Repeat::Custom, Repeat::Cron];

    pub fn code(self) -> &'static str {
        match self {
            Repeat::Minutes => "minutes",
            Repeat::Hourly => "hourly",
            Repeat::Daily => "daily",
            Repeat::Weekly => "weekly",
            Repeat::Monthly => "monthly",
            Repeat::Custom => "custom",
            Repeat::Cron => "cron",
        }
    }

    /// Unknown codes are `Daily`.
    pub fn parse(s: &str) -> Repeat {
        Repeat::ALL.into_iter().find(|r| r.code() == s).unwrap_or(Repeat::Daily)
    }
}

/// The syntax of [`Cal::expression`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Syntax {
    Calendar,
    Cron,
}

/// What the calendar builder edits; [`Cal::expression`] turns it into `OnCalendar=` (or cron).
#[derive(Clone, PartialEq, Debug)]
pub struct Cal {
    pub repeat: Repeat,
    pub every_min: u32,
    pub minute: u32,
    /// "HH:MM".
    pub time: String,
    /// 1 = Monday … 7 = Sunday.
    pub days: Vec<u8>,
    pub dom: u32,
    pub custom: String,
    pub cron: String,
}

impl Default for Cal {
    fn default() -> Self {
        Cal { repeat: Repeat::Daily, every_min: 15, minute: 0, time: "03:00".into(), days: vec![1], dom: 1, custom: String::new(), cron: String::new() }
    }
}

/// "H:M" or "HH:MM" (an optional ":00" seconds) → "HH:MM".
pub fn hhmm(s: &str) -> Option<String> {
    let mut p = s.split(':');
    let h: u32 = p.next()?.parse().ok()?;
    let m: u32 = p.next()?.parse().ok()?;
    match p.next() {
        None | Some("00") | Some("0") => {}
        Some(_) => return None,
    }
    (h < 24 && m < 60).then(|| format!("{h:02}:{m:02}"))
}

impl Cal {
    /// The expression to preview and store, with its syntax.
    pub fn expression(&self) -> (String, Syntax) {
        let time = hhmm(&self.time).unwrap_or_else(|| "00:00".into());
        let e = match self.repeat {
            Repeat::Minutes => format!("*-*-* *:00/{:02}:00", self.every_min.clamp(1, 59)),
            Repeat::Hourly => format!("*-*-* *:{:02}:00", self.minute.min(59)),
            Repeat::Daily => format!("*-*-* {time}:00"),
            Repeat::Weekly if !self.days.is_empty() => {
                let mut d = self.days.clone();
                d.sort_unstable();
                d.dedup();
                let names: Vec<&str> = d.iter().filter(|x| (1..=7).contains(*x)).map(|x| WEEKDAYS[*x as usize - 1]).collect();
                format!("{} *-*-* {time}:00", names.join(","))
            }
            Repeat::Weekly => format!("*-*-* {time}:00"),
            Repeat::Monthly => format!("*-*-{:02} {time}:00", self.dom.clamp(1, 31)),
            Repeat::Custom => self.custom.trim().to_string(),
            Repeat::Cron => return (self.cron.trim().to_string(), Syntax::Cron),
        };
        (e, Syntax::Calendar)
    }

    /// Recognize the expressions the builder writes (and systemd's shortcuts); anything else
    /// opens as a custom expression.
    pub fn parse(expr: &str) -> Cal {
        let e = expr.trim();
        let mut c = Cal::default();
        let custom = |c: Cal| Cal { repeat: Repeat::Custom, custom: e.to_string(), ..c };
        match e {
            "hourly" => return Cal { repeat: Repeat::Hourly, ..c },
            "daily" => return Cal { repeat: Repeat::Daily, time: "00:00".into(), ..c },
            "weekly" => return Cal { repeat: Repeat::Weekly, time: "00:00".into(), ..c },
            "monthly" => return Cal { repeat: Repeat::Monthly, time: "00:00".into(), ..c },
            "" => return c,
            _ => {}
        }
        let parts: Vec<&str> = e.split_whitespace().collect();
        let (days, date, time) = match parts.as_slice() {
            [date, time] => (None, *date, *time),
            [days, date, time] => (Some(*days), *date, *time),
            _ => return custom(c),
        };
        if let Some(rest) = time.strip_prefix("*:") {
            if days.is_some() || date != "*-*-*" {
                return custom(c);
            }
            if let Some(n) = rest.strip_prefix("00/").or_else(|| rest.strip_prefix("0/")) {
                let n = n.trim_end_matches(":00");
                if let Ok(n) = n.parse::<u32>() {
                    return Cal { repeat: Repeat::Minutes, every_min: n, ..c };
                }
            } else if let Ok(m) = rest.trim_end_matches(":00").parse::<u32>()
                && rest.len() <= 5
            {
                return Cal { repeat: Repeat::Hourly, minute: m, ..c };
            }
            return custom(c);
        }
        let Some(t) = hhmm(time) else { return custom(c) };
        c.time = t;
        if let Some(days) = days {
            if date != "*-*-*" {
                return custom(c);
            }
            let mut out = Vec::new();
            for d in days.split(',') {
                match WEEKDAYS.iter().position(|w| w.eq_ignore_ascii_case(d)) {
                    Some(i) => out.push(i as u8 + 1),
                    None => return custom(c),
                }
            }
            return Cal { repeat: Repeat::Weekly, days: out, ..c };
        }
        if date == "*-*-*" {
            return Cal { repeat: Repeat::Daily, ..c };
        }
        if let Some(dd) = date.strip_prefix("*-*-")
            && let Ok(d) = dd.parse::<u32>()
            && (1..=31).contains(&d)
        {
            return Cal { repeat: Repeat::Monthly, dom: d, ..c };
        }
        custom(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(c: Cal) {
        let (e, _) = c.expression();
        assert_eq!(Cal::parse(&e), c, "{e}");
    }

    #[test]
    fn calendar_builder_roundtrips() {
        roundtrip(Cal { repeat: Repeat::Daily, time: "02:30".into(), ..Cal::default() });
        roundtrip(Cal { repeat: Repeat::Weekly, days: vec![1, 5], time: "03:00".into(), ..Cal::default() });
        roundtrip(Cal { repeat: Repeat::Monthly, dom: 15, time: "04:05".into(), ..Cal::default() });
        roundtrip(Cal { repeat: Repeat::Hourly, minute: 7, ..Cal::default() });
        roundtrip(Cal { repeat: Repeat::Minutes, every_min: 15, ..Cal::default() });
    }

    #[test]
    fn calendar_expressions() {
        let c = Cal { repeat: Repeat::Weekly, days: vec![5, 1, 1], time: "3:00".into(), ..Cal::default() };
        assert_eq!(c.expression().0, "Mon,Fri *-*-* 03:00:00");
        assert_eq!(Cal { repeat: Repeat::Monthly, dom: 1, time: "04:00".into(), ..Cal::default() }.expression().0, "*-*-01 04:00:00");
        assert_eq!(Cal { repeat: Repeat::Cron, cron: " 0 4 * * * ".into(), ..Cal::default() }.expression(), ("0 4 * * *".into(), Syntax::Cron));
        // shortcuts and systemd's own forms
        assert_eq!(Cal::parse("hourly").repeat, Repeat::Hourly);
        assert_eq!(Cal::parse("daily").time, "00:00");
        assert_eq!(Cal::parse("*-*-* 02:30").time, "02:30");
        assert_eq!(Cal::parse("*:0/10").repeat, Repeat::Custom); // no date part: kept as written
        let odd = Cal::parse("Mon..Fri 02:30");
        assert_eq!((odd.repeat, odd.custom.as_str()), (Repeat::Custom, "Mon..Fri 02:30"));
        assert_eq!(Cal::parse("Sat,Sun *-*-* 09:15:00").days, vec![6, 7]);
    }

    #[test]
    fn repeat_codes() {
        for r in Repeat::ALL {
            assert_eq!(Repeat::parse(r.code()), r);
        }
        assert_eq!(Repeat::parse("nope"), Repeat::Daily);
        assert_eq!(hhmm("3:5"), Some("03:05".into()));
        assert_eq!(hhmm("03:05:00"), Some("03:05".into()));
        assert_eq!(hhmm("03:05:30"), None);
        assert_eq!(hhmm("24:00"), None);
    }
}
