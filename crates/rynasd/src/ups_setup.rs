//! The UPS setup wizard (`/v1/ups/device`, `/v1/ups/detect`, `/v1/ups/drivers`,
//! `/v1/ups/plan/device`) and the `ups.device` configuration domain.
//!
//! Works with any UPS NUT supports. Detection does not trust `nut-scanner`: the task
//! `ups.detect` builds candidates (rynas's table of known USB IDs, the scanner's suggestion,
//! generic drivers; [`rynas_nut::setup::candidates`]) and gives each a test run until one
//! reports the UPS status. A manual entry (any installed driver, a serial port, an SNMP
//! address, extra options) is tested the same way. The admin then confirms the setting,
//! which is staged as `ups.device` and applied like any configuration key.
//!
//! Two backends behind [`SetupBackend`]:
//!
//! - **NUT** (plain Debian): the test run is the driver on its own (`-s rynasprobe -x … -d 1`)
//!   under a timeout. The apply writes rynas blocks only, never the admin's lines: a section
//!   in `/etc/nut/ups.conf`, `MODE` in `nut.conf` (an existing `netserver` stays), a upsmon
//!   user `rynas-upsmon` in `upsd.users` (password generated once into
//!   `/etc/rynas/nut-upsmon.secret`) and its `MONITOR … primary` line in `upsmon.conf`; files
//!   root:nut 0640. An admin's section in the way (same name, or a USB section that may drive
//!   the same device) makes the apply refuse unless the setting lists it in `take_over`; it is
//!   then removed (the apply keeps a backup and rolls back on failure). Afterwards the driver
//!   instances are resynced and `nut-driver`, `nut-server` and `nut-monitor` enabled and
//!   restarted.
//! - **a module's UPS layer** (`rynas_module::UpsLayer`, D-MODULE): the layer owns `/etc/nut`;
//!   rynas never writes it. The test run and the apply are the layer's commands
//!   ([`LayerBackend`]); a layer too old to set up drivers blocks the wizard
//!   ([`UpsSetupBlock::ModuleTooOld`]).
//!
//! A running UPS driver holds the device: the detection reports `busy` unless the admin
//! allowed stopping it (`stop_drivers`); stopped units are started again at the end.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::TaskStarted;
use rynas_api_types::error_codes;
use rynas_api_types::ups::*;
use rynas_config::files::{self, FileKind};
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile};
use rynas_module::UpsLayer;
pub use rynas_module::ups::Runner;
use rynas_nut::setup as nut;
use rynas_tasks::{Cmd, CmdOutput};
use serde_json::Value;

use crate::ops::{Error, Res, user};
use crate::ups::{Ups, UpsState, backend_of};

pub const DEVICE_KEY: &str = "ups.device";
pub const NUT_UPS_CONF: &str = "/etc/nut/ups.conf";
pub const NUT_CONF: &str = "/etc/nut/nut.conf";
pub const NUT_USERS: &str = "/etc/nut/upsd.users";
pub const UPSMON_CONF: &str = "/etc/nut/upsmon.conf";
pub const MONITOR_USER: &str = "rynas-upsmon";
pub const MONITOR_SECRET: &str = "/etc/rynas/nut-upsmon.secret";
pub const DRIVER_LIST: &str = "/usr/share/nut/driver.list";
/// Where NUT installs its drivers: /usr/libexec/nut from NUT 2.8.2 (searched first), /lib/nut
/// before (Debian bookworm, trixie with 2.8.1), /usr/lib/nut elsewhere.
pub const DRIVER_DIRS: &[&str] = &["/usr/libexec/nut", "/lib/nut", "/usr/lib/nut"];
const BLOCK: &str = "ups-device";
const USERS_BLOCK: &str = "ups-monitor";
const PROBE_TIMEOUT: Duration = Duration::from_secs(45);
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);
const SHORT: Duration = Duration::from_secs(20);
const UNITS: Duration = Duration::from_secs(90);
/// Patterns of the units that run UPS drivers (Debian: one `nut-driver@<ups>` per section).
const DRIVER_UNITS: &[&str] = &["nut-driver@*.service", "nut-driver.service"];

// ---------------------------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------------------------

// The wizard's commands run through a [`Runner`]; tests and `--dry-system` use [`DryRunner`];
// the apply's commands go through the configuration's `System` instead ([`SystemRunner`]).

pub struct HostRunner;

impl Runner for HostRunner {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        cmd.output_blocking().map_err(|e| e.to_string())
    }
}

/// `--dry-system`: nothing runs. Answers like a host with NUT 2.8.5 and the USB UPS
/// `0925:1234` that only `nutdrv_qx` with the `armac` subdriver drives, so the whole wizard
/// can be walked through on a test instance. A module layer's own tool answers through its
/// `UpsLayer::dry_output`.
pub struct DryRunner {
    pub nut_version: &'static str,
    pub calls: Mutex<Vec<String>>,
    pub layer: Option<Arc<dyn UpsLayer>>,
}

impl Default for DryRunner {
    fn default() -> Self {
        Self {
            nut_version: "2.8.5",
            calls: Mutex::new(Vec::new()),
            layer: None,
        }
    }
}

/// The configuration's `System` as a [`Runner`] (an apply's import).
pub struct SystemRunner<'a>(pub &'a dyn rynas_config::System);

impl Runner for SystemRunner<'_> {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        self.0.run(cmd)
    }
}

impl DryRunner {
    fn out(code: i32, stdout: &str, stderr: &str) -> Result<CmdOutput, String> {
        Ok(CmdOutput {
            code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        })
    }
}

impl Runner for DryRunner {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        let line = cmd.display();
        tracing::info!("dry system: {line}");
        self.calls.lock().unwrap().push(line.clone());
        if let Some(o) = self.layer.as_ref().and_then(|l| l.dry_output(cmd)) {
            return Ok(o);
        }
        let program = Path::new(cmd.program())
            .file_name()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ok_vars = "battery.charge: 100\nbattery.voltage: 13.70\ninput.voltage: 230.0\n\
            ups.load: 12\nups.status: OL\n";
        let armac = line.contains("subdriver=armac");
        match program.as_str() {
            "upsdrvctl" => Self::out(
                0,
                &format!(
                    "Network UPS Tools - UPS driver controller {}\n",
                    self.nut_version
                ),
                "",
            ),
            "nut-scanner" => Self::out(
                0,
                "[nutdev1]\n\tdriver = \"richcomm_usb\"\n\tport = \"auto\"\n\tvendorid = \"0925\"\n\
                 \tproductid = \"1234\"\n\tproduct = \"UPS USB Mon V2.0\"\n\tvendor = \"Lakeview Research\"\n",
                "",
            ),
            "systemctl" => Self::out(0, "", ""),
            _ if armac => Self::out(0, ok_vars, ""),
            "richcomm_usb" => Self::out(1, "", "driver callback failed: Entity not found\n"),
            _ => Self::out(1, "", "No supported UPS detected\n"),
        }
    }
}

fn cmd(program: impl AsRef<std::ffi::OsStr>) -> Cmd {
    Cmd::new(program).env_clear().timeout(SHORT)
}

// ---------------------------------------------------------------------------------------------
// backends
// ---------------------------------------------------------------------------------------------

