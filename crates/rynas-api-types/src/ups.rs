//! UPS (`/api/v1/ups...`), D-UPS: a thin layer over NUT (any Debian with the `nut` package)
//! and, where a hardware module's system layer runs NUT itself, over that layer
//! ([`UpsBackend`], `rynas_module::UpsLayer`).
//!
//! - [`UpsStatus`]: what upsd (`127.0.0.1:3493`, plain-text protocol) reports, read by
//!   rynas-web every few seconds and pushed on `/api/v1/events` as `ups.status`.
//! - [`UpsSettings`]: the configuration key `ups.safemode`: the extra "after N minutes on
//!   battery" trigger and the time to stop work before it. Low battery and a critical UPS
//!   (OB+LB) always end in a shutdown (NUT's upsmon, or the module layer's own), whichever
//!   first.
//! - [`BatteryTestRequest`]: `POST /api/v1/ups/test` sends the NUT instant command of the test
//!   (`test.battery.start.quick|deep`, `test.battery.stop`; through the module layer's tool
//!   where there is one) as the rynasd task `ups.test`.
//! - Setup wizard: [`UpsDeviceConfig`] is the configuration key `ups.device` (the UPS's
//!   driver, port and options); `POST /api/v1/ups/detect` runs the task `ups.detect`, whose
//!   [`UpsDetectReport`] (candidates and their test runs) is part of [`UpsDeviceState`].

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
    /// NUT name.
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
    /// A hardware module's system layer runs NUT, the tests and the shutdown (its texts are the
    /// module's, in the web interface's module folder).
    Module,
    /// NUT is not installed.
    #[default]
    NotInstalled,
}

/// Default time to stop work before the shutdown.
pub const PRE_SAFEMODE_TIMEOUT_DEFAULT_S: u32 = 120;
/// Accepted range of the time on battery when set (1 minute .. 24 hours).
pub const SAFEMODE_AFTER_RANGE_S: (u32, u32) = (60, 86_400);
/// Accepted range of the time to stop work.
pub const PRE_SAFEMODE_TIMEOUT_RANGE_S: (u32, u32) = (10, 900);

/// The `ups.safemode` configuration key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsSettings {
    /// Shutdown after this long on battery; `null`: only on low battery.
    #[serde(default)]
    pub safemode_after_s: Option<u32>,
    /// Time to stop work before it (rynas stops its tasks there; with a module layer the
    /// time of all its pre-shutdown hooks together).
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

/// `POST /v1/ups/safe-mode` (internal, root only): the pre-shutdown hook of a module's UPS
/// layer. On plain NUT rynasd does the same by itself when the UPS turns critical or the
/// timer expires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeModeRequest {
    /// The reason: `lowbatt`, `waittimeup`, `nocomm`, `fsd`, `shutdowncmd`.
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

/// Reasons of a shutdown on UPS power (rynasd's own watcher uses `lowbatt`, `fsd`,
/// `waittimeup`).
pub const SAFE_MODE_REASONS: &[&str] = &["lowbatt", "waittimeup", "nocomm", "fsd", "shutdowncmd"];

// ---------------------------------------------------------------------------------------------
// UPS setup (driver wizard): `ups.device`
// ---------------------------------------------------------------------------------------------

/// One driver option of a UPS (`key = value` in `ups.conf`, `-x key=value` for a driver);
/// `value: null` is a flag such as `norating`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsOption {
    pub key: String,
    #[serde(default)]
    pub value: Option<String>,
}

impl UpsOption {
    pub fn new(key: &str, value: Option<&str>) -> Self {
        Self {
            key: key.into(),
            value: value.map(str::to_string),
        }
    }
}

/// A NUT driver with its port and options: one `ups.conf` section without its name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsDevice {
    /// Driver program (`usbhid-ups`, `nutdrv_qx`, `snmp-ups`...).
    pub driver: String,
    /// `auto` for USB, a serial device (`/dev/ttyS0`) or a network address (SNMP).
    pub port: String,
    #[serde(default)]
    pub options: Vec<UpsOption>,
}

impl UpsDevice {
    pub fn option(&self, key: &str) -> Option<&UpsOption> {
        self.options.iter().find(|o| o.key == key)
    }
}

/// Default NUT name of the UPS rynas sets up.
pub const UPS_DEVICE_NAME: &str = "ups";

