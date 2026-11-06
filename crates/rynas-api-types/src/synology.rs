//! Synology hardware (`/api/v1/hardware/synology/...`): the `synology` provider over synodeb's
//! synohwd (D-SYNOPLUG, `docs/plugins/synology.md`).
//!
//! Configuration key `synology.hw` ([`SynologySettings`]) renders `/etc/synohwd.toml`; actions
//! (beep, identify) go to synohwd's command FIFO or the LED class devices; the status is
//! synohwd's `status.json` with the units of the API.

use serde::{Deserialize, Serialize};

/// DSM "Fan Speed Mode".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FanMode {
    /// DSM quiet mode (default).
    #[default]
    Low,
    /// DSM cool mode.
    High,
    /// Fans at 99 %.
    Full,
}

impl FanMode {
    pub const ALL: [FanMode; 3] = [FanMode::Low, FanMode::High, FanMode::Full];

    /// The value in `synohwd.toml`.
    pub fn as_str(self) -> &'static str {
        match self {
            FanMode::Low => "low",
            FanMode::High => "high",
            FanMode::Full => "full",
        }
    }
}

/// What the power button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PowerButtonAction {
    /// Clean shutdown (`systemctl poweroff`), long beep and blinking power LED like DSM.
    #[default]
    Poweroff,
    /// Write the press to the event list only.
    Log,
    Ignore,
}

/// What the reset button does (the uP reports a press, never its length).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResetButtonAction {
    #[default]
    Log,
    Ignore,
    /// Two presses within 10 s: network back to DHCP and a safe firewall (D-NET), handled
    /// by `rynasctl net reset-button` (synohwd's `command:` hook, reserved for rynas).
    NetworkRescue,
}

fn default_true() -> bool {
    true
}
fn default_period() -> u16 {
    20
}
fn default_hysteresis() -> u8 {
    3
}
fn default_brightness() -> u8 {
    255
}

/// The `synology.hw` configuration key: every setting of synohwd. Ranges are synohwd's
/// (`RANGES`/`CHOICES` in synohwd.py); rynas refuses what synohwd would clamp or replace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynologySettings {
    #[serde(default)]
    pub fan_mode: FanMode,
    /// Seconds between fan adjustments, 5..=300.
    #[serde(default = "default_period")]
    pub fan_period_s: u16,
    /// Floor of the duty from the tables, 0..=99 %.
    #[serde(default)]
    pub fan_min_duty_pct: u8,
    /// °C a sensor must drop below a step before the fans slow down, 0..=20.
    #[serde(default = "default_hysteresis")]
    pub hysteresis_c: u8,
    /// Power off at DSM's shutdown thresholds (see the fan curves).
    #[serde(default = "default_true")]
    pub thermal_shutdown: bool,
    #[serde(default)]
    pub power_button: PowerButtonAction,
    #[serde(default)]
    pub reset_button: ResetButtonAction,
    #[serde(default = "default_true")]
    pub beep_on_start: bool,
    #[serde(default = "default_true")]
    pub beep_on_error: bool,
    /// Green = disk present, blinking on I/O (kernel trigger `ataN`).
    #[serde(default = "default_true")]
    pub disk_leds: bool,
    /// Disk LED brightness (PWM, shared), 1..=255.
    #[serde(default = "default_brightness")]
    pub led_brightness: u8,
    /// Spin the bay disks down after this idle time (`hdparm -S`), 0 (never)..=330.
    #[serde(default)]
    pub disk_standby_minutes: u16,
    /// Wake-on-LAN (magic packet) on every `lan*` port.
    #[serde(default)]
    pub wol: bool,
    /// Power back on after an AC loss (DSM default: on).
    #[serde(default = "default_true")]
    pub ac_recovery: bool,
}

impl Default for SynologySettings {
    /// synohwd's `DEFAULTS`.
    fn default() -> Self {
        Self {
            fan_mode: FanMode::Low,
            fan_period_s: default_period(),
            fan_min_duty_pct: 0,
            hysteresis_c: default_hysteresis(),
            thermal_shutdown: true,
            power_button: PowerButtonAction::Poweroff,
            reset_button: ResetButtonAction::Log,
            beep_on_start: true,
            beep_on_error: true,
            disk_leds: true,
            led_brightness: default_brightness(),
            disk_standby_minutes: 0,
            wol: false,
            ac_recovery: true,
        }
    }
}

/// `GET/PUT /api/v1/hardware/synology/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SynologySettingsState {
    /// Applied by rynas, or read from `/etc/synohwd.toml` when rynas never applied it.
    pub applied: SynologySettings,
    /// The session user's staged settings.
    #[serde(default)]
    pub pending: Option<SynologySettings>,
    /// `/etc/synohwd.toml` was found with a `command:` button action rynas does not manage:
    /// applying replaces it (shown as a warning).
    #[serde(default)]
    pub foreign_commands: Vec<String>,
    pub platform: SynologyPlatform,
}

