//! UPS (`/api/v1/ups...`), D-UPS: a thin layer over NUT (any Debian with the `nut` package)
//! and, where synodeb's UPS layer is installed, over `synodeb-ups` ([`UpsBackend`]).
//!
//! - [`UpsStatus`]: what upsd (`127.0.0.1:3493`, plain-text protocol) reports, read by
//!   rynas-web every few seconds and pushed on `/api/v1/events` as `ups.status`.
//! - [`UpsSettings`]: the configuration key `ups.safemode`: the extra "after N minutes on
//!   battery" trigger and the time to stop work before it. Low battery and a critical UPS
//!   (OB+LB) always end in a shutdown (NUT's upsmon) or Safe Mode (synodeb), whichever first.
//! - [`BatteryTestRequest`]: `POST /api/v1/ups/test` sends the NUT instant command of the test
//!   (`test.battery.start.quick|deep`, `test.battery.stop`; through `synodeb-ups test <kind>`
//!   on synodeb) as the rynasd task `ups.test`.

use serde::{Deserialize, Serialize};

/// Whether upsd answered and has a UPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpsLink {
    /// upsd answered with at least one UPS.
    Connected,
    /// upsd answered without any UPS.
    NotConfigured,
    /// Nothing listens on the upsd port, or it did not answer in time.
    #[default]
    Unreachable,
}

/// One reading of the first UPS upsd knows. Values the UPS does not report are `null`.
/// Voltages in volts (`_v`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UpsStatus {
    pub link: UpsLink,
    /// NUT name (`ups` on synodeb).
    pub name: Option<String>,
    /// `ups.status` split on spaces: `OL`, `OB`, `LB`, `CHRG`, `DISCHRG`, `RB`, `FSD`,
    /// `BYPASS`, `CAL`, `OVER`, `TRIM`, `BOOST`, `OFF`...
    #[serde(default)]
    pub flags: Vec<String>,
    pub battery_charge_pct: Option<f32>,
    pub battery_runtime_s: Option<u64>,
    pub load_pct: Option<f32>,
    pub input_voltage_v: Option<f32>,
    pub output_voltage_v: Option<f32>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    /// `ups.beeper.status`: `enabled`, `disabled`, `muted`.
    pub beeper: Option<String>,
    /// Battery install or manufacture date as the UPS reports it (`battery.date`,
    /// `battery.mfr.date`); free text, not parsed.
    pub battery_date: Option<String>,
    /// `ups.test.result` (e.g. "Done and passed", "In progress").
    pub test_result: Option<String>,
    /// `driver.name` (e.g. `usbhid-ups`).
    pub driver: Option<String>,
    /// The battery test instant commands the UPS offers (`LIST CMD`, only the
    /// `test.battery.*` ones; see [`BatteryTest::nut_cmd`]).
    #[serde(default)]
    pub commands: Vec<String>,
    pub read_at_ms: u64,
}

impl UpsStatus {
    pub fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }

    pub fn on_battery(&self) -> bool {
        self.has("OB")
    }

    pub fn low_battery(&self) -> bool {
        self.has("LB")
    }

    /// Forced shutdown, or on battery with low battery: the shutdown is about to start.
    pub fn critical(&self) -> bool {
        self.has("FSD") || (self.on_battery() && self.low_battery())
    }

    /// The UPS offers this battery test.
    pub fn supports(&self, test: BatteryTest) -> bool {
        self.commands.iter().any(|c| c == test.nut_cmd())
    }
}

/// Who carries out the UPS policy and the battery tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpsBackend {
    /// Plain NUT (`/etc/nut` exists): rynasd sends the tests to upsd with its own upsd user
    /// and, after the configured time on battery, stops its work and asks upsmon to shut the
    /// system down.
    Nut,
    /// synodeb's UPS layer (`/etc/synodeb/ups.conf`): `synodeb-ups` runs tests and Safe Mode.
    Synodeb,
    /// NUT is not installed.
    #[default]
    NotInstalled,
}

/// Default time to stop work before the shutdown (synodeb's `PRE_SAFEMODE_TIMEOUT`).
pub const PRE_SAFEMODE_TIMEOUT_DEFAULT_S: u32 = 120;
/// Accepted range of `SAFEMODE_AFTER` when set (1 minute .. 24 hours).
pub const SAFEMODE_AFTER_RANGE_S: (u32, u32) = (60, 86_400);
/// Accepted range of `PRE_SAFEMODE_TIMEOUT`.
pub const PRE_SAFEMODE_TIMEOUT_RANGE_S: (u32, u32) = (10, 900);

