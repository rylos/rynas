//! The rynas Power Schedule (DSM model, `docs/plugins/synology.md`): weekly rules that start
//! up or shut down the NAS.
//!
//! - [`rules`]: validation, conflicts, next occurrences in local time (DST-safe, jiff);
//! - [`policy`]: shutdown pre-checks (RAID, SMB open files) and the policy decision;
//! - [`wake`]: the RTC wake alarm (`rtcwake -m no`), the boot record;
//! - [`domain`]: the `power.schedule` configuration domain.
//!
//! Startup needs an RTC with a `wakealarm` (any Debian machine whose firmware honours it;
//! on Synology synohwd also tells the microcontroller); shutdown works everywhere.

pub mod domain;
pub mod policy;
pub mod rules;
pub mod wake;

pub use domain::{POWER_KEY, PowerDomain};

/// The time zone the rules are read in: the system's (`/etc/localtime`, `TZ`).
pub fn system_tz() -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::system()
}

/// IANA name of a zone, "UTC" when unknown.
pub fn tz_name(tz: &jiff::tz::TimeZone) -> String {
    tz.iana_name().unwrap_or("UTC").to_string()
}