/// What differs between plain NUT and a module's layer: the test run and the apply.
pub trait SetupBackend: Send + Sync {
    /// Why the wizard cannot run (`None`: it can).
    fn blocked(&self) -> Option<UpsSetupBlock>;
    /// The message of a block.
    fn block_text(&self, b: UpsSetupBlock) -> &'static str {
        match b {
            UpsSetupBlock::NutMissing => "NUT is not installed (Debian package nut)",
            UpsSetupBlock::ModuleTooOld => "the UPS layer cannot set up other UPS drivers",
        }
    }
    /// One test run of `d`.
    fn probe(&self, d: &UpsDevice, timeout: Duration) -> UpsProbe;
}

/// Plain NUT: the driver itself, configured on its command line.
pub struct NutBackend<'a> {
    pub root: &'a Path,
    pub runner: &'a dyn Runner,
}

/// The driver program below `root`, if installed.
pub fn driver_path(root: &Path, driver: &str) -> Option<PathBuf> {
    if !nut::valid_driver(driver) {
        return None;
    }
    DRIVER_DIRS
        .iter()
        .map(|d| files::under_root(root, Path::new(d)).join(driver))
        .find(|p| p.is_file())
}

/// Installed driver programs (empty when no driver directory exists below `root`).
pub fn installed_drivers(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for d in DRIVER_DIRS {
        let Ok(rd) = std::fs::read_dir(files::under_root(root, Path::new(d))) else {
            continue;
        };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if e.path().is_file() && nut::valid_driver(&n) && !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out.sort();
    out
}

impl SetupBackend for NutBackend<'_> {
    fn blocked(&self) -> Option<UpsSetupBlock> {
        (backend_of(self.root, None) == UpsBackend::NotInstalled)
            .then_some(UpsSetupBlock::NutMissing)
    }

    fn probe(&self, d: &UpsDevice, timeout: Duration) -> UpsProbe {
        let Some(path) = driver_path(self.root, &d.driver) else {
            return UpsProbe {
                state: UpsProbeState::NoDriver,
                ..Default::default()
            };
        };
        let c = cmd(path)
            .args(nut::driver_probe_args(d))
            .env("NUT_QUIET_INIT_BANNER", "true")
            .timeout(timeout);
        run_probe(self.runner, &c, timeout, |_| None)
    }
}

/// A module's UPS layer: its own test run.
pub struct LayerBackend<'a> {
    pub layer: &'a dyn UpsLayer,
    pub runner: &'a dyn Runner,
}

impl SetupBackend for LayerBackend<'_> {
    fn blocked(&self) -> Option<UpsSetupBlock> {
        self.layer.setup_blocked(self.runner)
    }

    fn block_text(&self, b: UpsSetupBlock) -> &'static str {
        match b {
            UpsSetupBlock::ModuleTooOld => self.layer.too_old_message(),
            UpsSetupBlock::NutMissing => "NUT is not installed (Debian package nut)",
        }
    }

    fn probe(&self, d: &UpsDevice, timeout: Duration) -> UpsProbe {
        let c = self.layer.probe_cmd(d, timeout);
        run_probe(self.runner, &c, timeout, |code| self.layer.probe_exit_state(code))
    }
}

/// Runs a test-run command: upsc-format variables on stdout, the driver's lines on stderr.
/// `special` maps an exit code to a state of its own.
fn run_probe(
    runner: &dyn Runner,
    c: &Cmd,
    timeout: Duration,
    special: impl Fn(Option<i32>) -> Option<UpsProbeState>,
) -> UpsProbe {
    let start = Instant::now();
    let ms = |s: Instant| s.elapsed().as_millis() as u64;
    match runner.run(c) {
        Ok(o) => {
            let mut p = nut::probe_of(
                &nut::parse_dump(&o.stdout),
                &format!("{}\n{}", o.stdout, o.stderr),
                ms(start),
            );
            if p.state != UpsProbeState::Ok
                && let Some(s) = special(o.code)
            {
                p.state = s;
            }
            p
        }
        Err(e) => UpsProbe {
            state: UpsProbeState::Failed,
            log_tail: vec![if e.contains("timed out") {
                format!("no answer within {} s", timeout.as_secs())
            } else {
                e
            }],
            duration_ms: ms(start),
            ..Default::default()
        },
    }
}

// ---------------------------------------------------------------------------------------------
// the wizard's state
// ---------------------------------------------------------------------------------------------

/// Wizard state of rynasd (part of [`Ups`]).
pub struct Setup {
    pub runner: Arc<dyn Runner>,
    /// The last detection report.
    pub report: Mutex<Option<UpsDetectReport>>,
    /// Installed NUT version, read once.
    pub version: Mutex<Option<String>>,
    /// One detection at a time.
    pub busy: Mutex<bool>,
}

impl Setup {
    pub fn new(runner: Arc<dyn Runner>) -> Self {
        Self {
            runner,
            report: Mutex::new(None),
            version: Mutex::new(None),
            busy: Mutex::new(false),
        }
    }
}

impl Ups {
    fn setup_backend(&self) -> Box<dyn SetupBackend + '_> {
        let runner = self.setup.runner.as_ref();
        if let Some(layer) = self.layer.as_deref() {
            Box::new(LayerBackend { layer, runner })
        } else {
            Box::new(NutBackend {
                root: &self.root,
                runner,
            })
        }
    }
}

/// Installed NUT version (`upsdrvctl -V`).
pub fn read_version(runner: &dyn Runner) -> Option<(u32, u32, u32)> {
    let o = runner.run(&cmd("upsdrvctl").arg("-V")).ok()?;
    nut::nut_version(&format!("{}\n{}", o.stdout, o.stderr))
}

fn cached_version(u: &Ups) -> Option<String> {
    let mut v = u.setup.version.lock().unwrap();
    if v.is_none() {
        *v = read_version(u.setup.runner.as_ref()).map(nut::fmt_version);
    }
    v.clone()
}