/// The `ups.device` configuration key: the UPS rynas set up with the wizard. On plain NUT a
/// rynas block in `ups.conf` (plus `nut.conf` MODE, a upsmon user in `upsd.users` and its
/// `MONITOR` line in `upsmon.conf`); with a module layer the driver choice handed to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsDeviceConfig {
    /// NUT name (the `ups.conf` section).
    #[serde(default = "default_device_name")]
    pub name: String,
    pub device: UpsDevice,
    /// `ups.conf` sections of the admin's that this setup removes (an explicit take over of
    /// a section in the way, see [`UpsSection::conflict`]).
    #[serde(default)]
    pub take_over: Vec<String>,
}

fn default_device_name() -> String {
    UPS_DEVICE_NAME.into()
}

/// A `ups.conf` section rynas does not manage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsSection {
    pub name: String,
    pub driver: Option<String>,
    pub port: Option<String>,
    /// It is in the way of the UPS being set up (same name, or the same USB device): the
    /// setup refuses unless it is listed in [`UpsDeviceConfig::take_over`].
    #[serde(default)]
    pub conflict: bool,
}

/// Why the wizard cannot run here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpsSetupBlock {
    /// NUT is not installed (Debian package `nut`).
    NutMissing,
    /// The module's UPS layer is too old to set up other drivers.
    ModuleTooOld,
}

/// Where a candidate comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpsCandidateSource {
    /// rynas's table of USB IDs known to need a specific driver or options.
    Known,
    /// `nut-scanner`'s suggestion.
    Scanner,
    /// A generic driver tried for any USB UPS.
    Generic,
    /// Entered by the admin.
    Manual,
}

/// Outcome of a test run of a candidate driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpsProbeState {
    /// Not tried (an earlier candidate answered, or the detection stopped).
    #[default]
    NotRun,
    /// The driver reported the UPS status.
    Ok,
    /// The driver started but reported no status, or did not start.
    Failed,
    /// Another driver holds the device (stop the running UPS driver to test).
    Busy,
    /// Known to need a newer NUT than the installed one ([`UpsCandidate::min_nut`]).
    NeedsNewerNut,
    /// The driver program is not installed.
    NoDriver,
}

/// What a test run read from the UPS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsProbe {
    pub state: UpsProbeState,
    /// `ups.status` (e.g. `OL CHRG`).
    pub status: Option<String>,
    pub battery_charge_pct: Option<u32>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    /// Last lines of the driver's output when it failed (English, as NUT prints them).
    #[serde(default)]
    pub log_tail: Vec<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpsCandidate {
    pub device: UpsDevice,
    pub source: UpsCandidateSource,
    /// Lowest NUT version this driver setting works with (`2.8.5`), when known.
    pub min_nut: Option<String>,
    pub probe: UpsProbe,
}

/// A UPS found on USB by `nut-scanner`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsUsbDevice {
    /// Four hex digits, lowercase (`0925`).
    pub vendor_id: String,
    pub product_id: String,
    pub vendor: Option<String>,
    pub product: Option<String>,
    /// `nut-scanner`'s suggestion.
    pub driver: Option<String>,
}

/// The last detection (task `ups.detect`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsDetectReport {
    pub at_ms: u64,
    /// Installed NUT version (`2.8.1`).
    pub nut_version: Option<String>,
    #[serde(default)]
    pub usb: Vec<UpsUsbDevice>,
    #[serde(default)]
    pub candidates: Vec<UpsCandidate>,
    /// Index in `candidates` of the first one that answered.
    pub found: Option<usize>,
    /// A UPS driver service was running and the admin did not allow stopping it: nothing
    /// was tested (ask, then detect again with `stop_drivers`).
    #[serde(default)]
    pub busy: bool,
}

/// `POST /api/v1/ups/detect` → 202 `TaskStarted` (task `ups.detect`); the report is in
/// `GET /api/v1/ups/device` once the task ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpsDetectRequest {
    /// Test only this setting (manual entry: serial, network, another driver); otherwise
    /// scan USB and try the candidates in order until one answers.
    #[serde(default)]
    pub device: Option<UpsDevice>,
    /// Stop a running UPS driver during the test (it is started again afterwards).
    #[serde(default)]
    pub stop_drivers: bool,
}

/// `GET /api/v1/ups/device`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsDeviceState {
    pub backend: UpsBackend,
    /// Why the wizard cannot run (`null`: it can).
    pub blocked: Option<UpsSetupBlock>,
    pub nut_version: Option<String>,
    pub applied: Option<UpsDeviceConfig>,
    /// The session user's staged value, when it differs.
    pub pending: Option<UpsDeviceConfig>,
    /// `ups.conf` sections rynas does not manage (plain NUT only), with `conflict` computed
    /// against `pending`, else `applied`.
    #[serde(default)]
    pub foreign: Vec<UpsSection>,
    /// A UPS driver service is running now.
    #[serde(default)]
    pub drivers_running: bool,
    pub detect: Option<UpsDetectReport>,
    /// The last detection found a setting that needs a newer NUT and rynasd can bring one
    /// (plain NUT only, never with a module's UPS layer): `POST /api/v1/ups/update-nut`.
    #[serde(default)]
    pub nut_update: Option<UpsNutUpdate>,
}

