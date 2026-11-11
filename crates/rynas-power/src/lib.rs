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
/// Read from the file every time: jiff keeps the system zone for five minutes, and a zone
/// changed in System > Date and time must count at once (D-TIME).
pub fn system_tz() -> jiff::tz::TimeZone {
    const LOCALTIME: &str = "/etc/localtime";
    if std::env::var_os("TZ").is_none()
        && let Some(name) = std::fs::read_link(LOCALTIME).ok().as_deref().and_then(zone_name)
        && let Ok(data) = std::fs::read(LOCALTIME)
        && let Ok(tz) = jiff::tz::TimeZone::tzif(&name, &data)
    {
        return tz;
    }
    jiff::tz::TimeZone::system()
}

/// The IANA name in the target of `/etc/localtime` (`../usr/share/zoneinfo/Europe/Rome`).
pub fn zone_name(target: &std::path::Path) -> Option<String> {
    let s = target.to_str()?;
    let (_, name) = s.rsplit_once("zoneinfo/")?;
    let name = name.strip_prefix("posix/").unwrap_or(name);
    (!name.is_empty() && !name.contains("..")).then(|| name.to_string())
}

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