/// Active UPS driver units.
pub fn running_drivers(runner: &dyn Runner) -> Vec<String> {
    let c = cmd("systemctl")
        .args(["list-units", "--plain", "--no-legend", "--state=active"])
        .args(DRIVER_UNITS);
    runner
        .run(&c)
        .map(|o| {
            o.stdout
                .lines()
                .filter_map(|l| l.split_whitespace().next())
                .filter(|u| u.starts_with("nut-driver"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// USB UPSes `nut-scanner -U` finds.
pub fn scan_usb(runner: &dyn Runner, log: &dyn Fn(&str)) -> Vec<UpsUsbDevice> {
    match runner.run(&cmd("nut-scanner").args(["-U", "-q"]).timeout(SCAN_TIMEOUT)) {
        Ok(o) => nut::parse_scanner(&o.stdout),
        Err(e) => {
            log(&format!("nut-scanner: {e}"));
            Vec::new()
        }
    }
}

/// The detection: candidates (or the manual entry), each tested until one answers.
#[allow(clippy::too_many_arguments)]
pub fn detect(
    backend: &dyn SetupBackend,
    runner: &dyn Runner,
    req: &UpsDetectRequest,
    timeout: Duration,
    log: &dyn Fn(&str),
    progress: &dyn Fn(u8),
    cancelled: &dyn Fn() -> bool,
    now_ms: u64,
) -> Result<UpsDetectReport, String> {
    if let Some(b) = backend.blocked() {
        return Err(backend.block_text(b).into());
    }
    let version = read_version(runner);
    let mut report = UpsDetectReport {
        at_ms: now_ms,
        nut_version: version.map(nut::fmt_version),
        ..Default::default()
    };
    log(&format!(
        "NUT version: {}",
        report.nut_version.as_deref().unwrap_or("unknown")
    ));
    if let Some(d) = &req.device {
        nut::check_device(d).map_err(|(_, m)| m)?;
        report.candidates.push(UpsCandidate {
            device: d.clone(),
            source: UpsCandidateSource::Manual,
            min_nut: None,
            probe: UpsProbe::default(),
        });
    } else {
        log("scanning USB (nut-scanner -U)");
        report.usb = scan_usb(runner, log);
        for u in &report.usb {
            log(&format!(
                "found {}:{} {} {} (nut-scanner suggests {})",
                u.vendor_id,
                u.product_id,
                u.vendor.as_deref().unwrap_or(""),
                u.product.as_deref().unwrap_or(""),
                u.driver.as_deref().unwrap_or("nothing")
            ));
        }
        report.candidates = nut::candidates(&report.usb);
        if report.candidates.is_empty() {
            log("no USB UPS found");
            return Ok(report);
        }
    }
    let running = running_drivers(runner);
    if !running.is_empty() {
        if !req.stop_drivers {
            log(&format!(
                "a UPS driver is running ({}): nothing tested",
                running.join(", ")
            ));
            report.busy = true;
            return Ok(report);
        }
        log(&format!("stopping {} during the test", running.join(", ")));
        let stop = cmd("systemctl").arg("stop").args(&running).timeout(UNITS);
        if let Err(e) = runner.run(&stop).and_then(ok) {
            return Err(format!("stopping the UPS driver: {e}"));
        }
    }
    let n = report.candidates.len();
    for i in 0..n {
        if cancelled() {
            log("cancelled");
            break;
        }
        let c = &report.candidates[i];
        let line = format!(
            "{} {}",
            c.device.driver,
            nut::driver_probe_args(&c.device)[2..].join(" ")
        );
        let too_old = match (version, c.min_nut.as_deref().and_then(nut::parse_version)) {
            (Some(have), Some(need)) => have < need,
            _ => false,
        };
        let probe = if too_old {
            log(&format!(
                "{line}: needs NUT {} or newer, skipped",
                c.min_nut.as_deref().unwrap_or("")
            ));
            UpsProbe {
                state: UpsProbeState::NeedsNewerNut,
                ..Default::default()
            }
        } else {
            log(&format!("testing {line}"));
            let p = backend.probe(&c.device, timeout);
            match p.state {
                UpsProbeState::Ok => log(&format!(
                    "  answers: status {}, charge {}",
                    p.status.as_deref().unwrap_or("?"),
                    p.battery_charge_pct
                        .map_or("?".to_string(), |c| format!("{c} %"))
                )),
                s => {
                    log(&format!("  {s:?}"));
                    for l in &p.log_tail {
                        log(&format!("  | {l}"));
                    }
                }
            }
            p
        };
        let ok = probe.state == UpsProbeState::Ok;
        report.candidates[i].probe = probe;
        progress(((i + 1) * 100 / n).min(99) as u8);
        if ok {
            report.found = Some(i);
            break;
        }
    }
    if !running.is_empty() {
        log(&format!("starting {} again", running.join(", ")));
        let start = cmd("systemctl").arg("start").args(&running).timeout(UNITS);
        if let Err(e) = runner.run(&start).and_then(ok) {
            log(&format!("starting the UPS driver again: {e}"));
        }
    }
    if report.found.is_none() {
        log("no candidate reported the UPS status");
    }
    Ok(report)
}

fn ok(o: CmdOutput) -> Result<CmdOutput, String> {
    if o.success() {
        Ok(o)
    } else {
        Err(o
            .stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("failed")
            .to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// rendering (plain NUT)
// ---------------------------------------------------------------------------------------------

/// The upsmon user's password, generated on first use (root 0600 below `root`).
pub fn monitor_password(root: &Path) -> Result<String, String> {
    crate::ups::secret(root, MONITOR_SECRET)
}

/// `nut.conf`'s MODE (the last assignment, in rynas's block or not).
pub fn current_mode(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("MODE="))
        .map(|v| {
            v.split('#')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches(['"', '\''])
                .to_string()
        })
        .next_back()
}

fn read(root: &Path, p: &str) -> Option<String> {
    std::fs::read_to_string(files::under_root(root, Path::new(p))).ok()
}

fn nut_file(path: &str, id: &str, body: String, drop: Vec<String>) -> ManagedFile {
    let mut f = ManagedFile::block(path, id, body).mode(0o640).group("nut");
    if let FileKind::Block { drop_outside, .. } = &mut f.kind {
        *drop_outside = drop;
    }
    f
}

/// The admin's `ups.conf` sections.
pub fn foreign_sections(root: &Path) -> Vec<nut::ConfSection> {
    read(root, NUT_UPS_CONF)
        .map(|t| nut::sections(&t))
        .unwrap_or_default()
}

/// The rynas blocks of `c` on plain NUT; refuses sections in the way that are not taken over.
pub fn nut_files(
    root: &Path,
    c: &UpsDeviceConfig,
    password: &str,
) -> Result<Vec<ManagedFile>, String> {
    let blocking = nut::blocking_sections(&foreign_sections(root), c);
    if !blocking.is_empty() {
        return Err(format!(
            "{NUT_UPS_CONF} has a section rynas does not manage in the way of this UPS ({}): \
             take it over in the wizard or remove it",
            blocking.join(", ")
        ));
    }
    if !installed_drivers(root).is_empty() && driver_path(root, &c.device.driver).is_none() {
        return Err(format!(
            "the NUT driver {} is not installed",
            c.device.driver
        ));
    }
    let mut gone: Vec<&String> = c.take_over.iter().collect();
    gone.push(&c.name);
    let mode = match read(root, NUT_CONF)
        .and_then(|t| current_mode(&t))
        .as_deref()
    {
        // upsd already serves other machines: keep it so
        Some("netserver") => "netserver",
        _ => "standalone",
    };
    Ok(vec![
        nut_file(
            NUT_UPS_CONF,
            BLOCK,
            nut::render_section(c),
            gone.iter().map(|n| format!("[{n}]")).collect(),
        ),
        nut_file(
            NUT_CONF,
            BLOCK,
            format!("# NUT mode of the UPS set up by rynas (ups.device).\nMODE={mode}\n"),
            vec!["MODE=".into()],
        ),
        nut_file(
            NUT_USERS,
            USERS_BLOCK,
            format!(
                "# rynas's upsmon watches the UPS set up with its wizard as this user.\n\
                 [{MONITOR_USER}]\n\tpassword = \"{password}\"\n\tupsmon primary\n"
            ),
            vec![],
        ),
        nut_file(
            UPSMON_CONF,
            BLOCK,
            format!(
                "# The UPS set up by rynas (ups.device): this system shuts down on its low battery.\n\
                 MONITOR {}@localhost 1 {MONITOR_USER} {password} primary\n",
                c.name
            ),
            gone.iter().map(|n| format!("MONITOR {n}@")).collect(),
        ),
    ])
}

/// Commands an apply runs on plain NUT after the files (also shown in the plan).
pub fn nut_commands(root: &Path, to: Option<&UpsDeviceConfig>) -> Vec<String> {
    let mut out = Vec::new();
    let has = |p: &str| files::under_root(root, Path::new(p)).exists();
    let enumerator = ["/lib/systemd/system", "/usr/lib/systemd/system"]
        .iter()
        .any(|d| has(&format!("{d}/nut-driver@.service")));
    if ["/usr/sbin/upsdrvsvcctl", "/sbin/upsdrvsvcctl"]
        .iter()
        .any(|p| has(p))
        || enumerator
    {
        out.push("upsdrvsvcctl resync".to_string());
    }
    if let Some(c) = to {
        if enumerator {
            out.push("systemctl enable --now nut-driver.target".into());
            // a driver that failed before (old setting at package install) starts clean
            out.push(format!(
                "systemctl reset-failed nut-driver@{}.service",
                c.name
            ));
            out.push(format!("systemctl restart nut-driver@{}.service", c.name));
        } else {
            out.push("systemctl enable --now nut-driver.service".into());
            out.push("systemctl restart nut-driver.service".into());
        }
        out.push("systemctl enable --now nut-server.service".into());
    }
    out.push("systemctl restart nut-server.service".into());
    if to.is_some() {
        out.push("systemctl enable --now nut-monitor.service".into());
    }
    out.push("systemctl restart nut-monitor.service".into());
    out
}

// ---------------------------------------------------------------------------------------------
// the configuration domain
// ---------------------------------------------------------------------------------------------

/// `ups.device`: the UPS set up with the wizard.
pub struct DeviceDomain {
    pub layer: Option<Arc<dyn UpsLayer>>,
}

fn config_of(v: &Value) -> Result<UpsDeviceConfig, Invalid> {
    serde_json::from_value(v.clone())
        .map_err(|e| Invalid::from(format!("invalid ups.device document: {e}")))
}

/// Runs one of [`nut_commands`] through the configuration's `System`.
fn run_nut_command(line: &str, ctx: &ApplyCtx) -> Result<(), String> {
    (ctx.log)(&format!("$ {line}"));
    let w: Vec<&str> = line.split_whitespace().collect();
    match w.as_slice() {
        ["systemctl", "enable", "--now", unit] => ctx.system.set_unit_enabled(unit, true),
        ["systemctl", "restart", unit] => ctx.system.restart_unit(unit),
        ["systemctl", "reset-failed", unit] => {
            // not loaded or not failed: nothing to clear
            if let Err(e) = ctx.system.run(
                &Cmd::new("systemctl")
                    .args(["reset-failed", unit])
                    .env_clear()
                    .timeout(SHORT),
            ) {
                (ctx.log)(&format!("systemctl reset-failed {unit}: {e}"));
            }
            Ok(())
        }
        ["upsdrvsvcctl", "resync"] => {
            // the enumerator also runs from its path unit: a failure here is not fatal
            if let Err(e) = ctx.system.run(
                &Cmd::new("upsdrvsvcctl")
                    .arg("resync")
                    .env_clear()
                    .timeout(UNITS),
            ) {
                (ctx.log)(&format!("upsdrvsvcctl resync: {e}"));
            }
            Ok(())
        }
        _ => Err(format!("unexpected command {line}")),
    }
}

impl Applier for DeviceDomain {
    fn key(&self) -> &str {
        DEVICE_KEY
    }

    /// Before `ups.safemode` (50): the policy's watcher reads the UPS this sets up.
    fn order(&self) -> i32 {
        45
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let mut c = config_of(&value)?;
        c.take_over.sort();
        c.take_over.dedup();
        nut::check_config(&c).map_err(|(f, m)| Invalid::at(&f, m))?;
        serde_json::to_value(c).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        if self.layer.is_some() {
            return Ok(Vec::new());
        }
        let c = config_of(value).map_err(|e| e.message)?;
        let pw = monitor_password(ctx.root)?;
        nut_files(ctx.root, &c, &pw)
    }

    fn reconcile(
        &self,
        _from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        let to = match to {
            Some(v) => Some(config_of(v).map_err(|e| e.message)?),
            None => None,
        };
        if let Some(layer) = &self.layer {
            // a driver that failed before (the layer's old setting) starts clean
            if let Some(c) = &to {
                run_nut_command(
                    &format!("systemctl reset-failed nut-driver@{}.service", c.name),
                    ctx,
                )?;
            }
            let c = layer.setup_cmd(to.as_ref().map(|c| &c.device));
            (ctx.log)(&format!("$ {}", c.display()));
            return ctx.system.run(&c).map(|_| ()).map_err(|e| {
                if e.contains("unknown") || e.contains("usage") {
                    format!("{e} ({})", layer.too_old_message())
                } else {
                    e
                }
            });
        }
        for line in nut_commands(ctx.root, to.as_ref()) {
            run_nut_command(&line, ctx)?;
        }
        Ok(())
    }

    fn import(&self, ctx: &ApplyCtx) -> Result<Option<Value>, String> {
        let c = if let Some(layer) = &self.layer {
            layer
                .current_device(&SystemRunner(ctx.system), &|p| ctx.read(p))
                .map(|device| UpsDeviceConfig {
                    name: UPS_DEVICE_NAME.into(),
                    device,
                    take_over: Vec::new(),
                })
        } else {
            ctx.read(Path::new(NUT_UPS_CONF))
                .and_then(|t| nut::block_of(&t, BLOCK))
                .and_then(|b| nut::parse_section_block(&b))
                .map(|(name, device)| UpsDeviceConfig {
                    name,
                    device,
                    take_over: Vec::new(),
                })
        };
        c.map(|c| serde_json::to_value(c).map_err(|e| e.to_string()))
            .transpose()
    }
}

// ---------------------------------------------------------------------------------------------
// the plan
// ---------------------------------------------------------------------------------------------

/// Hides passwords in a text shown to the browser: rynas's upsmon password, and the password
/// field of any `MONITOR` line or `password =` line (the admin's, in removed lines).
fn mask(text: &str, password: &str) -> String {
    let text = if password.is_empty() {
        text.to_string()
    } else {
        text.replace(password, "********")
    };
    text.lines()
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("MONITOR ") {
                let mut w: Vec<&str> = t.split_whitespace().collect();
                if w.len() > 4 {
                    w[4] = "********";
                }
                w.join(" ")
            } else if t.starts_with("password") && t.contains('=') {
                let indent = &l[..l.len() - t.len()];
                format!("{indent}password = \"********\"")
            } else {
                l.to_string()
            }
        })
        .map(|l| l + "\n")
        .collect::<String>()
        .trim_end_matches('\n')
        .to_string()
        + if text.ends_with('\n') { "\n" } else { "" }
}

/// What applying `c` does (no side effects; a missing upsmon password is not generated).
pub fn plan(root: &Path, layer: Option<&dyn UpsLayer>, c: &UpsDeviceConfig) -> UpsSetupPlan {
    if let Some(l) = layer {
        return UpsSetupPlan {
            commands: vec![l.setup_cmd(Some(&c.device)).display()],
            ..Default::default()
        };
    }
    let conflicts = nut::blocking_sections(&foreign_sections(root), c);
    let pw = read(root, MONITOR_SECRET)
        .and_then(|t| t.lines().next().map(str::to_string))
        .unwrap_or_default();
    let shown_pw = if pw.is_empty() { "PASSWORD" } else { &pw };
    // the plan renders without the refusal, which `conflicts` reports
    let c_free = UpsDeviceConfig {
        take_over: c
            .take_over
            .iter()
            .cloned()
            .chain(conflicts.iter().cloned())
            .collect(),
        ..c.clone()
    };
    let files = nut_files(root, &c_free, shown_pw)
        .unwrap_or_default()
        .into_iter()
        .map(|f| {
            let path = f.path.to_string_lossy().into_owned();
            let existing = read(root, &path);
            let FileKind::Block { id, drop_outside } = &f.kind else {
                unreachable!("NUT files are blocks")
            };
            let before = existing.as_deref().and_then(|t| nut::block_of(t, id));
            // lines of the admin's that the new file no longer has
            let removed = existing
                .as_deref()
                .map(|t| {
                    let (new, _) = files::render_final(
                        &FileKind::Block {
                            id: id.clone(),
                            drop_outside: drop_outside.clone(),
                        },
                        &f.content,
                        Some(t),
                    );
                    let kept: std::collections::HashSet<&str> = new.lines().collect();
                    let old_block = before.clone().unwrap_or_default();
                    let old_block: std::collections::HashSet<&str> = old_block.lines().collect();
                    t.lines()
                        .filter(|l| {
                            !l.trim().is_empty() && !kept.contains(l) && !old_block.contains(l)
                        })
                        .filter(|l| {
                            !l.starts_with("# BEGIN rynas ") && !l.starts_with("# END rynas ")
                        })
                        .map(|l| mask(l, &pw))
                        .collect()
                })
                .unwrap_or_default();
            UpsPlanFile {
                path,
                before: before.map(|b| mask(&b, &pw)),
                after: mask(&f.content, &pw).replace("PASSWORD", "********"),
                removed,
            }
        })
        .collect();
    UpsSetupPlan {
        files,
        commands: nut_commands(root, Some(c)),
        conflicts,
    }
}

// ---------------------------------------------------------------------------------------------
// endpoints
// ---------------------------------------------------------------------------------------------

pub fn router(st: UpsState) -> Router {
    Router::new()
        .route("/v1/ups/device", get(device_get).put(device_put))
        .route("/v1/ups/detect", post(detect_post))
        .route("/v1/ups/drivers", get(drivers_get))
        .route("/v1/ups/plan/device", post(plan_post))
        .with_state(st)
}

fn ups(st: &UpsState) -> Res<Arc<Ups>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "configuration and tasks are not enabled in this rynasd".into(),
        )
    })
}

async fn blocking<T: Send + 'static>(
    u: &Arc<Ups>,
    f: impl FnOnce(&Ups) -> Res<T> + Send + 'static,
) -> Res<T> {
    let u = u.clone();
    tokio::task::spawn_blocking(move || f(&u))
        .await
        .map_err(Error::internal)?
}