/// A newer NUT rynasd can install (`UpsDeviceState::nut_update`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsNutUpdate {
    /// The NUT version it brings (`2.8.5`).
    pub version: String,
    /// Where it comes from; `null`: rynas has a build for this system but its packages
    /// are not on this NAS (put them in `dir`).
    pub source: Option<UpsNutSource>,
    /// Where rynasd takes rynas's own packages from (`/var/lib/rynas/packages`).
    pub dir: String,
}

/// Where a NUT update comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpsNutSource {
    /// An apt source of the system has a new enough NUT (its candidate).
    Apt,
    /// A `<codename>-backports` source configured on the system has it (`apt -t`).
    Backports,
    /// rynas's NUT build for this Debian release (the local package folder, else downloaded
    /// from its fixed release, checked against the pinned SHA-256).
    Rynas,
}

/// One driver of `GET /api/v1/ups/drivers`: installed NUT drivers with the manufacturers
/// NUT's hardware list names for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NutDriver {
    pub name: String,
    #[serde(default)]
    pub manufacturers: Vec<String>,
}

/// A file the setup changes (`POST /api/v1/ups/plan/device`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsPlanFile {
    pub path: String,
    /// rynas's current block (`null`: none yet).
    pub before: Option<String>,
    pub after: String,
    /// Lines outside the block the setup removes (taken-over sections, contradicting lines).
    #[serde(default)]
    pub removed: Vec<String>,
}

/// `POST /api/v1/ups/plan/device` (no side effects): what applying the setting does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsSetupPlan {
    #[serde(default)]
    pub files: Vec<UpsPlanFile>,
    /// Commands run after the files (service restarts, or the module layer's setup).
    #[serde(default)]
    pub commands: Vec<String>,
    /// Sections in the way that are not taken over: the apply would refuse.
    #[serde(default)]
    pub conflicts: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// network UPS server: `ups.netserver`
// ---------------------------------------------------------------------------------------------

/// NUT's port (upsd, IANA `nut`).
pub const NUT_PORT: u16 = 3493;
/// At most this many client accounts.
pub const MAX_NET_CLIENTS: usize = 32;
/// At most this many listen addresses, and as many allowed hosts or subnets.
pub const MAX_NET_ADDRS: usize = 16;
/// Length range of a password the admin chooses.
pub const NET_PASSWORD_LEN: (usize, usize) = (12, 64);

/// What a client account may do on upsd (`upsd.users`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpsClientRole {
    /// `upsmon secondary`: another machine's upsmon that shuts that machine down when this
    /// UPS runs out (NUT before 2.8 called it `slave`).
    #[default]
    Secondary,
    /// Logs in and reads, nothing else (no `actions`, no `instcmds`): dashboards and home
    /// automation. Reading works without any login too; the account names the client.
    Monitor,
}

/// A client account of the network UPS server. The password is write-only: `password` (the
/// admin's) or `generate` on a `PUT`; stored sealed (`sealed`, never in answers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpsNetClient {
    /// The NUT user name: `[a-z0-9_-]`, 1–32 characters.
    pub name: String,
    #[serde(default)]
    pub role: UpsClientRole,
    /// What the account is for (free text, at most 64 characters).
    #[serde(default)]
    pub note: String,
    /// Write-only: a password chosen by the admin (12–64 of `[A-Za-z0-9._~@%+=:,-]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Write-only: generate a new password (shown once in the answer of the `PUT`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub generate: bool,
    /// The stored password, sealed with rynasd's secret key (base64); never in answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed: Option<String>,
}

/// The `ups.netserver` configuration key: other machines read this UPS from upsd and shut
/// down on its low battery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpsNetServerConfig {
    pub enabled: bool,
    /// Addresses of this NAS upsd listens on, besides localhost; empty: all its addresses
    /// (IPv4 and IPv6).
    #[serde(default)]
    pub listen: Vec<String>,
    /// Hosts and subnets (`192.168.1.20`, `192.168.1.0/24`, `fd00::/64`) allowed to connect,
    /// besides the NAS itself; empty: any host that reaches the port. Enforced by systemd's
    /// per-service IP filter on `nut-server.service`.
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub clients: Vec<UpsNetClient>,
}

