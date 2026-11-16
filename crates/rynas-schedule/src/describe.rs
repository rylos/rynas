//! A schedule in words, as data: the interface turns a [`Description`] into a sentence in
//! the user's language (its own keys and weekday names), so nothing here is English text.

use crate::recurrence::{Recurrence, TimeOfDay};
use crate::spec::{CalendarSpec, Error};

/// What a calendar expression says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Description {
    /// One of the presets; render it from its fields (see [`day_span`] for the first and
    /// last run of the "every N" presets).
    Preset(Recurrence),
    /// A valid expression that no preset shows: show the normalized text.
    Advanced { normalized: String },
    /// Not accepted (or not supported here).
    Invalid(Error),
}

/// Describes an expression.
pub fn describe(expr: &str) -> Description {
    match CalendarSpec::parse(expr) {
        Ok(c) => match Recurrence::from_spec(&c) {
            Some(r) => Description::Preset(r),
            None => Description::Advanced { normalized: c.to_string() },
        },
        Err(e) => Description::Invalid(e),
    }
}

/// The first and last run of the day of [`Recurrence::Minutes`] and [`Recurrence::Hours`]
/// ("every 15 minutes, 08:00 to 18:45").
pub fn day_span(r: &Recurrence) -> Option<(TimeOfDay, TimeOfDay)> {
    match *r {
        Recurrence::Minutes { every, from, to, .. } if every > 0 => {
            Some((TimeOfDay::new(from, 0), TimeOfDay::new(to, 60 - every)))
        }
        Recurrence::Hours { every, minute, from, to, .. } if every > 0 => {
            let last = from + (to - from) / every * every;
            Some((TimeOfDay::new(from, minute), TimeOfDay::new(last, minute)))
        }
        _ => None,
    }
}

/// The whole day: no span worth showing.
pub fn all_day(r: &Recurrence) -> bool {
    matches!(*r, Recurrence::Minutes { from: 0, to: 23, .. } | Recurrence::Hours { from: 0, to: 23, .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recurrence::Weekdays;

    #[test]
    fn descriptions() {
        assert_eq!(describe("quarterly"), Description::Advanced { normalized: "*-01,04,07,10-01 00:00:00".into() });
        assert_eq!(describe("*-*-* 99:00"), Description::Invalid(Error::Invalid));
        let Description::Preset(r) = describe("Mon..Fri 08..18:00/15") else { panic!() };
        assert_eq!(r, Recurrence::Minutes { every: 15, from: 8, to: 18, days: Weekdays::MON_FRI });
        assert_eq!(day_span(&r), Some((TimeOfDay::new(8, 0), TimeOfDay::new(18, 45))));
        assert!(!all_day(&r));
        let Description::Preset(r) = describe("*-*-* 00/5:10") else { panic!() };
        assert_eq!(day_span(&r), Some((TimeOfDay::new(0, 10), TimeOfDay::new(20, 10))));
        assert!(all_day(&r));
        let Description::Preset(r) = describe("*-*-* 08..20/4:00") else { panic!() };
        assert_eq!(day_span(&r), Some((TimeOfDay::new(8, 0), TimeOfDay::new(20, 0))));
        assert_eq!(day_span(&Recurrence::Daily { days: Weekdays(0), times: vec![TimeOfDay::new(1, 0)] }), None);
    }
}
