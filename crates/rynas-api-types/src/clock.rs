//! Date, time and time zone of the system (`/api/v1/system/time`), administrators only
//! (D-TIME, `docs/phase2/date-time.md`).
//!
//! The system is the saved state: the zone is `/etc/localtime` (set through timedated), the
//! servers a drop-in of the time daemon, automatic sync the daemon's unit. Nothing of it lives
//! in rynas's configuration.

use serde::{Deserialize, Serialize};

/// The time daemon that keeps the clock in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TimeDaemon {
    /// `systemd-timesyncd` (a client only).
    Timesyncd,
    /// chrony.
    Chrony,
    /// Another daemon (ntpsec, OpenNTPD: `/usr/sbin/ntpd`): sync and servers are managed
    /// outside rynas and shown read-only; rynas never installs a daemon over it.
    Other,
    /// None installed: turning sync on installs `systemd-timesyncd`.
    #[default]
    None,
}

/// What the daemon says about its source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TimeSource {
    /// Name of the server in use (or its address when no name is known).
    pub server: String,
    /// Its address, when the daemon tells it.
    pub address: Option<String>,
    pub stratum: Option<u8>,
    /// Source time minus system time, in microseconds (positive: the clock is behind).
    pub offset_us: Option<i64>,
    /// When the clock was last corrected from it (ms since the epoch).
    pub last_sync_ms: Option<u64>,
    /// Seconds between two questions to it.
    pub poll_s: Option<u32>,
}

/// `GET /system/time`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ClockState {
    /// The server's clock when it answered (ms since the epoch).
    pub now_ms: u64,
    /// IANA name, e.g. `Europe/Rome`.
    pub timezone: String,
    /// Offset of the zone now, in seconds east of UTC.
    pub utc_offset_s: i32,
    /// The hardware clock keeps local time instead of UTC (a warning: DST changes and dual
    /// boot make it drift by hours).
    pub local_rtc: bool,
    pub daemon: TimeDaemon,
    /// Automatic sync is on (the daemon's unit runs).
    pub sync_enabled: bool,
    /// The kernel counts the clock as synchronized.
    pub synchronized: bool,
    /// rynas's own server list; empty: the distribution's servers.
    pub servers: Vec<String>,
    /// The distribution's servers (used while [`ClockState::servers`] is empty; with chrony
    /// always used besides them).
    pub default_servers: Vec<String>,
    /// Servers handed out by DHCP to the network interfaces, used besides the others (with
    /// timesyncd asked first, even when rynas has its own list).
    pub dhcp_servers: Vec<String>,
    /// Servers DHCP handed out through the DHCP client's hook into the server list
    /// (timesyncd with dhclient): used only while [`ClockState::servers`] is empty, rynas's
    /// own list replaces them.
    #[serde(default)]
    pub dhcp_hook_servers: Vec<String>,
    /// chrony's configuration does not read the folder rynas writes its servers to
    /// (`sourcedir /etc/chrony/sources.d` missing): rynas's servers are ignored.
    #[serde(default)]
    pub servers_ignored: bool,
    pub source: Option<TimeSource>,
}

/// `POST /system/time` (task `time.apply`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ClockSettings {
    pub timezone: String,
    pub sync_enabled: bool,
    /// At most [`MAX_SERVERS`] host names or addresses; empty: the distribution's servers.
    pub servers: Vec<String>,
    /// `false` moves a hardware clock kept in local time to UTC; `true` is accepted only
    /// when it already is local (nothing changes).
    pub local_rtc: bool,
}

/// `POST /system/time/set` (task `time.set`), only while automatic sync is off. Exactly one
/// of the two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SetClock {
    /// An instant (the browser's clock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch_ms: Option<u64>,
    /// A wall time in the system's zone, `YYYY-MM-DD HH:MM:SS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
}

/// One entry of `GET /system/time/zones`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneInfo {
    pub name: String,
    /// Offset now, in seconds east of UTC.
    pub offset_s: i32,
}

pub const MAX_SERVERS: usize = 8;
/// The years a manual setting accepts (a clock outside them is a typing slip, and certificate
/// checks and schedules would break).
pub const MIN_YEAR: i32 = 2000;
pub const MAX_YEAR: i32 = 2099;

/// `YYYY-MM-DD HH:MM:SS` (or without seconds), the shape [`SetClock::local`] takes.
pub fn local_time_ok(s: &str) -> bool {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| b.get(r).is_some_and(|x| x.iter().all(u8::is_ascii_digit));
    let base = b.len() >= 16
        && digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && b[10] == b' '
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16);
    base && (b.len() == 16 || (b.len() == 19 && b[16] == b':' && digits(17..19)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_times() {
        assert!(local_time_ok("2026-11-11 09:30:00"));
        assert!(local_time_ok("2026-11-11 09:30"));
        assert!(!local_time_ok("2026-11-11T09:30:00"));
        assert!(!local_time_ok("2026-11-11 09:30:00; reboot"));
        assert!(!local_time_ok("tomorrow"));
        assert!(!local_time_ok(""));
    }

    #[test]
    fn daemon_names() {
        assert_eq!(serde_json::to_string(&TimeDaemon::Timesyncd).unwrap(), "\"timesyncd\"");
        let s: SetClock = serde_json::from_str(r#"{"epoch_ms":5}"#).unwrap();
        assert_eq!((s.epoch_ms, s.local), (Some(5), None));
    }
}