fn parse_doc(v: Option<Value>) -> Res<Option<UpsDeviceConfig>> {
    v.map(|v| serde_json::from_value(v).map_err(Error::internal))
        .transpose()
}

fn device_state(u: &Ups, owner: &str) -> Res<UpsDeviceState> {
    let backend = u.backend();
    let c = &u.ops.config;
    let applied = parse_doc(c.current(DEVICE_KEY)?)?;
    let pending = match c.pending_value(owner, DEVICE_KEY)? {
        Some(v) => parse_doc(v)?.filter(|p| Some(p) != applied.as_ref()),
        None => None,
    };
    let blocked = u.setup_backend().blocked();
    let foreign = if u.layer.is_some() || blocked.is_some() {
        Vec::new()
    } else {
        nut::mark_conflicts(
            &foreign_sections(&u.root),
            pending.as_ref().or(applied.as_ref()),
        )
    };
    Ok(UpsDeviceState {
        backend,
        blocked,
        nut_version: if blocked.is_some() {
            None
        } else {
            cached_version(u)
        },
        applied,
        pending,
        foreign,
        drivers_running: blocked.is_none() && !running_drivers(u.setup.runner.as_ref()).is_empty(),
        detect: u.setup.report.lock().unwrap().clone(),
    })
}

async fn device_get(State(st): State<UpsState>, hd: HeaderMap) -> Res<Json<UpsDeviceState>> {
    let (u, owner) = (ups(&st)?, user(&hd)?);
    Ok(Json(blocking(&u, move |u| device_state(u, &owner)).await?))
}