/// Why the network UPS server cannot be set up here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpsNetBlock {
    /// NUT is not installed.
    NutMissing,
    /// A hardware module's UPS layer runs NUT and owns its files, and has no network UPS
    /// server of its own.
    Module,
    /// The module's UPS layer is too old for its network UPS server (the module's text says
    /// which version is needed).
    ModuleTooOld,
}

/// Where the allowed hosts of a module layer's network UPS server come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpsNetSource {
    /// Nothing set: the server is only on the NAS itself.
    #[default]
    None,
    /// The layer's own file for the server (written by a save here, or by the layer's tool).
    Own,
    /// Set by hand in the layer's general UPS file; a save here writes the layer's own file,
    /// which then wins.
    Manual,
    /// A source of a newer layer.
    #[serde(other)]
    Other,
}

/// A module layer's network UPS server (`UpsNetServerState::layer`): the layer runs upsd and
/// its firewall, rynas sets allowed hosts and accounts through the layer's tool. Listen
/// addresses are the layer's (every address of the NAS), allowed hosts are required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsNetLayer {
    /// The server is on in the layer now.
    pub enabled: bool,
    /// The addresses upsd listens on, as the layer reports them.
    #[serde(default)]
    pub listen: Vec<String>,
    /// The allowed hosts and subnets the layer applies now.
    #[serde(default)]
    pub allow: Vec<String>,
    /// The layer's own monitoring account (upsmon primary, for NAS clients that expect it;
    /// its password is set with the layer's tool), when it exists.
    pub primary_account: Option<String>,
    /// Accounts the layer has that the rynas setting does not (set with the layer's tool): a
    /// save here removes them.
    #[serde(default)]
    pub outside: Vec<String>,
    pub source: UpsNetSource,
}

/// An address of this NAS (a listen choice).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsNetAddress {
    pub address: String,
    /// Its connected subnet (`192.168.1.0/24`), a suggestion for `allow`.
    pub subnet: String,
}

/// `GET /api/v1/ups/netserver`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsNetServerState {
    /// Why it cannot be set up (`null`: it can).
    pub blocked: Option<UpsNetBlock>,
    /// Passwords and envelopes left out.
    pub applied: Option<UpsNetServerConfig>,
    /// The session user's staged value, when it differs.
    pub pending: Option<UpsNetServerConfig>,
    /// This NAS's LAN addresses (loopback and link-local left out).
    #[serde(default)]
    pub addresses: Vec<UpsNetAddress>,
    /// The address clients are told to use: the LAN address of the default route.
    pub host: Option<String>,
    pub hostname: String,
    pub port: u16,
    /// The NUT name of the UPS clients monitor (the wizard's, else the first `ups.conf`
    /// section).
    pub ups_name: Option<String>,
    /// Whether the kernel applies the allowed-hosts filter (systemd's IP accounting of
    /// `nut-server.service` counts packets only when its cgroup eBPF programs are attached).
    /// `null`: no list of allowed hosts, upsd not running, or unknown.
    #[serde(default)]
    pub filter_active: Option<bool>,
    /// What is wrong with the applied server now (empty: nothing).
    #[serde(default)]
    pub problems: Vec<UpsNetProblem>,
    /// A module layer runs the server (`null`: plain NUT, rynas writes `/etc/nut`).
    #[serde(default)]
    pub layer: Option<UpsNetLayer>,
}

/// A problem of the applied network UPS server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpsNetProblem {
    /// The server is on but `nut-server.service` (upsd) is not running: no client can
    /// connect (often: no UPS set up yet, or upsd failed to start).
    ServerInactive,
    /// Allowed hosts are set but the kernel does not apply systemd's IP filter (no eBPF
    /// cgroup support): every host that reaches port 3493 can connect; a firewall must
    /// limit it.
    FilterInactive,
    /// The module layer's state of the server cannot be read (its tool failed or answered
    /// in a format this rynas does not know).
    LayerUnreadable,
    /// A code of a newer server.
    #[serde(other)]
    Other,
}

/// A password set or generated by a `PUT`, shown once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsNetPassword {
    pub name: String,
    pub password: String,
}

/// `PUT /api/v1/ups/netserver` → the new state and the passwords given or generated by this
/// request (the only time they are sent back).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpsNetServerSaved {
    pub state: UpsNetServerState,
    #[serde(default)]
    pub passwords: Vec<UpsNetPassword>,
}

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
