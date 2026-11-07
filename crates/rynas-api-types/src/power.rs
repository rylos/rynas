//! Power Schedule (`/api/v1/power/...`), DSM model: weekly rules that start or shut the NAS
//! down.
//!
//! Configuration key `power.schedule` ([`PowerSchedule`]). Shutdown rules become internal
//! scheduler tasks `power-off-<rule>` (staged with the schedule) that check the system first;
//! startup rules arm the RTC wake alarm before every shutdown (`rynas-power-wake.service`).
//! Any Debian machine with an RTC `wakealarm` can start on schedule; shutdown works everywhere.

use serde::{Deserialize, Serialize};

use crate::PendingOp;

/// The `power.schedule` configuration key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PowerSchedule {
    #[serde(default)]
    pub rules: Vec<PowerRule>,
}

fn default_true() -> bool {
    true
}

/// One rule: on these weekdays at `hour:minute` (local time), start up or shut down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerRule {
    /// `[a-z0-9][a-z0-9-]{0,31}`, unique; shutdown rules run as task `power-off-<id>`.
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub action: PowerAction,
    /// At least one day.
    pub days: Vec<Weekday>,
    /// 0..=23, local time.
    pub hour: u8,
    /// 0..=59.
    pub minute: u8,
    /// Shutdown rules only: what to do when the system is busy (default: wait up to 30 min,
    /// then skip; every check on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown: Option<ShutdownPolicy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    Startup,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    pub const ALL: [Weekday; 7] = [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
        Weekday::Sat,
        Weekday::Sun,
    ];

    /// 0 = Monday.
    pub fn index(self) -> u8 {
        self as u8
    }

    /// systemd's `OnCalendar=` spelling.
    pub fn systemd(self) -> &'static str {
        ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][self as usize]
    }
}

/// What a scheduled shutdown does when a check finds the system busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownPolicy {
    #[serde(default)]
    pub when_busy: BusyPolicy,
    /// `wait`: re-check every minute up to this long, then skip. 1..=720.
    #[serde(default = "default_wait")]
    pub max_wait_min: u32,
    /// A RAID resync/recovery/check or a btrfs scrub is running.
    #[serde(default = "default_true")]
    pub check_storage: bool,
    /// Another rynas task (snapshot, backup, replication, apply...) is running.
    #[serde(default = "default_true")]
    pub check_tasks: bool,
    /// SMB clients have open files (`smbstatus --json`).
    #[serde(default = "default_true")]
    pub check_smb: bool,
}

fn default_wait() -> u32 {
    30
}

impl Default for ShutdownPolicy {
    fn default() -> Self {
        Self {
            when_busy: BusyPolicy::Wait,
            max_wait_min: default_wait(),
            check_storage: true,
            check_tasks: true,
            check_smb: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BusyPolicy {
    /// Wait up to `max_wait_min`, then skip this shutdown.
    #[default]
    Wait,
    /// Skip this shutdown at once.
    Skip,
    /// Shut down anyway (the checks are logged).
    Force,
}

/// `GET/PUT /api/v1/power/schedule`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PowerScheduleState {
    pub applied: PowerSchedule,
    /// The session user's staged schedule.
    #[serde(default)]
    pub pending: Option<PowerSchedule>,
    /// Per rule of the effective (staged or applied) schedule.
    pub rules: Vec<PowerRuleStatus>,
    pub next: PowerNext,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerRuleStatus {
    pub id: String,
    #[serde(default)]
    pub pending: Option<PendingOp>,
    /// Shutdown rules: the scheduler task (`power-off-<id>`).
    #[serde(default)]
    pub task_id: Option<String>,
}

/// `GET /api/v1/power/next`: the next scheduled startup and shutdown of the applied schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PowerNext {
    #[serde(default)]
    pub next_startup_ms: Option<u64>,
    #[serde(default)]
    pub next_startup_rule: Option<String>,
    #[serde(default)]
    pub next_shutdown_ms: Option<u64>,
    #[serde(default)]
    pub next_shutdown_rule: Option<String>,
    /// IANA name of the system time zone the rules are read in, e.g. "Europe/Rome".
    pub time_zone: String,
    pub wake: WakeSupport,
    #[serde(default)]
    pub last_boot: Option<BootInfo>,
}

/// Can this machine start itself (RTC wake alarm)?
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WakeSupport {
    /// An RTC with a `wakealarm` attribute exists.
    pub supported: bool,
    /// The RTC used, e.g. "rtc0".
    #[serde(default)]
    pub rtc: Option<String>,
    /// The alarm currently in the RTC.
    #[serde(default)]
    pub armed_ms: Option<u64>,
    /// When rynas last armed (or cleared) it, and for which time.
    #[serde(default)]
    pub last_armed: Option<WakeArmed>,
    /// Shown next to the startup rules (no RTC, the firmware may ignore the alarm...), in
    /// English for logs and the CLI; the UI translates `warning_code`.
    #[serde(default)]
    pub warning: Option<String>,
    /// The same warning as a code (`rtc` names the clock, `last_armed.error` the failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning_code: Option<WakeWarning>,
}

/// Why the NAS may not start itself on schedule ([`WakeSupport::warning_code`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeWarning {
    /// No real-time clock found.
    NoRtc,
    /// The clock `rtc` has no wake alarm.
    NoWakeAlarm,
    /// The last arming failed (`last_armed.error`).
    LastArmFailed,
    #[serde(other)]
    Other,
}

/// Why arming the wake alarm failed ([`WakeArmed::error_code`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeArmError {
    /// No real-time clock.
    NoRtc,
    /// The clock `rtc` has no wake alarm.
    NoWakeAlarm,
    /// Setting the alarm failed: `error` holds what the command or the kernel said.
    Failed,
    #[serde(other)]
    Other,
}