fn check_put(u: &Ups, c: &UpsDeviceConfig) -> Res<()> {
    nut::check_config(c).map_err(|(f, m)| Error::invalid(f, m))?;
    let backend = u.setup_backend();
    if let Some(b) = backend.blocked() {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            backend.block_text(b).into(),
        ));
    }
    if u.layer.is_some() {
        return Ok(());
    }
    if !installed_drivers(&u.root).is_empty() && driver_path(&u.root, &c.device.driver).is_none() {
        return Err(Error::invalid(
            "device.driver",
            "this NUT driver is not installed",
        ));
    }
    let blocking = nut::blocking_sections(&foreign_sections(&u.root), c);
    if !blocking.is_empty() {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            format!(
                "ups.conf sections in the way: {} (take them over or remove them)",
                blocking.join(", ")
            ),
        )
        .field("take_over"));
    }
    Ok(())
}

async fn device_put(
    State(st): State<UpsState>,
    hd: HeaderMap,
    body: Result<Json<UpsDeviceConfig>, JsonRejection>,
) -> Res<Json<UpsDeviceState>> {
    let (u, owner) = (ups(&st)?, user(&hd)?);
    let Json(req) = body?;
    Ok(Json(
        blocking(&u, move |u| {
            check_put(u, &req)?;
            crate::services::stage(&u.ops.config, &owner, DEVICE_KEY, &req)?;
            device_state(u, &owner)
        })
        .await?,
    ))
}

async fn plan_post(
    State(st): State<UpsState>,
    body: Result<Json<UpsDeviceConfig>, JsonRejection>,
) -> Res<Json<UpsSetupPlan>> {
    let u = ups(&st)?;
    let Json(req) = body?;
    nut::check_config(&req).map_err(|(f, m)| Error::invalid(f, m))?;
    Ok(Json(
        blocking(&u, move |u| {
            Ok(plan(&u.root, u.layer.as_deref(), &req))
        })
        .await?,
    ))
}

async fn drivers_get(State(st): State<UpsState>) -> Res<Json<Vec<NutDriver>>> {
    let u = ups(&st)?;
    Ok(Json(
        blocking(&u, |u| {
            let list = read(&u.root, DRIVER_LIST).unwrap_or_default();
            Ok(nut::drivers_with_hcl(&installed_drivers(&u.root), &list))
        })
        .await?,
    ))
}

