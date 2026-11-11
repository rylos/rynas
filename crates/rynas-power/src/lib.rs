//! The rynas Power Schedule (DSM model, `docs/plugins/synology.md`): weekly rules that start
//! up or shut down the NAS.
//!
//! - [`rules`]: validation, conflicts, next occurrences in local time (DST-safe, jiff);
//! - [`policy`]: shutdown pre-checks (RAID, SMB open files) and the policy decision;
//! - [`wake`]: the RTC wake alarm (`rtcwake -m no`), the boot record;
//! - [`domain`]: the `power.schedule` configuration domain;
//! - [`manual`]: manual restart and shut down (logind's schedule, `shutdown` arguments, apt
//!   processes).
//!
//! Startup needs an RTC with a `wakealarm` (any Debian machine whose firmware honours it;
//! on Synology synohwd also tells the microcontroller); shutdown works everywhere.

pub mod domain;
pub mod manual;
pub mod policy;
pub mod rules;
pub mod wake;

pub use domain::{POWER_KEY, PowerDomain};

/// The time zone the rules are read in: the system's (`/etc/localtime`, `TZ`).
///
/// The shared cache of [`rynas_tz`]: read again as soon as `/etc/localtime` changes (jiff
/// alone keeps the system zone for five minutes), so a zone changed in System > Date and time
/// counts at once (D-TIME).
pub fn system_tz() -> jiff::tz::TimeZone {
    rynas_tz::system_tz()
}

pub use rynas_tz::zone_name;

/// IANA name of a zone, "UTC" when unknown.
pub fn tz_name(tz: &jiff::tz::TimeZone) -> String {
    tz.iana_name().unwrap_or("UTC").to_string()
}

#[cfg(test)]
mod tests {
    use super::zone_name;
    use std::path::Path;

    #[test]
    fn zone_names_from_the_link() {
        let n = |p: &str| zone_name(Path::new(p));
        assert_eq!(n("../usr/share/zoneinfo/Europe/Rome").as_deref(), Some("Europe/Rome"));
        assert_eq!(n("/usr/share/zoneinfo/UTC").as_deref(), Some("UTC"));
        assert_eq!(n("/usr/share/zoneinfo/posix/Asia/Tokyo").as_deref(), Some("Asia/Tokyo"));
        assert_eq!(n("/etc/zone"), None);
        assert_eq!(n("/usr/share/zoneinfo/"), None);
    }
}