/// The `ups.safemode` configuration key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsSettings {
    /// Shutdown (Safe Mode on synodeb) after this long on battery (DSM "after N minutes");
    /// `null`: only on low battery.
    #[serde(default)]
    pub safemode_after_s: Option<u32>,
    /// Time to stop work before it (rynas stops its tasks there; on synodeb the time of all
    /// the pre-Safe Mode hooks together).
    #[serde(default = "default_pre_timeout")]
    pub pre_safemode_timeout_s: u32,
}

fn default_pre_timeout() -> u32 {
    PRE_SAFEMODE_TIMEOUT_DEFAULT_S
}

impl Default for UpsSettings {
    fn default() -> Self {
        Self {
            safemode_after_s: None,
            pre_safemode_timeout_s: PRE_SAFEMODE_TIMEOUT_DEFAULT_S,
        }
    }
}

/// `GET /api/v1/ups/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsSettingsState {
    /// The settings can be changed (`backend` is not `not_installed`); otherwise they are
    /// read-only defaults and `PUT` answers 404.
    pub available: bool,
    #[serde(default)]
    pub backend: UpsBackend,
    pub applied: UpsSettings,
    /// The session user's staged value, when it differs.
    pub pending: Option<UpsSettings>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatteryTest {
    Quick,
    Deep,
    Stop,
}

impl BatteryTest {
    pub const ALL: [BatteryTest; 3] = [BatteryTest::Quick, BatteryTest::Deep, BatteryTest::Stop];

    pub fn arg(self) -> &'static str {
        match self {
            BatteryTest::Quick => "quick",
            BatteryTest::Deep => "deep",
            BatteryTest::Stop => "stop",
        }
    }

    /// The test of a `quick|deep|stop` argument.
    pub fn from_arg(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.arg() == s)
    }

    /// The NUT instant command.
    pub fn nut_cmd(self) -> &'static str {
        match self {
            BatteryTest::Quick => "test.battery.start.quick",
            BatteryTest::Deep => "test.battery.start.deep",
            BatteryTest::Stop => "test.battery.stop",
        }
    }
}

/// `POST /api/v1/ups/test` → 202 `TaskStarted` (task type `ups.test`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatteryTestRequest {
    pub test: BatteryTest,
}

/// `POST /v1/ups/safe-mode` (internal, root only): synodeb's pre-Safe Mode hook. On plain NUT
/// rynasd does the same by itself when the UPS turns critical or the timer expires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeModeRequest {
    /// synodeb's reason: `lowbatt`, `waittimeup`, `nocomm`, `fsd`, `shutdowncmd`.
    pub reason: String,
}

/// What the pre-shutdown stop did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SafeModeReport {
    /// rynasd tasks cancelled (ids).
    pub cancelled_tasks: Vec<String>,
    /// Scheduler task units stopped.
    pub stopped_units: Vec<String>,
}

/// synodeb's Safe Mode reasons (rynasd's own watcher uses `lowbatt`, `fsd`, `waittimeup`).
pub const SAFE_MODE_REASONS: &[&str] = &["lowbatt", "waittimeup", "nocomm", "fsd", "shutdowncmd"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_and_commands() {
        let u = UpsStatus {
            commands: vec![
                "test.battery.start.quick".into(),
                "test.battery.stop".into(),
            ],
            flags: vec!["OB".into(), "LB".into()],
            ..Default::default()
        };
        assert!(u.supports(BatteryTest::Quick) && u.supports(BatteryTest::Stop));
        assert!(!u.supports(BatteryTest::Deep));
        assert!(u.critical());
        assert_eq!(BatteryTest::from_arg("deep"), Some(BatteryTest::Deep));
        assert_eq!(BatteryTest::from_arg("x"), None);
        // an older reading without the new fields still parses
        let s: UpsSettingsState =
            serde_json::from_str(r#"{"available":true,"applied":{},"pending":null}"#).unwrap();
        assert_eq!(s.backend, UpsBackend::NotInstalled);
    }
}