/// The machine as the provider sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SynologyPlatform {
    /// DMI product, e.g. "DS920+".
    pub model: String,
    pub bays: u32,
    pub fans: u32,
    /// `synohwd.service` is running (its status file is fresh).
    pub synohwd_running: bool,
}

/// `POST /api/v1/hardware/synology/beep`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BeepRequest {
    #[serde(default)]
    pub kind: BeepKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BeepKind {
    #[default]
    Short,
    Long,
}

/// `POST /api/v1/hardware/synology/mute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MuteResult {
    /// An audible alarm was stopped (synohwd's beeps are single, so never today).
    pub stopped_beep: bool,
    /// Open hardware notifications acknowledged (their reminders stop).
    pub acknowledged: u64,
}

/// Sensor classes of the fan curves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensorClass {
    Disk,
    Nvme,
    Cpu,
    Nic,
}

/// One step of a curve: at or above `threshold_c` the fans run at least at `duty_pct`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanPoint {
    pub threshold_c: u8,
    pub duty_pct: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanCurve {
    pub class: SensorClass,
    pub points: Vec<FanPoint>,
    /// synohwd powers off after two readings at or above this (when `thermal_shutdown`).
    pub shutdown_c: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanModeCurves {
    pub mode: FanMode,
    pub curves: Vec<FanCurve>,
}

/// A sensor reading as the fan controller sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensorReading {
    pub class: SensorClass,
    /// `cpu`, `bay1`, `nvme0`, `enp1s0`...
    pub sensor: String,
    pub temperature_c: Option<f32>,
}

/// `GET /api/v1/hardware/synology/fan-curves`: DSM's tables of this model (synohwd's
/// `FAN_TABLES` / `MODEL_FAN_TABLES`), the active mode and the current readings, for the chart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FanCurves {
    pub model: String,
    /// The applied (or imported) `fan_mode`.
    pub active_mode: FanMode,
    /// `low` and `high` (`full` has no table: 99 %).
    pub modes: Vec<FanModeCurves>,
    pub readings: Vec<SensorReading>,
    #[serde(default)]
    pub duty_pct: Option<u8>,
}

/// Front status LED.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusLed {
    Off,
    Green,
    GreenBlink,
    Orange,
    OrangeBlink,
}

/// Fan tachometer alarm (`fanN_alarm` of hwmon `synology`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanAlarm {
    Ok,
    Stopped,
    /// Not measured (yet), or the attribute is missing.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynologyFan {
    pub number: u32,
    #[serde(default)]
    pub rpm: Option<u32>,
    pub alarm: FanAlarm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynologyBay {
    pub number: u32,
    pub present: bool,
    pub powered: bool,
    /// Block device, e.g. "sda".
    #[serde(default)]
    pub device: Option<String>,
    /// Last known value (kept by synohwd while the disk sleeps).
    #[serde(default)]
    pub temperature_c: Option<f32>,
    pub asleep: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedTemperature {
    pub name: String,
    #[serde(default)]
    pub temperature_c: Option<f32>,
}

/// An event of synohwd's list (buttons, fan failures, thermal shutdown), newest last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareEvent {
    /// `None` when synohwd's local time could not be read.
    #[serde(default)]
    pub at_ms: Option<u64>,
    pub text: String,
}

/// Raw values for the diagnostics page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SynologyDiagnostics {
    /// The microcontroller's `D` reading × 16.13 as synohwd reports it; unit unknown.
    #[serde(default)]
    pub up_current_raw: Option<u64>,
}

/// `GET /api/v1/hardware/synology/status`: synohwd's status with API units (the
/// `/api/v1/hardware` summary stays as it was).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SynologyStatus {
    pub model: String,
    /// synohwd is running and its status file is fresh (younger than 3 periods + 10 s).
    pub running: bool,
    pub stale: bool,
    /// When synohwd wrote the status.
    #[serde(default)]
    pub read_at_ms: Option<u64>,
    #[serde(default)]
    pub age_s: Option<u64>,
    #[serde(default)]
    pub period_s: Option<u32>,
    #[serde(default)]
    pub fan_duty_pct: Option<u8>,
    pub fan_fail: bool,
    pub fans: Vec<SynologyFan>,
    #[serde(default)]
    pub status_led: Option<StatusLed>,
    pub bays: Vec<SynologyBay>,
    #[serde(default)]
    pub cpu_temperature_c: Option<f32>,
    pub nvme: Vec<NamedTemperature>,
    pub nic: Vec<NamedTemperature>,
    pub events: Vec<HardwareEvent>,
    pub diagnostics: SynologyDiagnostics,
    /// How this boot started, when rynas can tell (see `/api/v1/power/next`).
    #[serde(default)]
    pub boot: Option<crate::power::BootInfo>,
}