async fn detect_post(
    State(st): State<UpsState>,
    hd: HeaderMap,
    body: Result<Json<UpsDetectRequest>, JsonRejection>,
) -> Res<Response> {
    let (u, who) = (ups(&st)?, user(&hd)?);
    let Json(req) = body?;
    if let Some(d) = &req.device {
        nut::check_device(d).map_err(|(f, m)| Error::invalid(f, m))?;
    }
    {
        let mut b = u.setup.busy.lock().unwrap();
        if *b {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::BUSY,
                "a UPS detection is already running".into(),
            ));
        }
        *b = true;
    }
    let u2 = u.clone();
    let spawned = u
        .ops
        .tasks
        .spawn("ups.detect", &who, move |ctx| async move {
            let u3 = u2.clone();
            let ctx2 = ctx.clone();
            let r = tokio::task::spawn_blocking(move || {
                let log = |l: &str| ctx2.log(l);
                let progress = |p: u8| ctx2.progress(p);
                let cancelled = || ctx2.is_cancelled();
                detect(
                    u3.setup_backend().as_ref(),
                    u3.setup.runner.as_ref(),
                    &req,
                    PROBE_TIMEOUT,
                    &log,
                    &progress,
                    &cancelled,
                    rynas_tasks::now_ms(),
                )
            })
            .await
            .map_err(|e| e.to_string());
            *u2.setup.busy.lock().unwrap() = false;
            let report = r??;
            *u2.setup.report.lock().unwrap() = Some(report);
            Ok(())
        });
    let id = match spawned {
        Ok(id) => id,
        Err(e) => {
            *u.setup.busy.lock().unwrap() = false;
            return Err(e.into());
        }
    };
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers by program name and a substring of the command line.
    struct Fake {
        rules: Vec<(&'static str, &'static str, i32, &'static str, &'static str)>,
        calls: Mutex<Vec<String>>,
    }

    impl Runner for Fake {
        fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
            let line = cmd.display();
            self.calls.lock().unwrap().push(line.clone());
            let prog = Path::new(cmd.program())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            for (p, sub, code, out, err) in &self.rules {
                if *p == prog && line.contains(sub) {
                    return Ok(CmdOutput {
                        code: Some(*code),
                        stdout: (*out).into(),
                        stderr: (*err).into(),
                    });
                }
            }
            Ok(CmdOutput {
                code: Some(1),
                stdout: String::new(),
                stderr: "No supported UPS detected".into(),
            })
        }
    }

    const SCAN: &str = "[nutdev1]\n\tdriver = \"richcomm_usb\"\n\tport = \"auto\"\n\tvendorid = \"0925\"\n\tproductid = \"1234\"\n";
    const OK_VARS: &str = "battery.charge: 87\nups.status: OL CHRG\ndevice.model: UPS1050VA-PRO\n";

    fn nut_root(drivers: &[&str]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let etc = d.path().join("etc/nut");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("nut.conf"), "MODE=none\n").unwrap();
        std::fs::write(etc.join("ups.conf"), "maxretry = 3\n").unwrap();
        std::fs::write(etc.join("upsd.users"), "# users\n").unwrap();
        std::fs::write(
            etc.join("upsmon.conf"),
            "SHUTDOWNCMD \"/sbin/shutdown -h +0\"\n",
        )
        .unwrap();
        let lib = d.path().join("lib/nut");
        std::fs::create_dir_all(&lib).unwrap();
        for dr in drivers {
            std::fs::write(lib.join(dr), "").unwrap();
        }
        d
    }

    fn run_detect(
        root: &Path,
        fake: &Fake,
        req: &UpsDetectRequest,
    ) -> (UpsDetectReport, Vec<String>) {
        let lines = Mutex::new(Vec::new());
        let b = NutBackend { root, runner: fake };
        let r = detect(
            &b,
            fake,
            req,
            Duration::from_secs(5),
            &|l| lines.lock().unwrap().push(l.to_string()),
            &|_| {},
            &|| false,
            1,
        )
        .unwrap();
        (r, lines.into_inner().unwrap())
    }

    #[test]
    fn vultech_on_new_nut_finds_nutdrv_qx_armac_first() {
        let root = nut_root(&["nutdrv_qx", "richcomm_usb", "usbhid-ups", "blazer_usb"]);
        let fake = Fake {
            rules: vec![
                (
                    "upsdrvctl",
                    "-V",
                    0,
                    "Network UPS Tools - UPS driver controller 2.8.5\n",
                    "",
                ),
                ("nut-scanner", "-U", 0, SCAN, ""),
                ("nutdrv_qx", "subdriver=armac", 0, OK_VARS, ""),
            ],
            calls: Mutex::new(Vec::new()),
        };
        let (r, _) = run_detect(root.path(), &fake, &UpsDetectRequest::default());
        assert_eq!(r.found, Some(0));
        let c = &r.candidates[0];
        assert_eq!(c.device.driver, "nutdrv_qx");
        assert_eq!(c.probe.status.as_deref(), Some("OL CHRG"));
        assert_eq!(c.probe.battery_charge_pct, Some(87));
        assert!(
            r.candidates[1..]
                .iter()
                .all(|c| c.probe.state == UpsProbeState::NotRun)
        );
        let calls = fake.calls.lock().unwrap();
        let probe = calls.iter().find(|c| c.contains("nutdrv_qx")).unwrap();
        assert!(probe.ends_with("lib/nut/nutdrv_qx -s rynasprobe -x port=auto -x vendorid=0925 -x productid=1234 -x subdriver=armac -x protocol=megatec -x norating -x pollfreq=30 -d 1"), "{probe}");
    }

    #[test]
    fn vultech_on_old_nut_says_why_nothing_works() {
        let root = nut_root(&["nutdrv_qx", "richcomm_usb", "usbhid-ups", "blazer_usb"]);
        let fake = Fake {
            rules: vec![
                (
                    "upsdrvctl",
                    "-V",
                    0,
                    "Network UPS Tools - UPS driver controller 2.8.1\n",
                    "",
                ),
                ("nut-scanner", "-U", 0, SCAN, ""),
                (
                    "richcomm_usb",
                    "",
                    1,
                    "",
                    "driver callback failed: Entity not found\n",
                ),
                ("nutdrv_qx", "", 1, "", "Device not supported!\n"),
            ],
            calls: Mutex::new(Vec::new()),
        };
        let (r, log) = run_detect(root.path(), &fake, &UpsDetectRequest::default());
        assert_eq!(r.found, None);
        assert_eq!(r.nut_version.as_deref(), Some("2.8.1"));
        let states: Vec<UpsProbeState> = r.candidates.iter().map(|c| c.probe.state).collect();
        assert_eq!(states[0], UpsProbeState::NeedsNewerNut);
        assert!(
            states[1..].iter().all(|s| *s == UpsProbeState::Failed),
            "{states:?}"
        );
        assert_eq!(
            r.candidates[1].probe.log_tail.last().map(String::as_str),
            Some("driver callback failed: Entity not found")
        );
        assert!(log.iter().any(|l| l.contains("needs NUT 2.8.5 or newer")));
        // the armac candidate was not even started
        assert!(
            !fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.contains("subdriver=armac"))
        );
    }

    #[test]
    fn running_driver_is_reported_then_stopped_and_restarted_on_request() {
        let root = nut_root(&["usbhid-ups"]);
        let rules = vec![
            ("upsdrvctl", "-V", 0, "2.8.5", ""),
            (
                "systemctl",
                "list-units",
                0,
                "nut-driver@ups.service loaded active running UPS driver\n",
                "",
            ),
            ("systemctl", "stop", 0, "", ""),
            ("systemctl", "start", 0, "", ""),
            ("usbhid-ups", "", 0, OK_VARS, ""),
        ];
        let manual = UpsDevice {
            driver: "usbhid-ups".into(),
            port: "auto".into(),
            options: vec![],
        };
        let fake = Fake {
            rules: rules.clone(),
            calls: Mutex::new(Vec::new()),
        };
        let (r, _) = run_detect(
            root.path(),
            &fake,
            &UpsDetectRequest {
                device: Some(manual.clone()),
                stop_drivers: false,
            },
        );
        assert!(r.busy && r.found.is_none());
        assert!(
            !fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.contains("usbhid-ups -s"))
        );
        let fake = Fake {
            rules,
            calls: Mutex::new(Vec::new()),
        };
        let (r, _) = run_detect(
            root.path(),
            &fake,
            &UpsDetectRequest {
                device: Some(manual),
                stop_drivers: true,
            },
        );
        assert_eq!(r.found, Some(0));
        assert_eq!(r.candidates[0].source, UpsCandidateSource::Manual);
        let calls = fake.calls.lock().unwrap().clone();
        let pos = |s: &str| {
            calls
                .iter()
                .position(|c| c.contains(s))
                .unwrap_or_else(|| panic!("{s}: {calls:?}"))
        };
        assert!(pos("systemctl stop nut-driver@ups.service") < pos("usbhid-ups -s"));
        assert!(pos("usbhid-ups -s") < pos("systemctl start nut-driver@ups.service"));
    }

    #[test]
    fn missing_driver_and_no_usb() {
        let root = nut_root(&["usbhid-ups"]);
        let fake = Fake {
            rules: vec![("nut-scanner", "-U", 0, "", "")],
            calls: Mutex::new(Vec::new()),
        };
        let (r, log) = run_detect(root.path(), &fake, &UpsDetectRequest::default());
        assert!(r.candidates.is_empty() && log.iter().any(|l| l == "no USB UPS found"));
        let manual = UpsDevice {
            driver: "snmp-ups".into(),
            port: "10.0.0.5".into(),
            options: vec![UpsOption::new("community", Some("public"))],
        };
        let (r, _) = run_detect(
            root.path(),
            &fake,
            &UpsDetectRequest {
                device: Some(manual),
                stop_drivers: false,
            },
        );
        assert_eq!(r.candidates[0].probe.state, UpsProbeState::NoDriver);
    }

    /// A module layer of the tests: the tool `layer-ups` with `probe`, `setup` and `status`.
    struct TestLayer {
        old: bool,
    }

    const LAYER: &str = "/usr/sbin/layer-ups";

    impl UpsLayer for TestLayer {
        fn policy_path(&self) -> &'static str {
            "/etc/layer/ups.conf"
        }
        fn render_policy(&self, s: &UpsSettings) -> ManagedFile {
            ManagedFile::whole(
                "/etc/layer/ups.conf",
                format!("AFTER={}\n", s.safemode_after_s.unwrap_or(0)),
                false,
            )
        }
        fn parse_policy(&self, _text: &str) -> UpsSettings {
            UpsSettings::default()
        }
        fn policy_note(&self) -> &'static str {
            "read at the next event"
        }
        fn stop_notice(&self, reason: &str) -> (String, String) {
            (format!("Layer stop: {reason}"), String::new())
        }
        fn test_cmd(&self, kind: BatteryTest) -> Cmd {
            Cmd::new(LAYER).args(["test", kind.arg()])
        }
        fn result_cmd(&self) -> Cmd {
            Cmd::new(LAYER).args(["test", "result"])
        }
        fn parse_result(&self, stdout: &str) -> String {
            stdout.trim().into()
        }
        fn setup_blocked(&self, _runner: &dyn Runner) -> Option<UpsSetupBlock> {
            self.old.then_some(UpsSetupBlock::ModuleTooOld)
        }
        fn too_old_message(&self) -> &'static str {
            "update the layer"
        }
        fn probe_cmd(&self, d: &UpsDevice, timeout: Duration) -> Cmd {
            Cmd::new(LAYER).args(["probe", &d.driver]).timeout(timeout)
        }
        fn probe_exit_state(&self, code: Option<i32>) -> Option<UpsProbeState> {
            (code == Some(3)).then_some(UpsProbeState::Busy)
        }
        fn setup_cmd(&self, to: Option<&UpsDevice>) -> Cmd {
            match to {
                Some(d) => Cmd::new(LAYER).args(["setup", &d.driver]),
                None => Cmd::new(LAYER).args(["setup", "--default"]),
            }
        }
        fn current_device(
            &self,
            runner: &dyn Runner,
            _read: &dyn Fn(&Path) -> Option<String>,
        ) -> Option<UpsDevice> {
            let o = runner.run(&Cmd::new(LAYER).arg("status")).ok()?;
            o.success().then(|| UpsDevice {
                driver: o.stdout.trim().into(),
                port: "auto".into(),
                options: Vec::new(),
            })
        }
    }

    #[test]
    fn module_layer_probe_gate_and_plan() {
        let fake = Fake {
            rules: vec![
                ("layer-ups", "probe nutdrv_qx", 0, OK_VARS, ""),
                ("layer-ups", "probe usbhid-ups", 3, "", "held\n"),
            ],
            calls: Mutex::new(Vec::new()),
        };
        let old = TestLayer { old: true };
        let b = LayerBackend {
            layer: &old,
            runner: &fake,
        };
        assert_eq!(b.blocked(), Some(UpsSetupBlock::ModuleTooOld));
        let e = detect(
            &b,
            &fake,
            &UpsDetectRequest::default(),
            Duration::from_secs(5),
            &|_| {},
            &|_| {},
            &|| false,
            1,
        )
        .unwrap_err();
        assert_eq!(e, "update the layer");
        let layer = TestLayer { old: false };
        let b = LayerBackend {
            layer: &layer,
            runner: &fake,
        };
        assert_eq!(b.blocked(), None);
        let d = device_cfg().device;
        assert_eq!(b.probe(&d, Duration::from_secs(5)).state, UpsProbeState::Ok);
        let busy = UpsDevice {
            driver: "usbhid-ups".into(),
            ..d.clone()
        };
        assert_eq!(b.probe(&busy, Duration::from_secs(5)).state, UpsProbeState::Busy);
        let root = tempfile::tempdir().unwrap();
        let p = plan(root.path(), Some(&layer), &device_cfg());
        assert_eq!(p.commands, ["/usr/sbin/layer-ups setup nutdrv_qx"]);
        assert!(p.files.is_empty());
    }

    #[test]
    fn drivers_in_libexec_from_nut_2_8_2() {
        let root = tempfile::tempdir().unwrap();
        let libexec = root.path().join("usr/libexec/nut");
        std::fs::create_dir_all(&libexec).unwrap();
        std::fs::write(libexec.join("nutdrv_qx"), "").unwrap();
        std::fs::write(libexec.join("nut-driver-enumerator.sh"), "").unwrap();
        assert_eq!(installed_drivers(root.path()), ["nutdrv_qx"]);
        assert!(
            driver_path(root.path(), "nutdrv_qx")
                .unwrap()
                .ends_with("usr/libexec/nut/nutdrv_qx")
        );
        assert!(driver_path(root.path(), "../nutdrv_qx").is_none());
    }

    #[test]
    fn dry_runner_walks_the_vultech_case() {
        let root = nut_root(&["nutdrv_qx", "richcomm_usb", "usbhid-ups", "blazer_usb"]);
        let dry = DryRunner::default();
        let b = NutBackend {
            root: root.path(),
            runner: &dry,
        };
        let r = detect(
            &b,
            &dry,
            &UpsDetectRequest::default(),
            Duration::from_secs(5),
            &|_| {},
            &|_| {},
            &|| false,
            1,
        )
        .unwrap();
        assert_eq!(r.found, Some(0));
    }

    fn device_cfg() -> UpsDeviceConfig {
        UpsDeviceConfig {
            name: "ups".into(),
            device: nut::candidates(&nut::parse_scanner(SCAN)).remove(0).device,
            take_over: vec![],
        }
    }

    fn open(
        root: &Path,
        layer: Option<Arc<dyn UpsLayer>>,
    ) -> (
        tempfile::TempDir,
        rynas_config::Config,
        Arc<rynas_config::MemorySystem>,
    ) {
        let state = tempfile::tempdir().unwrap();
        let sys = Arc::new(rynas_config::MemorySystem::new("nas"));
        let domains: Vec<Arc<dyn Applier>> = vec![Arc::new(DeviceDomain { layer })];
        let cfg = rynas_config::Config::open(
            rynas_config::ConfigOptions::new(state.path(), root),
            sys.clone(),
            domains,
        )
        .unwrap();
        (state, cfg, sys)
    }

    fn apply(cfg: &rynas_config::Config) -> Result<u64, String> {
        cfg.apply("admin", "admin", "ups", &|_| {}, &|| false)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn nut_apply_writes_only_rynas_blocks_and_restarts_nut() {
        let root = nut_root(&["nutdrv_qx"]);
        std::fs::write(
            root.path().join("etc/nut/upsmon.conf"),
            "MONITOR remote@10.0.0.9 1 u p secondary\nSHUTDOWNCMD \"/sbin/shutdown -h +0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("lib/systemd/system")).unwrap();
        std::fs::write(
            root.path().join("lib/systemd/system/nut-driver@.service"),
            "",
        )
        .unwrap();
        let (_s, cfg, sys) = open(root.path(), None);
        cfg.stage(
            "admin",
            DEVICE_KEY,
            Some(serde_json::to_value(device_cfg()).unwrap()),
        )
        .unwrap();
        apply(&cfg).unwrap();
        let rd = |p: &str| std::fs::read_to_string(root.path().join(p)).unwrap();
        let ups = rd("etc/nut/ups.conf");
        assert!(
            ups.starts_with("maxretry = 3\n# BEGIN rynas ups-device"),
            "{ups}"
        );
        assert!(ups.contains("\tsubdriver = \"armac\"\n\tprotocol = \"megatec\"\n\tnorating\n"));
        assert!(
            rd("etc/nut/nut.conf").contains("MODE=standalone")
                && !rd("etc/nut/nut.conf").contains("MODE=none")
        );
        let pw = rd("etc/rynas/nut-upsmon.secret").trim().to_string();
        assert!(rd("etc/nut/upsd.users").contains(&format!(
            "[rynas-upsmon]\n\tpassword = \"{pw}\"\n\tupsmon primary"
        )));
        let mon = rd("etc/nut/upsmon.conf");
        assert!(mon.contains(&format!(
            "MONITOR ups@localhost 1 rynas-upsmon {pw} primary"
        )));
        assert!(
            mon.contains("MONITOR remote@10.0.0.9"),
            "the admin's other UPS stays"
        );
        let calls = sys.state().calls.clone();
        for want in [
            "run upsdrvsvcctl resync",
            "run systemctl enable --now nut-driver.target",
            "run systemctl reset-failed nut-driver@ups.service",
            "run systemctl restart nut-driver@ups.service",
            "run systemctl restart nut-server.service",
            "run systemctl enable --now nut-monitor.service",
        ] {
            assert!(calls.iter().any(|c| c == want), "{want}: {calls:?}");
        }
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(root.path().join("etc/nut/upsd.users"))
                .unwrap()
                .permissions(),
        );
        assert_eq!(mode & 0o777, 0o640);
        // import reads the block back
        let (name, device) =
            nut::parse_section_block(&nut::block_of(&ups, BLOCK).unwrap()).unwrap();
        assert_eq!(
            UpsDeviceConfig {
                name,
                device,
                take_over: vec![]
            },
            device_cfg()
        );
    }

    #[test]
    fn foreign_section_refuses_until_taken_over() {
        let root = nut_root(&["nutdrv_qx"]);
        std::fs::write(
            root.path().join("etc/nut/ups.conf"),
            "[myups]\n\tdriver = usbhid-ups\n\tport = auto\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("etc/nut/upsmon.conf"),
            "MONITOR myups@localhost 1 admin secret primary\n",
        )
        .unwrap();
        std::fs::write(root.path().join("etc/nut/nut.conf"), "MODE=netserver\n").unwrap();
        let (_s, cfg, _sys) = open(root.path(), None);
        let mut c = device_cfg();
        let p = plan(root.path(), None, &c);
        assert_eq!(p.conflicts, ["myups"]);
        cfg.stage("admin", DEVICE_KEY, Some(serde_json::to_value(&c).unwrap()))
            .unwrap();
        let e = apply(&cfg).unwrap_err();
        assert!(e.contains("myups"), "{e}");
        assert!(
            std::fs::read_to_string(root.path().join("etc/nut/ups.conf"))
                .unwrap()
                .contains("[myups]")
        );
        c.take_over = vec!["myups".into()];
        let p = plan(root.path(), None, &c);
        assert!(p.conflicts.is_empty());
        let ups_file = p.files.iter().find(|f| f.path == NUT_UPS_CONF).unwrap();
        assert!(
            ups_file.removed.iter().any(|l| l == "[myups]"),
            "{ups_file:?}"
        );
        assert!(p.files.iter().all(|f| {
            !f.after.contains("PASSWORD")
                && !f
                    .removed
                    .iter()
                    .any(|l| l.contains("secret") && !l.contains("********"))
        }));
        cfg.stage("admin", DEVICE_KEY, Some(serde_json::to_value(&c).unwrap()))
            .unwrap();
        apply(&cfg).unwrap();
        let rd = |p: &str| std::fs::read_to_string(root.path().join(p)).unwrap();
        assert!(!rd("etc/nut/ups.conf").contains("myups"));
        assert!(!rd("etc/nut/upsmon.conf").contains("MONITOR myups@"));
        assert!(
            rd("etc/nut/nut.conf").contains("MODE=netserver"),
            "a network server stays one"
        );
    }

    #[test]
    fn module_layer_apply_runs_its_setup_and_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let (_s, cfg, sys) = open(root.path(), Some(Arc::new(TestLayer { old: false })));
        cfg.stage(
            "admin",
            DEVICE_KEY,
            Some(serde_json::to_value(device_cfg()).unwrap()),
        )
        .unwrap();
        apply(&cfg).unwrap();
        assert!(!root.path().join("etc/nut").exists());
        let calls = sys.state().calls.clone();
        assert!(calls.iter().any(|c| c == "run /usr/sbin/layer-ups setup nutdrv_qx"), "{calls:?}");
        assert!(calls.iter().any(|c| c.contains("reset-failed nut-driver@ups.service")), "{calls:?}");
    }
}