/// Written by `rynasctl power arm-wake` (`/var/lib/rynas/power/wake.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WakeArmed {
    pub at_ms: u64,
    /// `None`: the alarm was cleared (no startup rule).
    #[serde(default)]
    pub wake_ms: Option<u64>,
    #[serde(default)]
    pub rule: Option<String>,
    #[serde(default)]
    pub rtc: Option<String>,
    /// `boot` or `shutdown`.
    pub reason: String,
    /// The `rtcwake` run failed or the RTC has no alarm (English; the output of the command
    /// for `failed`); the UI translates `error_code`.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<WakeArmError>,
}

/// How a boot started, as far as rynas can tell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BootInfo {
    pub boot_ms: u64,
    pub cause: BootCause,
    /// The previous boot ended with a clean shutdown (`None`: unknown, e.g. first boot).
    #[serde(default)]
    pub previous_shutdown_clean: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BootCause {
    /// The RTC alarm armed by the power schedule (boot within minutes of it).
    ScheduledStartup,
    /// The hardware watchdog reset the board.
    Watchdog,
    /// Power button, Wake-on-LAN, AC recovery or a reboot: the hardware does not say which.
    #[default]
    Unknown,
}

// ---------------------------------------------------------------------------------------------
// manual restart and shut down (`/api/v1/system/power`)
// ---------------------------------------------------------------------------------------------

/// What an administrator asks the NAS to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemPowerAction {
    Restart,
    Shutdown,
}

/// Delays offered by the dialog, in minutes (`0`: now).
pub const SYSTEM_POWER_DELAYS_MIN: [u32; 3] = [0, 1, 5];

/// `POST /system/power`: starts the task `system.restart` or `system.shutdown`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemPowerRequest {
    pub action: SystemPowerAction,
    /// One of [`SYSTEM_POWER_DELAYS_MIN`].
    #[serde(default)]
    pub delay_min: u32,
}

/// A restart or shut down scheduled for later (systemd-logind's schedule, whoever set it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSystemPower {
    pub action: SystemPowerAction,
    pub at_ms: u64,
    /// The rynas user who asked; `None` when it was set outside rynas (`shutdown +N`).
    #[serde(default)]
    pub by: Option<String>,
}

/// What a restart or shut down would interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerWarningCode {
    /// A RAID array resyncs, rebuilds, reshapes or checks (`subject`: the array, `op`, `pct`).
    RaidBusy,
    /// A scrub task runs (`subject`: its task kind).
    ScrubRunning,
    /// Another rynas task runs (`subject`: its task kind).
    TaskRunning,
    /// A Task Scheduler task runs (`subject`: its unit).
    SchedulerTaskRunning,
    /// SMB clients hold files open (`n`).
    SmbOpenFiles,
    /// apt or dpkg runs (`subject`: the process).
    AptRunning,
    /// The UPS runs on battery.
    UpsOnBattery,
    /// The boot layer boots a new kernel on trial: if it fails, the previous one comes back.
    BootTrial,
}

/// One warning of [`SystemPowerState::warnings`]; the UI translates `code` and fills in the
/// values, `text` is the English fallback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PowerWarning {
    pub code: PowerWarningCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// RAID operation (`resync`, `recovery`, `reshape`, `check`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pct: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
    pub text: String,
}

/// `GET /system/power`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SystemPowerState {
    #[serde(default)]
    pub pending: Option<PendingSystemPower>,
    /// What a restart or shut down now would interrupt; the administrator may go on anyway.
    #[serde(default)]
    pub warnings: Vec<PowerWarning>,
}
