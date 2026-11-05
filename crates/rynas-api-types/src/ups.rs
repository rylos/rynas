//! UPS (`/api/v1/ups...`), D-UPS: a thin layer over NUT and, on synodeb, over `synodeb-ups`.
//!
//! - [`UpsStatus`]: what upsd (`127.0.0.1:3493`, plain-text protocol) reports, read by
//!   rynas-web every few seconds and pushed on `/api/v1/events` as `ups.status`.
//! - [`UpsSettings`]: the configuration key `ups.safemode` (synodeb only), the rynas-owned
//!   keys `SAFEMODE_AFTER` and `PRE_SAFEMODE_TIMEOUT` of `/etc/synodeb/ups.conf`. Safe Mode
//!   also always starts on NUT low battery and on a critical UPS (OB+LB), whichever comes first.
//! - [`BatteryTestRequest`]: `POST /api/v1/ups/test` runs `synodeb-ups test <kind>` as the
//!   rynasd task `ups.test`.

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
}

/// Default `PRE_SAFEMODE_TIMEOUT` of synodeb.
pub const PRE_SAFEMODE_TIMEOUT_DEFAULT_S: u32 = 120;
/// Accepted range of `SAFEMODE_AFTER` when set (1 minute .. 24 hours).
pub const SAFEMODE_AFTER_RANGE_S: (u32, u32) = (60, 86_400);
/// Accepted range of `PRE_SAFEMODE_TIMEOUT`.
pub const PRE_SAFEMODE_TIMEOUT_RANGE_S: (u32, u32) = (10, 900);

/// The `ups.safemode` configuration key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsSettings {
    /// Safe Mode after this long on battery (DSM "after N minutes"); `null`: only on low
    /// battery (`SAFEMODE_AFTER=0`).
    #[serde(default)]
    pub safemode_after_s: Option<u32>,
    /// Time for all the pre-Safe Mode hooks together (rynas stops its tasks there).
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
    /// `/etc/synodeb` exists: Safe Mode and the battery test are synodeb's. Elsewhere the
    /// settings are read-only defaults and `PUT` answers 404.
    pub available: bool,
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
    pub fn arg(self) -> &'static str {
        match self {
            BatteryTest::Quick => "quick",
            BatteryTest::Deep => "deep",
            BatteryTest::Stop => "stop",
        }
    }
}

/// `POST /api/v1/ups/test` → 202 `TaskStarted` (task type `ups.test`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatteryTestRequest {
    pub test: BatteryTest,
}

/// `POST /v1/ups/safe-mode` (internal, root only): the pre-Safe Mode hook of synodeb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeModeRequest {
    /// synodeb's reason: `lowbatt`, `waittimeup`, `nocomm`, `fsd`, `shutdowncmd`.
    pub reason: String,
}

/// What the pre-Safe Mode hook stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SafeModeReport {
    /// rynasd tasks cancelled (ids).
    pub cancelled_tasks: Vec<String>,
    /// Scheduler task units stopped.
    pub stopped_units: Vec<String>,
}

/// synodeb's Safe Mode reasons.
pub const SAFE_MODE_REASONS: &[&str] = &["lowbatt", "waittimeup", "nocomm", "fsd", "shutdowncmd"];
