//! Wake-on-LAN and device power (`/v1/wol/…`, public under `/api/v1/wol/…`;
//! `docs/research/wake-on-lan.md`, API in `docs/phase1/plan.md` "API v1: Wake-on-LAN").
//!
//! - **Devices** live in the config key `wol` ([`WolConfig`], [`WolDomain`]): revisions,
//!   export and rollback like every domain. The routes write it directly
//!   ([`rynas_config::Config::apply_update`], one revision per change), administrators only.
//! - **Status** ([`probe`]): the linked PC agent's contact, a TCP port, ping, the neighbor
//!   table; checked every 60 s, every 5 s while someone looked at the page in the last 30 s,
//!   every second during a wake. Runtime state (status, last address, wake history) in
//!   `<state>/wol/state.json`.
//! - **Wake** = a task `wol.wake` ([`send`]): the magic packet on the chosen interfaces three
//!   times a second apart, again after 30 s and 60 s while the device stays off, then the wait
//!   for it. One running wake per device; a second request joins it.
//! - **Sleep and enable** only through the paired agent (commands in its plan, agents from
//!   0.8.26, [`crate::pc_agent::power_command`](crate::pc_agent::PcAgent::power_command)).
//! - **Wake for backups**: a PC whose backup is due inside its window and that is off is woken
//!   once per due time; its plan says so ([`rynas_api_types::pcagent::AgentPlan::woken_ms`]).
//!   A wake nobody watched that fails notifies (`wol.wake_failed`), the next good wake
//!   resolves it (`wol.wake_resumed`).
//!
//! Users who are not administrators see, wake and put to sleep only the devices that list
//! them in `users`.

pub mod probe;
pub mod send;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::Ipv4Addr;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use rynas_api_types::error_codes;
use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
use rynas_api_types::pcagent::{AgentCommand, AgentFacts, SleepMode};
use rynas_api_types::wol::*;
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile};
use rynas_notify::Notifier;
use rynas_notify::event::{code, field, firing, resolved};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use self::probe::{Neighbor, Seen};
use self::send::NasIface;
use crate::ops::{Error, Ops, Res, user};
use crate::pc_agent::{Dev, PcAgent};

/// Most devices.
pub const MAX_DEVICES: usize = 256;
/// Most MACs, TCP ports and users of one device.
pub const MAX_MACS: usize = 8;
pub const MAX_PORTS: usize = 16;
pub const MAX_USERS: usize = 64;
/// Wakes kept per device, newest first.
pub const HISTORY: usize = 20;
/// Someone looked at the page this recently: checks every [`FAST`].
pub const WATCH_MS: u64 = 30_000;
pub const FAST: Duration = Duration::from_secs(5);
pub const SLOW: Duration = Duration::from_secs(60);
/// The NAS interfaces are read again after this long.
const IFACES_TTL: Duration = Duration::from_secs(10);
/// Resends of a wake while the device stays off (seconds after the first).
const RESEND_AT: [u64; 2] = [30, 60];
/// User of the wakes rynasd starts itself (wake for backups).
pub const SYSTEM_USER: &str = "rynas";
/// A last-seen address is forgotten after a week.
pub const IP_TTL_MS: u64 = 7 * 86_400_000;
/// A host name's address is resolved again after this long.
const DNS_TTL: Duration = Duration::from_secs(60);
/// Wake for backups: at least this long between two wakes of a device (besides once a day).
pub const BACKUP_WAKE_GAP_MS: u64 = 30 * 60_000;

// ---------------------------------------------------------------------------------------------
// validation
// ---------------------------------------------------------------------------------------------

/// A device id: `[a-z0-9-]`, 1–32, not starting with `-`.
pub fn valid_id(id: &str) -> bool {
    rynas_api_types::pcagent::valid_device(id)
}

/// An id from a name: lower case letters and digits, other characters as one `-`.
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            s.push(c);
        } else if !s.ends_with('-') {
            s.push('-');
        }
    }
    let mut s: String = s.trim_matches('-').chars().take(32).collect();
    while s.ends_with('-') {
        s.pop();
    }
    if s.is_empty() { "device".to_string() } else { s }
}

/// `base`, or `base-2`, `base-3`… (within 32 characters) when taken.
pub fn unique_id(base: &str, taken: &dyn Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    for n in 2.. {
        let suffix = format!("-{n}");
        let mut head: String = base.chars().take(32 - suffix.len()).collect();
        while head.ends_with('-') {
            head.pop();
        }
        let id = format!("{head}{suffix}");
        if !taken(&id) {
            return id;
        }
    }
    unreachable!()
}

fn valid_host(h: &str) -> bool {
    h.len() <= 253
        && (h.parse::<Ipv4Addr>().is_ok()
            || h.split('.').all(|l| {
                !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }))
}

/// A SecureOn password in its stored form (`aa:bb:cc:dd:ee:ff` or `a.b.c.d`).
fn normalize_secureon(s: &str) -> Option<String> {
    let b = send::secureon_bytes(s.trim())?;
    Some(if b.len() == 6 {
        b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(":")
    } else {
        b.iter().map(u8::to_string).collect::<Vec<_>>().join(".")
    })
}

/// Checks one device and gives it in its stored form (MACs normalized, lists deduplicated).
/// The fields are named as in the device's JSON. The id is checked only when set.
pub fn normalize_device(mut d: WolDevice) -> Result<WolDevice, Invalid> {
    d.id = d.id.trim().to_string();
    if !d.id.is_empty() && !valid_id(&d.id) {
        return Err(Invalid::at("id", "1-32 lower case letters, digits and -"));
    }
    d.name = d.name.trim().to_string();
    if d.name.is_empty() || d.name.chars().count() > 64 || d.name.chars().any(char::is_control) {
        return Err(Invalid::at("name", "1-64 characters, no control characters"));
    }
    if d.macs.is_empty() || d.macs.len() > MAX_MACS {
        return Err(Invalid::at("macs", format!("1-{MAX_MACS} MAC addresses")));
    }
    let mut macs: Vec<String> = Vec::new();
    for m in &d.macs {
        let n = normalize_mac(m).ok_or_else(|| Invalid::at("macs", format!("{m} is not a unicast MAC address")))?;
        if !macs.contains(&n) {
            macs.push(n);
        }
    }
    d.macs = macs;
    d.host = d.host.map(|h| h.trim().to_string()).filter(|h| !h.is_empty());
    if d.host.as_deref().is_some_and(|h| !valid_host(h)) {
        return Err(Invalid::at("host", "an IPv4 address or a host name"));
    }
    d.iface = d.iface.map(|i| i.trim().to_string()).filter(|i| !i.is_empty());
    if d.iface.as_deref().is_some_and(|i| !rynas_net::networkd::valid_iface(i)) {
        return Err(Invalid::at("iface", "not an interface name"));
    }
    d.secureon = match d.secureon.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => Some(normalize_secureon(s).ok_or_else(|| Invalid::at("secureon", "6 bytes as aa:bb:cc:dd:ee:ff or 4 as a.b.c.d"))?),
    };
    if d.ports.len() > MAX_PORTS || d.ports.contains(&0) {
        return Err(Invalid::at("ports", format!("at most {MAX_PORTS} TCP ports, 1-65535")));
    }
    d.ports.sort_unstable();
    d.ports.dedup();
    if !(30..=900).contains(&d.wake_timeout_s) {
        return Err(Invalid::at("wake_timeout_s", "30-900 seconds"));
    }
    d.pc_device = d.pc_device.map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
    if d.pc_device.as_deref().is_some_and(|p| !rynas_api_types::pcagent::valid_device(p)) {
        return Err(Invalid::at("pc_device", "not a PC name"));
    }
    if d.users.len() > MAX_USERS || d.users.iter().any(|u| !rynas_auth::valid_username(u)) {
        return Err(Invalid::at("users", format!("at most {MAX_USERS} valid user names")));
    }
    d.users.sort();
    d.users.dedup();
    if d.notes.chars().count() > 1000 {
        return Err(Invalid::at("notes", "at most 1000 characters"));
    }
    Ok(d)
}

/// Checks a whole `wol` document: every device ([`normalize_device`]), ids made from the names
/// where empty, unique ids, MACs and PC links.
pub fn normalize_config(c: WolConfig) -> Result<WolConfig, Invalid> {
    if c.devices.len() > MAX_DEVICES {
        return Err(Invalid::at("devices", format!("at most {MAX_DEVICES} devices")));
    }
    let at = |i: usize, e: Invalid| Invalid {
        field: Some(format!("devices.{i}.{}", e.field.as_deref().unwrap_or("name"))),
        message: e.message,
    };
    let mut out: Vec<WolDevice> = Vec::with_capacity(c.devices.len());
    for (i, d) in c.devices.into_iter().enumerate() {
        let mut d = normalize_device(d).map_err(|e| at(i, e))?;
        if d.id.is_empty() {
            d.id = unique_id(&slug(&d.name), &|x| out.iter().any(|o| o.id == x));
        }
        if out.iter().any(|o| o.id == d.id) {
            return Err(at(i, Invalid::at("id", format!("{} is used twice", d.id))));
        }
        if let Some(m) = d.macs.iter().find(|m| out.iter().any(|o| o.macs.contains(m))) {
            return Err(at(i, Invalid::at("macs", format!("{m} belongs to another device"))));
        }
        if d.pc_device.is_some() && out.iter().any(|o| o.pc_device == d.pc_device) {
            return Err(at(i, Invalid::at("pc_device", "this PC is linked to another device")));
        }
        out.push(d);
    }
    Ok(WolConfig { devices: out })
}

// ---------------------------------------------------------------------------------------------
// the configuration domain
// ---------------------------------------------------------------------------------------------

/// `wol`: the devices (no file); an apply hands them to the service.
pub struct WolDomain {
    pub tx: watch::Sender<WolConfig>,
}

impl WolDomain {
    pub fn new() -> (Self, watch::Receiver<WolConfig>) {
        let (tx, rx) = watch::channel(WolConfig::default());
        (Self { tx }, rx)
    }
}

fn config_of(v: &Value) -> Result<WolConfig, Invalid> {
    serde_json::from_value(v.clone()).map_err(|e| Invalid::from(format!("invalid wol document: {e}")))
}

impl Applier for WolDomain {
    fn key(&self) -> &str {
        WOL_KEY
    }

    fn order(&self) -> i32 {
        // after users
        30
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let c = normalize_config(config_of(&value)?)?;
        serde_json::to_value(c).map_err(|e| e.to_string().into())
    }

    fn render(&self, _value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        Ok(Vec::new())
    }

    fn reload(&self, value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        let c = match value {
            Some(v) => config_of(v).map_err(|e| e.message)?,
            None => WolConfig::default(),
        };
        (ctx.log)("Wake-on-LAN: devices handed to the service");
        self.tx.send_replace(c);
        Ok(())
    }
}

/// The devices applied at start (the domain only hands over later applies).
pub fn initial_settings(cfg: &rynas_config::Config) -> WolConfig {
    cfg.applied(WOL_KEY).ok().flatten().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// the network, injectable
// ---------------------------------------------------------------------------------------------

/// What the service asks of the network (a stand-in in tests: no real packet, ping or
/// neighbor table).
pub trait Net: Send + Sync {
    fn ifaces(&self) -> Vec<NasIface>;
    fn neighbors(&self) -> Vec<Neighbor>;
    fn ping(&self, ip: Ipv4Addr) -> bool;
    /// Any of the TCP ports answers.
    fn port(&self, ip: Ipv4Addr, ports: &[u16]) -> bool;
    /// One magic packet payload on one interface (raw frame and UDP broadcast to the subnet
    /// of the device's address `ip`, else the interface's first).
    fn send(&self, iface: &NasIface, payload: &[u8], port: u16, ip: Option<Ipv4Addr>) -> Result<(), String>;
    fn resolve(&self, host: &str) -> Option<Ipv4Addr>;
    fn reverse(&self, ip: Ipv4Addr) -> Option<String>;
}

/// The machine's network. `dry` (`--dry-system`): reads and checks, but only logs packets.
pub struct SystemNet {
    pub dry: bool,
}

fn ip_json(args: &[&str]) -> String {
    match std::process::Command::new("ip").args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(_) | Err(_) => {
            tracing::debug!("wol: ip {} failed", args.join(" "));
            String::new()
        }
    }
}

impl Net for SystemNet {
    fn ifaces(&self) -> Vec<NasIface> {
        send::parse_ip_addr(&ip_json(&["-j", "-d", "addr", "show"]))
    }

    fn neighbors(&self) -> Vec<Neighbor> {
        probe::parse_neigh(&ip_json(&["-4", "-j", "neigh", "show"]))
    }

    fn ping(&self, ip: Ipv4Addr) -> bool {
        probe::ping(ip)
    }

    fn port(&self, ip: Ipv4Addr, ports: &[u16]) -> bool {
        probe::any_port(ip, ports)
    }

    fn send(&self, iface: &NasIface, payload: &[u8], port: u16, ip: Option<Ipv4Addr>) -> Result<(), String> {
        if self.dry {
            tracing::info!("wol: (dry system) magic packet of {} bytes on {} port {port}", payload.len(), iface.name);
            return Ok(());
        }
        send::send_on(iface, payload, port, ip)
    }

    fn resolve(&self, host: &str) -> Option<Ipv4Addr> {
        probe::resolve(host)
    }

    fn reverse(&self, ip: Ipv4Addr) -> Option<String> {
        probe::reverse(ip)
    }
}

// ---------------------------------------------------------------------------------------------
// runtime state
// ---------------------------------------------------------------------------------------------

/// A device's runtime state (`<state>/wol/state.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DevState {
    #[serde(default)]
    pub status: WolStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<WolProbe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    /// When `ip` was last confirmed (the device answered there, or its agent or the
    /// neighbor table said so); forgotten after [`IP_TTL_MS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_seen_ms: Option<u64>,
    /// Newest first, at most [`HISTORY`].
    #[serde(default)]
    pub history: Vec<WolWake>,
    /// When the device was last woken for a backup (wake for backups: once a day).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_woken_ms: Option<u64>,
    /// A failed wake's notification is open.
    #[serde(default)]
    pub alert: bool,
    /// When the device itself last answered a port or ping check (written at most every
    /// 10 minutes): a device that usually answers and stops is off, whatever its agent's last
    /// contact says ([`probe::decide`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunState {
    #[serde(default)]
    pub devices: BTreeMap<String, DevState>,
}

impl DevState {
    fn set_status(&mut self, status: WolStatus, probe: Option<WolProbe>, now: u64) -> bool {
        let changed = self.status != status || self.probe != probe;
        if self.status != status || self.since_ms.is_none() {
            self.since_ms = Some(now);
        }
        self.status = status;
        self.probe = probe;
        changed
    }

    fn record(&mut self, w: WolWake) {
        self.history.insert(0, w);
        self.history.truncate(HISTORY);
    }
}

// ---------------------------------------------------------------------------------------------
// pure parts of the views and decisions
// ---------------------------------------------------------------------------------------------

/// `user` may see (wake, put to sleep) the device.
pub fn visible(d: &WolDevice, user: &str, admin: bool) -> bool {
    admin || d.users.iter().any(|u| u == user)
}

/// Why the device may not wake.
pub fn warnings(d: &WolDevice, facts: Option<&AgentFacts>, ip: Option<Ipv4Addr>, ifaces: &[NasIface]) -> Vec<WolWarning> {
    let mut w = Vec::new();
    let first = d.macs.first();
    if let Some(nic) = first.and_then(|m| facts.and_then(|f| f.nics.iter().find(|n| &n.mac == m))) {
        if nic.wireless {
            w.push(WolWarning::Wireless);
        } else if nic.wol_supported == Some(false) {
            w.push(WolWarning::Unsupported);
        } else if nic.wol_enabled == Some(false) {
            w.push(WolWarning::Disabled);
        }
    }
    if d.iface.is_none() && ip.is_some_and(|ip| !ifaces.iter().any(|i| i.up && send::eligible(i) && i.holds(ip))) {
        w.push(WolWarning::NoRoute);
    }
    if first.is_some_and(|m| mac_is_local(m)) {
        w.push(WolWarning::RandomMac);
    }
    w
}

/// Wake for backups: the due time to wake the PC for now, or `None`. `due`: the backup due now
/// inside its window ([`crate::pc_agent::wake_due`]); never while the PC is on or a backup
/// runs, at most once a calendar day (zone `tz`) and [`BACKUP_WAKE_GAP_MS`] after the last
/// wake for a backup (a failed wake is tried again the next day, never in a storm).
pub fn backup_wake(due: Option<u64>, online: bool, busy: bool, last_woken: Option<u64>, now: u64, tz: &jiff::tz::TimeZone) -> Option<u64> {
    let due = due?;
    let day = |ms: u64| {
        i64::try_from(ms).ok().and_then(|m| jiff::Timestamp::from_millisecond(m).ok()).map(|t| t.to_zoned(tz.clone()).date())
    };
    let allowed = last_woken.is_none_or(|l| now.saturating_sub(l) >= BACKUP_WAKE_GAP_MS && day(l) != day(now));
    (!online && !busy && allowed).then_some(due)
}

/// A device as a user who is not an administrator sees it: no SecureOn password, user list
/// or notes; in the history their own name, other users' names empty (the interface says
/// "another user").
pub fn redact(mut v: WolDeviceView, who: &str) -> WolDeviceView {
    v.device.secureon = None;
    v.device.users.clear();
    v.device.notes.clear();
    for w in &mut v.history {
        if let WolTrigger::User { name } = &mut w.by
            && name != who
        {
            name.clear();
        }
    }
    v
}

/// The NAS's own MACs (never a target).
pub fn own_macs(ifaces: &[NasIface]) -> HashSet<String> {
    ifaces.iter().flat_map(|i| i.mac.iter().chain(i.perm_mac.iter()).cloned()).collect()
}

/// The pending power command of an agent, by name.
fn pending_of(d: &Dev) -> Option<String> {
    let taken = d.command_at_ms.is_none_or(|c| d.last_seen_ms.is_some_and(|t| t >= c));
    if taken {
        return None;
    }
    match d.command.as_ref()? {
        AgentCommand::Sleep { mode, .. } => Some(
            match mode {
                SleepMode::Suspend => "suspend",
                SleepMode::Hibernate => "hibernate",
                SleepMode::Poweroff => "poweroff",
                SleepMode::Unknown => "unknown",
            }
            .to_string(),
        ),
        AgentCommand::EnableWol { .. } => Some("enable_wol".to_string()),
        _ => None,
    }
}

/// The task log line of a wake step (English, as the log keeps it; clients translate the step).
pub fn step_line(name: &str, s: &WolWakeStep) -> String {
    match s {
        WolWakeStep::Starting { name, macs } => format!("Waking {name} ({})", macs.join(", ")),
        WolWakeStep::Sent { ifaces } => format!("Sent on {} (3×)", ifaces.join(", ")),
        WolWakeStep::NotSent { failed } => failed.iter().map(|f| format!("Not sent on {}: {}", f.iface, f.error)).collect::<Vec<_>>().join("\n"),
        WolWakeStep::NoInterface { iface: Some(i) } => format!("no interface {i} on this NAS"),
        WolWakeStep::NoInterface { iface: None } => "no interface to send on".to_string(),
        WolWakeStep::SentAgain { ifaces, secs } => format!("Sent again on {} after {secs} s", ifaces.join(", ")),
        WolWakeStep::AlreadyOn => format!("{name} is already on"),
        WolWakeStep::Unconfirmed => "Nothing can confirm the wake: the device has no known address and no agent".to_string(),
        WolWakeStep::WaitAgent { secs } => format!("Network up after {secs} s, waiting for the agent"),
        WolWakeStep::AgentTimeout { secs } => format!("The network answered after {secs} s but the agent did not get in touch"),
        WolWakeStep::Online { secs } => format!("Online after {secs} s"),
        WolWakeStep::Timeout { secs } => format!("No answer within {secs} s"),
        // never made here: only a newer rynas has steps this one does not know
        WolWakeStep::Unknown => String::new(),
    }
}

/// A running wake.
struct Waking {
    upid: String,
    since_ms: u64,
    /// Its latest step.
    step: Option<WolWakeStep>,
}

fn now_ms() -> u64 {
    crate::pc_agent::now_ms()
}

// ---------------------------------------------------------------------------------------------
// the service
// ---------------------------------------------------------------------------------------------

pub struct Wol {
    pub ops: Arc<Ops>,
    notifier: Arc<dyn Notifier>,
    state_path: PathBuf,
    cfg: watch::Receiver<WolConfig>,
    pc: Option<Arc<PcAgent>>,
    net: Arc<dyn Net>,
    /// A system user of this name exists (the `users` of a device).
    user_exists: fn(&str) -> bool,
    state: Mutex<RunState>,
    /// Running wakes by device id.
    waking: Mutex<HashMap<String, Waking>>,
    /// Host names resolved lately: name → (when, address).
    dns: Mutex<HashMap<String, (Instant, Option<Ipv4Addr>)>>,
    /// The last time someone read the overview (ms).
    watched_ms: AtomicU64,
    kick: tokio::sync::Notify,
    ifaces: Mutex<Option<(Instant, Arc<Vec<NasIface>>)>>,
}

pub type WolState = Option<Arc<Wol>>;

/// A system user exists (NSS).
pub fn system_user_exists(u: &str) -> bool {
    crate::groups::user_groups(u).is_some()
}

impl Wol {
    pub fn new(
        ops: Arc<Ops>,
        notifier: Arc<dyn Notifier>,
        state_dir: PathBuf,
        cfg: watch::Receiver<WolConfig>,
        pc: Option<Arc<PcAgent>>,
        net: Arc<dyn Net>,
        user_exists: fn(&str) -> bool,
    ) -> Arc<Self> {
        let state_path = state_dir.join("wol").join("state.json");
        let state = std::fs::read(&state_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Arc::new(Self {
            ops,
            notifier,
            state_path,
            cfg,
            pc,
            net,
            user_exists,
            state: Mutex::new(state),
            waking: Mutex::new(HashMap::new()),
            dns: Mutex::new(HashMap::new()),
            watched_ms: AtomicU64::new(0),
            kick: tokio::sync::Notify::new(),
            ifaces: Mutex::new(None),
        })
    }

    pub fn devices(&self) -> Vec<WolDevice> {
        self.cfg.borrow().devices.clone()
    }

    fn device(&self, id: &str) -> Res<WolDevice> {
        self.cfg
            .borrow()
            .devices
            .iter()
            .find(|d| d.id == id)
            .cloned()
            .ok_or_else(|| Error::not_found(format!("no device {id}")))
    }

    /// A device the caller may see, or 404 (the same answer as an unknown one).
    fn visible_device(&self, id: &str, who: &str, admin: bool) -> Res<WolDevice> {
        self.device(id).ok().filter(|d| visible(d, who, admin)).ok_or_else(|| Error::not_found(format!("no device {id}")))
    }

    /// The NAS interfaces (cached for [`IFACES_TTL`]; blocking).
    pub fn ifaces(&self) -> Arc<Vec<NasIface>> {
        let mut c = self.ifaces.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, l)) = c.as_ref()
            && at.elapsed() < IFACES_TTL
        {
            return l.clone();
        }
        let l = Arc::new(self.net.ifaces());
        *c = Some((Instant::now(), l.clone()));
        l
    }

    /// The linked PC agent's record (not revoked).
    fn agent_of(&self, d: &WolDevice) -> Option<Dev> {
        let pc = self.pc.as_ref()?;
        pc.device(d.pc_device.as_deref()?).ok().filter(|x| !x.revoked)
    }

    fn dev_state(&self, id: &str) -> DevState {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).devices.get(id).cloned().unwrap_or_default()
    }

    fn edit_state(&self, f: impl FnOnce(&mut RunState) -> bool) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut changed = f(&mut st);
        // devices no longer configured go, and their open notification with them
        let ids: HashSet<String> = self.cfg.borrow().devices.iter().map(|d| d.id.clone()).collect();
        let gone: Vec<(String, bool)> = st.devices.iter().filter(|(k, _)| !ids.contains(*k)).map(|(k, v)| (k.clone(), v.alert)).collect();
        for (id, alert) in &gone {
            st.devices.remove(id);
            if *alert {
                self.resolve_alert(id, id);
            }
        }
        changed |= !gone.is_empty();
        if !changed {
            return;
        }
        let Ok(bytes) = serde_json::to_vec_pretty(&*st) else { return };
        if let Some(dir) = self.state_path.parent() {
            let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
        }
        if let Err(e) = crate::tls::write_atomic(&self.state_path, &bytes, 0o600, None) {
            tracing::warn!("wol: writing the state: {e}");
        }
    }

    /// Closes a failed wake's notification (`name`: the device's name for the text).
    fn resolve_alert(&self, id: &str, name: &str) {
        let ev = resolved(NotifyCategory::Backup, "wol", format!("wol:wake-failed:{id}"), format!("{name} wakes up again"));
        self.notifier.emit(field(field(code(ev, "wol.wake_resumed"), "device", name), "id", id));
    }

    /// A host name's address, cached for [`DNS_TTL`] (blocking).
    fn resolve(&self, host: &str) -> Option<Ipv4Addr> {
        if let Ok(ip) = host.parse() {
            return Some(ip);
        }
        if let Some((at, ip)) = self.dns.lock().unwrap_or_else(|p| p.into_inner()).get(host)
            && at.elapsed() < DNS_TTL
        {
            return *ip;
        }
        let ip = self.net.resolve(host);
        let mut c = self.dns.lock().unwrap_or_else(|p| p.into_inner());
        c.retain(|_, (at, _)| at.elapsed() < DNS_TTL);
        c.insert(host.to_string(), (Instant::now(), ip));
        ip
    }

    /// Milliseconds since the linked agent's last request.
    fn agent_age(&self, d: &WolDevice, agent: &Dev, now: u64) -> Option<u64> {
        let pc = self.pc.as_ref()?;
        pc.last_contact(d.pc_device.as_deref()?, agent).map(|t| now.saturating_sub(t))
    }

    /// The network answers at `ip` and it is the device ([`probe::answer_is_device`]: the
    /// neighbor entry the answer leaves holds one of its MACs). Blocking.
    fn net_answer(&self, d: &WolDevice, ip: Ipv4Addr) -> (bool, bool) {
        let port = self.net.port(ip, &d.ports);
        let ping = !port && self.net.ping(ip);
        if !port && !ping {
            return (false, false);
        }
        let on_subnet = self.ifaces().iter().any(|i| i.up && i.holds(ip));
        if probe::answer_is_device(ip, on_subnet, &d.macs, &self.net.neighbors()) {
            (port, ping)
        } else {
            (false, false)
        }
    }

    fn ip_of(&self, d: &WolDevice, agent: Option<&Dev>, neigh: &[Neighbor]) -> (Option<Ipv4Addr>, bool) {
        if let Some(ip) = probe::ip_from_neighbors(neigh, &d.macs) {
            return (Some(ip), true);
        }
        if let Some(ip) = agent.and_then(|a| a.facts.as_ref()).and_then(|f| probe::ip_from_facts(f, &d.macs)) {
            return (Some(ip), true);
        }
        if let Some(ip) = d.host.as_deref().and_then(|h| self.resolve(h)) {
            return (Some(ip), false);
        }
        let st = self.dev_state(&d.id);
        let fresh = st.ip_seen_ms.is_some_and(|t| now_ms().saturating_sub(t) < IP_TTL_MS);
        (st.ip.filter(|_| fresh).and_then(|s| s.parse().ok()), false)
    }

    fn check(&self, d: &WolDevice, neigh: &[Neighbor], now: u64) -> (Seen, Option<Ipv4Addr>, bool) {
        let agent = self.agent_of(d);
        let (ip, fresh) = self.ip_of(d, agent.as_ref(), neigh);
        let mut s = Seen {
            agent: agent.is_some(),
            agent_age_ms: agent.as_ref().and_then(|a| self.agent_age(d, a, now)),
            ip_known: ip.is_some(),
            neighbor: neigh.iter().any(|n| n.reachable && d.macs.contains(&n.mac)),
            answers_usually: self.dev_state(&d.id).answer_ms.is_some_and(|t| now.saturating_sub(t) < probe::ANSWERS_USUALLY_MS),
            ..Default::default()
        };
        if let Some(ip) = ip {
            (s.port, s.ping) = self.net_answer(d, ip);
        }
        (s, ip, fresh)
    }

    pub fn round(&self, devices: &[WolDevice]) {
        let neigh = self.net.neighbors();
        let now = now_ms();
        let mut results = Vec::with_capacity(devices.len());
        for chunk in devices.chunks(16) {
            std::thread::scope(|s| {
                let hs: Vec<_> = chunk.iter().map(|d| s.spawn(|| (d.id.clone(), self.check(d, &neigh, now)))).collect();
                results.extend(hs.into_iter().filter_map(|h| h.join().ok()));
            });
        }
        let waking: HashSet<String> = self.waking.lock().unwrap_or_else(|p| p.into_inner()).keys().cloned().collect();
        self.edit_state(|st| {
            let mut changed = false;
            for (id, (seen, ip, fresh)) in results {
                if waking.contains(&id) {
                    continue;
                }
                let (status, probe) = probe::decide(&seen);
                let e = st.devices.entry(id).or_default();
                changed |= e.set_status(status, probe, now);
                if (seen.port || seen.ping) && e.answer_ms.is_none_or(|t| now.saturating_sub(t) > 600_000) {
                    e.answer_ms = Some(now);
                    changed = true;
                }
                // an address is remembered when it is confirmed: the neighbor table or the
                // agent said so, or the device answered there (written again hourly)
                let confirmed = fresh || seen.port || seen.ping;
                if let Some(ip) = ip.filter(|_| confirmed).map(|i| i.to_string()) {
                    let stale = e.ip_seen_ms.is_none_or(|t| now.saturating_sub(t) > 3_600_000);
                    if e.ip.as_deref() != Some(ip.as_str()) || stale {
                        e.ip = Some(ip);
                        e.ip_seen_ms = Some(now);
                        changed = true;
                    }
                }
            }
            changed
        });
    }

    /// The background checks: every [`SLOW`], every [`FAST`] while watched.
    pub async fn probe_loop(self: Arc<Self>) {
        let mut last: HashMap<String, Instant> = HashMap::new();
        loop {
            let _ = tokio::time::timeout(Duration::from_secs(1), self.kick.notified()).await;
            let watched = now_ms().saturating_sub(self.watched_ms.load(Ordering::Relaxed)) < WATCH_MS;
            let every = if watched { FAST } else { SLOW };
            let devices = self.devices();
            let waking: HashSet<String> = self.waking.lock().unwrap_or_else(|p| p.into_inner()).keys().cloned().collect();
            last.retain(|k, _| devices.iter().any(|d| &d.id == k));
            let due: Vec<WolDevice> = devices
                .into_iter()
                .filter(|d| !waking.contains(&d.id) && last.get(&d.id).is_none_or(|t| t.elapsed() >= every))
                .collect();
            if due.is_empty() {
                continue;
            }
            for d in &due {
                last.insert(d.id.clone(), Instant::now());
            }
            let w = self.clone();
            let _ = tokio::task::spawn_blocking(move || w.round(&due)).await;
        }
    }

    // ------------------------------------------------------------------------ views

    fn agent_view(&self, d: &Dev, name: &str) -> WolAgent {
        let facts = d.facts.clone().unwrap_or_default();
        let power = crate::pc_agent::takes_power_commands(d);
        WolAgent {
            agent_version: d.agent_version.clone(),
            nics: facts.nics,
            sleep: if power { facts.sleep.into_iter().filter(|m| *m != SleepMode::Unknown).collect() } else { Vec::new() },
            power_commands: power,
            busy: self.pc.as_ref().is_some_and(|p| p.running(name)),
            pending: pending_of(d),
        }
    }

    pub fn overview(&self, who: &str, admin: bool) -> WolOverview {
        let ifaces = self.ifaces();
        let waking: HashMap<String, (String, u64, Option<WolWakeStep>)> =
            self.waking.lock().unwrap_or_else(|p| p.into_inner()).iter().map(|(id, w)| (id.clone(), (w.upid.clone(), w.since_ms, w.step.clone()))).collect();
        let devices = self
            .devices()
            .into_iter()
            .filter(|d| visible(d, who, admin))
            .map(|d| {
                let st = self.dev_state(&d.id);
                let agent = self.agent_of(&d);
                let fresh = st.ip_seen_ms.is_some_and(|t| now_ms().saturating_sub(t) < IP_TTL_MS);
                let ip: Option<Ipv4Addr> = st
                    .ip
                    .as_deref()
                    .filter(|_| fresh)
                    .and_then(|s| s.parse().ok())
                    .or_else(|| agent.as_ref().and_then(|a| a.facts.as_ref()).and_then(|f| probe::ip_from_facts(f, &d.macs)))
                    .or_else(|| d.host.as_deref().and_then(|h| h.parse().ok()));
                let wake = waking.get(&d.id).cloned();
                let v = WolDeviceView {
                    status: if wake.is_some() { WolStatus::Waking } else { st.status },
                    probe: st.probe,
                    since_ms: st.since_ms,
                    ip: ip.map(|i| i.to_string()),
                    send_iface: send::send_iface(&ifaces, d.iface.as_deref(), ip),
                    waking_since_ms: wake.as_ref().map(|w| w.1),
                    wake_step: wake.as_ref().and_then(|w| w.2.clone()),
                    waking: wake.map(|w| w.0),
                    history: st.history,
                    warnings: warnings(&d, agent.as_ref().and_then(|a| a.facts.as_ref()), ip, &ifaces),
                    agent: agent.as_ref().map(|a| self.agent_view(a, d.pc_device.as_deref().unwrap_or(""))),
                    can_manage: admin,
                    device: d,
                };
                if admin { v } else { redact(v, who) }
            })
            .collect();
        let ifaces = if admin {
            ifaces
                .iter()
                .filter(|i| send::eligible(i))
                .map(|i| WolIface { name: i.name.clone(), ipv4: i.cidrs(), up: i.up })
                .collect()
        } else {
            Vec::new()
        };
        WolOverview { devices, ifaces }
    }

    /// Cards that could become devices (blocking).
    pub fn discover(&self) -> Vec<WolCandidate> {
        let ifaces = self.ifaces();
        let own = own_macs(&ifaces);
        let devices = self.devices();
        let known = |mac: &str| devices.iter().find(|d| d.macs.iter().any(|m| m == mac)).map(|d| d.id.clone());
        let mut out: Vec<WolCandidate> = Vec::new();
        if let Some(pc) = &self.pc {
            for (name, d) in pc.snapshot().devices {
                let Some(f) = d.facts.as_ref().filter(|_| !d.revoked) else { continue };
                for nic in &f.nics {
                    if own.contains(&nic.mac) || out.iter().any(|c| c.mac == nic.mac) {
                        continue;
                    }
                    out.push(WolCandidate {
                        mac: nic.mac.clone(),
                        ip: nic.ipv4.first().and_then(|c| c.split('/').next()).map(str::to_string),
                        name: Some(if d.hostname.is_empty() { name.clone() } else { d.hostname.clone() }),
                        iface: None,
                        source: WolSource::Agent,
                        pc_device: Some(name.clone()),
                        nic: Some(nic.clone()),
                        known: known(&nic.mac),
                    });
                }
            }
        }
        let mut seen: Vec<Neighbor> = Vec::new();
        for n in self.net.neighbors() {
            if !own.contains(&n.mac) && !out.iter().any(|c| c.mac == n.mac) && !seen.iter().any(|s| s.mac == n.mac) {
                seen.push(n);
            }
        }
        seen.truncate(128);
        let names: Vec<Option<String>> = std::thread::scope(|s| {
            let hs: Vec<_> = seen.iter().map(|n| s.spawn(|| self.net.reverse(n.ip))).collect();
            hs.into_iter().map(|h| h.join().ok().flatten()).collect()
        });
        for (n, name) in seen.into_iter().zip(names) {
            out.push(WolCandidate {
                known: known(&n.mac),
                mac: n.mac,
                ip: Some(n.ip.to_string()),
                name,
                iface: Some(n.dev),
                source: WolSource::Network,
                pc_device: None,
                nic: None,
            });
        }
        out
    }

    // ------------------------------------------------------------------------ changes

    /// Checks a device against the system (users, PC, NAS MACs) and the other devices, and
    /// writes the document with it (`replace`: the id of the device it replaces). Blocking.
    pub fn save_device(&self, who: &str, d: WolDevice, replace: Option<&str>) -> Res<WolDevice> {
        let mut d = normalize_device(d).map_err(invalid)?;
        if let Some(u) = d.users.iter().find(|u| !(self.user_exists)(u)) {
            return Err(Error::invalid("users", format!("no user {u}")));
        }
        if let Some(p) = d.pc_device.as_deref()
            && self.pc.as_ref().is_none_or(|pc| pc.device(p).is_err())
        {
            return Err(Error::invalid("pc_device", format!("no paired PC {p}")));
        }
        let own = own_macs(&self.ifaces());
        if let Some(m) = d.macs.iter().find(|m| own.contains(*m)) {
            return Err(Error::invalid("macs", format!("{m} is this NAS")));
        }
        let mut refusal: Option<Error> = None;
        let mut stored: Option<WolDevice> = None;
        let what = if replace.is_some() { "change" } else { "add" };
        let comment = format!("Wake-on-LAN: {what} {}", d.name);
        let r = self.ops.config.apply_update(who, WOL_KEY, &comment, &|l| tracing::info!("wol: {l}"), |cur| {
            let mut doc: WolConfig = cur.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
            let check = (|| {
                match replace {
                    Some(id) => {
                        let i = doc.devices.iter().position(|x| x.id == id).ok_or_else(|| Error::not_found(format!("no device {id}")))?;
                        d.id = id.to_string();
                        doc.devices.remove(i);
                    }
                    None if d.id.is_empty() => {
                        d.id = unique_id(&slug(&d.name), &|x| doc.devices.iter().any(|o| o.id == x));
                    }
                    None => {
                        if doc.devices.iter().any(|o| o.id == d.id) {
                            return Err(Error::new(StatusCode::CONFLICT, error_codes::ALREADY_EXISTS, format!("device {} already exists", d.id)));
                        }
                    }
                }
                if let Some(m) = d.macs.iter().find(|m| doc.devices.iter().any(|o| o.macs.contains(m))) {
                    return Err(Error::invalid("macs", format!("{m} belongs to another device")));
                }
                if d.pc_device.is_some() && doc.devices.iter().any(|o| o.pc_device == d.pc_device) {
                    return Err(Error::invalid("pc_device", "this PC is linked to another device"));
                }
                if doc.devices.len() >= MAX_DEVICES {
                    return Err(Error::invalid("devices", format!("at most {MAX_DEVICES} devices")));
                }
                Ok(())
            })();
            if let Err(e) = check {
                refusal = Some(e);
                return Err("refused".into());
            }
            doc.devices.push(d.clone());
            stored = Some(d.clone());
            serde_json::to_value(doc).map(Some).map_err(|e| e.to_string())
        });
        if let Some(e) = refusal {
            return Err(e);
        }
        r?;
        stored.ok_or_else(|| Error::internal("the device was not stored"))
    }

    /// Removes a device (blocking).
    pub fn delete_device(&self, who: &str, id: &str) -> Res<()> {
        let mut missing = false;
        let r = self.ops.config.apply_update(who, WOL_KEY, &format!("Wake-on-LAN: remove {id}"), &|l| tracing::info!("wol: {l}"), |cur| {
            let mut doc: WolConfig = cur.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
            let before = doc.devices.len();
            doc.devices.retain(|d| d.id != id);
            if doc.devices.len() == before {
                missing = true;
                return Err("refused".into());
            }
            serde_json::to_value(doc).map(Some).map_err(|e| e.to_string())
        });
        if missing {
            return Err(Error::not_found(format!("no device {id}")));
        }
        r?;
        Ok(())
    }

    // ------------------------------------------------------------------------ wake

    /// Starts a wake of device `id` (or joins the running one).
    pub fn start_wake(self: &Arc<Self>, id: &str, by: WolTrigger, task_user: &str) -> Res<WolWakeStarted> {
        let dev = self.device(id)?;
        let mut waking = self.waking.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(w) = waking.get(id) {
            return Ok(WolWakeStarted { upid: w.upid.clone(), joined: true });
        }
        let w = self.clone();
        let upid = self.ops.tasks.spawn("wol.wake", task_user, move |ctx| async move { w.run_wake(ctx, dev, by).await })?;
        waking.insert(id.to_string(), Waking { upid: upid.clone(), since_ms: now_ms(), step: None });
        Ok(WolWakeStarted { upid, joined: false })
    }

    async fn blocking<T: Send + 'static>(self: &Arc<Self>, f: impl FnOnce(&Wol) -> T + Send + 'static) -> Result<T, String> {
        let w = self.clone();
        tokio::task::spawn_blocking(move || f(&w)).await.map_err(|e| e.to_string())
    }

    /// One round of sends: three times a second apart on every chosen interface. The
    /// interfaces where it went out at least once.
    async fn send_round(self: &Arc<Self>, ctx: &rynas_tasks::TaskCtx, dev: &WolDevice, chosen: &[NasIface], ip: Option<Ipv4Addr>) -> Vec<String> {
        let secure = dev.secureon.as_deref().and_then(send::secureon_bytes);
        let payloads: Vec<Vec<u8>> = dev.macs.iter().filter_map(|m| send::mac_bytes(m)).map(|m| send::magic_payload(m, secure.as_deref())).collect();
        let mut ok: Vec<String> = Vec::new();
        let mut errors: BTreeMap<String, String> = BTreeMap::new();
        for n in 0..3 {
            if n > 0 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let (ifs, pl, port) = (chosen.to_vec(), payloads.clone(), dev.port);
            let r = self
                .blocking(move |w| {
                    ifs.iter()
                        .map(|i| {
                            let r = pl.iter().map(|p| w.net.send(i, p, port, ip)).find(|r| r.is_err()).unwrap_or(Ok(()));
                            (i.name.clone(), r)
                        })
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default();
            for (name, r) in r {
                match r {
                    Ok(()) if !ok.contains(&name) => ok.push(name),
                    Ok(()) => {}
                    Err(e) => {
                        errors.insert(name, e);
                    }
                }
            }
        }
        let failed: Vec<WolSendError> =
            errors.into_iter().filter(|(n, _)| !ok.contains(n)).map(|(iface, error)| WolSendError { iface, error }).collect();
        if !failed.is_empty() {
            self.step(ctx, dev, WolWakeStep::NotSent { failed });
        }
        ok
    }

    /// Records `s` as the latest step of the running wake of `dev`.
    fn set_step(&self, dev: &WolDevice, s: WolWakeStep) {
        if let Some(w) = self.waking.lock().unwrap_or_else(|p| p.into_inner()).get_mut(&dev.id) {
            w.step = Some(s);
        }
    }

    /// Logs `s` in the wake's task (English) and records it as its latest step.
    fn step(&self, ctx: &rynas_tasks::TaskCtx, dev: &WolDevice, s: WolWakeStep) {
        for line in step_line(&dev.name, &s).lines() {
            ctx.log(line);
        }
        self.set_step(dev, s);
    }

    async fn run_wake(self: Arc<Self>, ctx: rynas_tasks::TaskCtx, dev: WolDevice, by: WolTrigger) -> Result<(), String> {
        let started = now_ms();
        let t0 = Instant::now();
        let id = dev.id.clone();
        let upid = ctx.id().to_string();
        let mut ifaces_used: Vec<String> = Vec::new();
        let r = self.wake_steps(&ctx, &dev, started, t0, &mut ifaces_used).await;
        // a cancelled wake ends quietly: recorded as not answered, never notified
        let cancelled = ctx.is_cancelled();
        let (end, secs) = match &r {
            Ok(x) => *x,
            Err(_) if cancelled => (WolWakeEnd::Timeout, None),
            Err(_) => (WolWakeEnd::SendFailed, None),
        };
        let step = self.waking.lock().unwrap_or_else(|p| p.into_inner()).get(&id).and_then(|w| w.step.clone());
        let wake = WolWake { at_ms: started, by, end, secs, ifaces: ifaces_used, upid: Some(upid), step };
        let (w, d2) = (self.clone(), dev.clone());
        let _ = tokio::task::spawn_blocking(move || w.finish(&d2, wake, cancelled)).await;
        self.waking.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
        match end {
            _ if cancelled => Err("cancelled".into()),
            WolWakeEnd::Online | WolWakeEnd::AlreadyOn | WolWakeEnd::Unconfirmed => Ok(()),
            WolWakeEnd::Timeout => Err(format!("{} did not answer within {} s", dev.name, dev.wake_timeout_s)),
            WolWakeEnd::SendFailed => Err(r.err().unwrap_or_else(|| "the packet could not be sent".into())),
        }
    }

    async fn wake_steps(
        self: &Arc<Self>,
        ctx: &rynas_tasks::TaskCtx,
        dev: &WolDevice,
        started: u64,
        t0: Instant,
        used: &mut Vec<String>,
    ) -> Result<(WolWakeEnd, Option<u32>), String> {
        self.step(ctx, dev, WolWakeStep::Starting { name: dev.name.clone(), macs: dev.macs.clone() });
        // on already? (its agent's recent requests, or its own answer on the network); the
        // packets go out anyway: a PC that has just gone to sleep still looks on for a moment
        let d2 = dev.clone();
        let (ip, already, agent_linked, chosen) = self
            .blocking(move |w| {
                let now = now_ms();
                let agent = w.agent_of(&d2);
                let neigh = w.net.neighbors();
                let (ip, _) = w.ip_of(&d2, agent.as_ref(), &neigh);
                let net_up = ip.is_some_and(|ip| {
                    let (port, ping) = w.net_answer(&d2, ip);
                    port || ping
                });
                // the same rule as the status ([`probe::decide`]): an agent contact before a
                // device that usually answers went quiet does not make it "already on"
                let usually = w.dev_state(&d2.id).answer_ms.is_some_and(|t| now.saturating_sub(t) < probe::ANSWERS_USUALLY_MS);
                let fresh = agent.as_ref().and_then(|a| w.agent_age(&d2, a, now)).is_some_and(|age| {
                    age <= probe::AGENT_FRESH_MS && !(usually && !net_up && age > probe::AGENT_SILENT_MS)
                });
                let ifaces = w.ifaces();
                let chosen: Vec<NasIface> = send::choose(&ifaces, d2.iface.as_deref(), ip).into_iter().cloned().collect();
                (ip, net_up || fresh, agent.is_some(), chosen)
            })
            .await?;
        if chosen.is_empty() {
            // the task's error says it in the log
            let s = WolWakeStep::NoInterface { iface: dev.iface.clone() };
            let e = step_line(&dev.name, &s);
            self.set_step(dev, s);
            return Err(e);
        }
        let sent = self.send_round(ctx, dev, &chosen, ip).await;
        if sent.is_empty() {
            return Err("the packet could not be sent on any interface".into());
        }
        self.step(ctx, dev, WolWakeStep::Sent { ifaces: sent.clone() });
        *used = sent;
        if already {
            self.step(ctx, dev, WolWakeStep::AlreadyOn);
            return Ok((WolWakeEnd::AlreadyOn, Some(0)));
        }
        if !agent_linked && ip.is_none() {
            self.step(ctx, dev, WolWakeStep::Unconfirmed);
            return Ok((WolWakeEnd::Unconfirmed, None));
        }
        let timeout = Duration::from_secs(u64::from(dev.wake_timeout_s));
        let mut resends = RESEND_AT.iter().peekable();
        let mut net_up_at: Option<u64> = None;
        loop {
            let tick = Instant::now();
            let secs = t0.elapsed().as_secs();
            let d2 = dev.clone();
            let (contact, net_up) = self
                .blocking(move |w| {
                    let agent = w.agent_of(&d2);
                    let contact = agent.as_ref().and_then(|a| w.pc.as_ref()?.last_contact(d2.pc_device.as_deref()?, a)).is_some_and(|t| t > started);
                    let ip = ip.or_else(|| w.ip_of(&d2, agent.as_ref(), &w.net.neighbors()).0);
                    let net_up = ip.is_some_and(|ip| {
                        let (port, ping) = w.net_answer(&d2, ip);
                        port || ping
                    });
                    (contact, net_up)
                })
                .await?;
            if net_up && net_up_at.is_none() {
                net_up_at = Some(secs);
                if agent_linked && !contact {
                    self.step(ctx, dev, WolWakeStep::WaitAgent { secs: secs as u32 });
                }
            }
            if contact || (net_up && !agent_linked) {
                self.step(ctx, dev, WolWakeStep::Online { secs: secs as u32 });
                return Ok((WolWakeEnd::Online, Some(secs as u32)));
            }
            if t0.elapsed() >= timeout {
                if let Some(at) = net_up_at {
                    self.step(ctx, dev, WolWakeStep::AgentTimeout { secs: at as u32 });
                    return Ok((WolWakeEnd::Online, Some(at as u32)));
                }
                self.step(ctx, dev, WolWakeStep::Timeout { secs: dev.wake_timeout_s });
                return Ok((WolWakeEnd::Timeout, None));
            }
            if net_up_at.is_none() && resends.peek().is_some_and(|at| secs >= **at) {
                resends.next();
                let again = self.send_round(ctx, dev, &chosen, ip).await;
                if !again.is_empty() {
                    self.step(ctx, dev, WolWakeStep::SentAgain { ifaces: again, secs: secs as u32 });
                }
            }
            if ctx.is_cancelled() {
                return Err("cancelled".into());
            }
            tokio::time::sleep(Duration::from_secs(1).saturating_sub(tick.elapsed())).await;
        }
    }

    /// Records a finished wake: history, status, the notification of a wake nobody watched
    /// (never for a cancelled one). Blocking (writes the state).
    fn finish(&self, dev: &WolDevice, w: WolWake, cancelled: bool) {
        let now = now_ms();
        let good = matches!(w.end, WolWakeEnd::Online | WolWakeEnd::AlreadyOn);
        let failed = !cancelled && matches!(w.end, WolWakeEnd::Timeout | WolWakeEnd::SendFailed);
        let unwatched = w.by == WolTrigger::Backup;
        let mut alert_open = false;
        self.edit_state(|st| {
            let e = st.devices.entry(dev.id.clone()).or_default();
            alert_open = e.alert;
            match w.end {
                WolWakeEnd::Online | WolWakeEnd::AlreadyOn => {
                    let probe = e.probe.or(Some(WolProbe::Ping));
                    e.set_status(WolStatus::Online, probe, now);
                    e.alert = false;
                }
                WolWakeEnd::Timeout if !cancelled => {
                    e.set_status(WolStatus::Offline, None, now);
                }
                _ => {}
            }
            if failed && unwatched {
                e.alert = true;
            }
            e.record(w.clone());
            true
        });
        // removed during the wake: its state went above, nothing would ever close the alert
        if self.device(&dev.id).is_err() {
            return;
        }
        if failed && unwatched {
            let key = format!("wol:wake-failed:{}", dev.id);
            let mut ev = firing(NotifyCategory::Backup, NotifySeverity::Warning, "wol", key, format!("{} did not wake up", dev.name));
            ev.body = match w.end {
                WolWakeEnd::Timeout => format!("No answer within {} s of the wake for its backup.", dev.wake_timeout_s),
                _ => "The magic packet could not be sent.".to_string(),
            };
            self.notifier.emit(field(field(code(ev, "wol.wake_failed"), "device", &dev.name), "id", &dev.id));
        } else if good && alert_open {
            self.resolve_alert(&dev.id, &dev.name);
        }
    }

    // ------------------------------------------------------------------------ wake for backups

    /// One pass of wake for backups (every minute; blocking).
    pub fn backup_pass(self: &Arc<Self>) {
        let (now, tz) = (now_ms(), rynas_power::system_tz());
        let paused_all = self.pc.as_ref().is_some_and(|p| p.paused_all());
        for d in self.devices() {
            let st = self.dev_state(&d.id);
            let dev = self.agent_of(&d);
            let enabled = dev.as_ref().is_some_and(|x| x.settings.plan.wake_for_backup);
            if !enabled {
                // turned off (or the PC unlinked): its failed wake's notification closes
                if st.alert {
                    self.edit_state(|s| {
                        s.devices.entry(d.id.clone()).or_default().alert = false;
                        true
                    });
                    self.resolve_alert(&d.id, &d.name);
                }
                continue;
            }
            let (Some(pc), Some(dev), Some(name)) = (self.pc.clone(), dev, d.pc_device.as_deref()) else { continue };
            if self.waking.lock().unwrap_or_else(|p| p.into_inner()).contains_key(&d.id) {
                continue;
            }
            // the checked status (the device's own answer) or its agent's recent requests
            let online = st.status == WolStatus::Online || self.agent_age(&d, &dev, now).is_some_and(|a| a <= probe::AGENT_FRESH_MS);
            let due = crate::pc_agent::wake_due(&dev, paused_all, &tz, now);
            let Some(due) = backup_wake(due, online, pc.running(name), st.backup_woken_ms, now, &tz) else { continue };
            match self.start_wake(&d.id, WolTrigger::Backup, SYSTEM_USER) {
                Ok(s) => {
                    tracing::info!("wol: waking {} for its backup due at {due} ({})", d.name, s.upid);
                    self.edit_state(|s| {
                        s.devices.entry(d.id.clone()).or_default().backup_woken_ms = Some(now);
                        true
                    });
                    if let Err(e) = pc.set_woken(name, now) {
                        tracing::warn!("wol: marking {name} as woken: {}", e.2);
                    }
                }
                Err(e) => tracing::warn!("wol: waking {} for its backup: {}", d.name, e.2),
            }
        }
    }
}

/// Starts the background checks and wake for backups.
pub fn spawn(w: Arc<Wol>) {
    tokio::spawn(w.clone().probe_loop());
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let w2 = w.clone();
            let _ = tokio::task::spawn_blocking(move || w2.backup_pass()).await;
        }
    });
}

// ---------------------------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------------------------

fn invalid(e: Invalid) -> Error {
    match e.field {
        Some(f) => Error::invalid(f, e.message),
        None => Error::new(StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE, e.message),
    }
}

fn svc(st: &WolState) -> Res<Arc<Wol>> {
    st.clone().ok_or_else(|| {
        Error::new(StatusCode::SERVICE_UNAVAILABLE, error_codes::BACKEND_UNAVAILABLE, "Wake-on-LAN is not enabled in this rynasd".into())
    })
}

fn audit(who: &str, action: &str, target: &str) {
    tracing::info!(target: "rynas::audit", "wol {action} {target} by {who}");
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Res<T> {
    tokio::task::spawn_blocking(f).await.map_err(Error::internal)
}

fn conflict(msg: String) -> Error {
    Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, msg)
}

pub fn router(st: WolState) -> Router {
    Router::new()
        .route("/v1/wol", get(overview))
        .route("/v1/wol/discover", get(discover))
        .route("/v1/wol/devices", post(create))
        .route("/v1/wol/devices/{id}", put(update).delete(remove))
        .route("/v1/wol/devices/{id}/wake", post(wake))
        .route("/v1/wol/devices/{id}/sleep", post(sleep))
        .route("/v1/wol/devices/{id}/enable", post(enable))
        .with_state(st)
}

async fn overview(State(st): State<WolState>, h: HeaderMap) -> Res<Json<WolOverview>> {
    let (w, who) = (svc(&st)?, user(&h)?);
    let w2 = w.clone();
    let v = blocking(move || {
        let admin = w2.ops.admins.is_admin(&who);
        w2.overview(&who, admin)
    })
    .await?;
    // only a caller who sees a device makes the checks faster
    if !v.devices.is_empty() {
        let now = now_ms();
        let before = w.watched_ms.swap(now, Ordering::Relaxed);
        if now.saturating_sub(before) >= WATCH_MS {
            // the page has just opened: check now instead of within a minute
            w.kick.notify_one();
        }
    }
    Ok(Json(v))
}

async fn discover(State(st): State<WolState>, h: HeaderMap) -> Res<Json<Vec<WolCandidate>>> {
    let (w, who) = (svc(&st)?, user(&h)?);
    w.ops.admins.require_admin(&who, "Wake-on-LAN")?;
    Ok(Json(blocking(move || w.discover()).await?))
}

async fn create(State(st): State<WolState>, h: HeaderMap, body: Result<Json<WolDevice>, JsonRejection>) -> Res<Json<WolDevice>> {
    let (w, who) = (svc(&st)?, user(&h)?);
    w.ops.admins.require_admin(&who, "Wake-on-LAN")?;
    let Json(d) = body?;
    let who2 = who.clone();
    let d = blocking(move || w.save_device(&who2, d, None)).await??;
    audit(&who, "add", &d.id);
    Ok(Json(d))
}

async fn update(
    State(st): State<WolState>,
    h: HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: Result<Json<WolDevice>, JsonRejection>,
) -> Res<Json<WolDevice>> {
    let (w, who) = (svc(&st)?, user(&h)?);
    w.ops.admins.require_admin(&who, "Wake-on-LAN")?;
    let Json(d) = body?;
    let (who2, id2) = (who.clone(), id.clone());
    let d = blocking(move || w.save_device(&who2, d, Some(&id2))).await??;
    audit(&who, "change", &id);
    Ok(Json(d))
}

async fn remove(State(st): State<WolState>, h: HeaderMap, UrlPath(id): UrlPath<String>) -> Res<StatusCode> {
    let (w, who) = (svc(&st)?, user(&h)?);
    w.ops.admins.require_admin(&who, "Wake-on-LAN")?;
    let (who2, id2) = (who.clone(), id.clone());
    blocking(move || w.delete_device(&who2, &id2)).await??;
    audit(&who, "remove", &id);
    Ok(StatusCode::NO_CONTENT)
}

async fn wake(State(st): State<WolState>, h: HeaderMap, UrlPath(id): UrlPath<String>) -> Res<Json<WolWakeStarted>> {
    let (w, who) = (svc(&st)?, user(&h)?);
    let (w2, who2, id2) = (w.clone(), who.clone(), id.clone());
    blocking(move || {
        let admin = w2.ops.admins.is_admin(&who2);
        w2.visible_device(&id2, &who2, admin).map(|_| ())
    })
    .await??;
    let s = w.start_wake(&id, WolTrigger::User { name: who.clone() }, &who)?;
    audit(&who, "wake", &id);
    Ok(Json(s))
}

/// The paired agent of a device the caller may see.
fn linked_pc(w: &Wol, id: &str, who: &str, admin: bool) -> Res<(Arc<PcAgent>, String, Dev)> {
    let d = w.visible_device(id, who, admin)?;
    let name = d.pc_device.clone().ok_or_else(|| conflict(format!("{} has no paired PC agent", d.name)))?;
    let pc = w.pc.clone().ok_or_else(|| conflict("PC backup is not running on this NAS".into()))?;
    let dev = pc.device(&name).map_err(|_| conflict(format!("the PC {name} is not paired")))?;
    Ok((pc, name, dev))
}

async fn sleep(
    State(st): State<WolState>,
    h: HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: Result<Json<WolSleepRequest>, JsonRejection>,
) -> Res<StatusCode> {
    let (w, who) = (svc(&st)?, user(&h)?);
    let Json(req) = body?;
    if req.mode == SleepMode::Unknown {
        return Err(Error::invalid("mode", "suspend, hibernate or poweroff"));
    }
    let (who2, id2) = (who.clone(), id.clone());
    blocking(move || {
        let admin = w.ops.admins.is_admin(&who2);
        let (pc, name, dev) = linked_pc(&w, &id2, &who2, admin)?;
        if crate::pc_agent::takes_power_commands(&dev) && !dev.facts.as_ref().is_some_and(|f| f.sleep.contains(&req.mode)) {
            return Err(conflict(format!("the PC {name} cannot do that")));
        }
        pc.power_command(&name, |cid| AgentCommand::Sleep { id: cid, mode: req.mode })
    })
    .await??;
    audit(&who, &format!("sleep ({:?})", req.mode), &id);
    Ok(StatusCode::ACCEPTED)
}

async fn enable(State(st): State<WolState>, h: HeaderMap, UrlPath(id): UrlPath<String>) -> Res<StatusCode> {
    let (w, who) = (svc(&st)?, user(&h)?);
    w.ops.admins.require_admin(&who, "Wake-on-LAN")?;
    let (who2, id2) = (who.clone(), id.clone());
    blocking(move || {
        let (pc, name, dev) = linked_pc(&w, &id2, &who2, true)?;
        let nics: Vec<String> = dev
            .facts
            .as_ref()
            .map(|f| f.nics.iter().filter(|n| !n.wireless && n.wol_supported == Some(true)).map(|n| n.name.clone()).collect())
            .unwrap_or_default();
        if nics.is_empty() && crate::pc_agent::takes_power_commands(&dev) {
            return Err(conflict(format!("the PC {name} has no wired card that supports Wake-on-LAN")));
        }
        pc.power_command(&name, |cid| AgentCommand::EnableWol { id: cid, nics })
    })
    .await??;
    audit(&who, "enable", &id);
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, mac: &str) -> WolDevice {
        WolDevice { name: name.into(), macs: vec![mac.into()], ..Default::default() }
    }

    #[test]
    fn ids() {
        assert_eq!(slug("Anna's PC (office)"), "anna-s-pc-office");
        assert_eq!(slug("  ---  "), "device");
        assert_eq!(slug("Città"), "citt");
        assert_eq!(slug(&"x".repeat(40)).len(), 32);
        let taken = |x: &str| ["pc", "pc-2"].contains(&x);
        assert_eq!(unique_id("pc", &taken), "pc-3");
        assert_eq!(unique_id("tv", &taken), "tv");
        let long = "a".repeat(32);
        let id = unique_id(&long, &|x| x == long);
        assert_eq!(id.len(), 32);
        assert!(id.ends_with("-2") && valid_id(&id));
        assert!(!valid_id("Bad") && !valid_id("") && !valid_id(&"a".repeat(33)) && valid_id("a-1"));
    }

    #[test]
    fn validation() {
        let c = normalize_config(WolConfig { devices: vec![dev("Office PC", "AA-BB-CC-DD-EE-F0"), dev("Office PC", "aa:bb:cc:dd:ee:f1")] }).unwrap();
        assert_eq!(c.devices[0].id, "office-pc");
        assert_eq!(c.devices[1].id, "office-pc-2");
        assert_eq!(c.devices[0].macs, ["aa:bb:cc:dd:ee:f0"]);
        // the same MAC twice
        let e = normalize_config(WolConfig { devices: vec![dev("a", "aa:bb:cc:dd:ee:f0"), dev("b", "AA:BB:CC:DD:EE:F0")] }).unwrap_err();
        assert_eq!(e.field.as_deref(), Some("devices.1.macs"));
        let bad = |f: &dyn Fn(&mut WolDevice)| {
            let mut d = dev("pc", "aa:bb:cc:dd:ee:f0");
            f(&mut d);
            normalize_device(d).unwrap_err().field.unwrap()
        };
        assert_eq!(bad(&|d| d.macs.clear()), "macs");
        assert_eq!(bad(&|d| d.macs = vec!["ff:ff:ff:ff:ff:ff".into()]), "macs");
        assert_eq!(bad(&|d| d.name = " ".into()), "name");
        assert_eq!(bad(&|d| d.wake_timeout_s = 29), "wake_timeout_s");
        assert_eq!(bad(&|d| d.wake_timeout_s = 901), "wake_timeout_s");
        assert_eq!(bad(&|d| d.ports = (1..=17).collect()), "ports");
        assert_eq!(bad(&|d| d.ports = vec![0]), "ports");
        assert_eq!(bad(&|d| d.secureon = Some("1.2.3".into())), "secureon");
        assert_eq!(bad(&|d| d.host = Some("bad host".into())), "host");
        assert_eq!(bad(&|d| d.iface = Some("a/b".into())), "iface");
        assert_eq!(bad(&|d| d.id = "Bad".into()), "id");
        assert_eq!(bad(&|d| d.users = vec!["bad user".into()]), "users");
        let mut d = dev("pc", "aa:bb:cc:dd:ee:f0");
        d.secureon = Some("01-02-03-0A-0B-0C".into());
        d.ports = vec![445, 22, 22];
        d.port = 0;
        d.host = Some(" pc.lan ".into());
        let d = normalize_device(d).unwrap();
        assert_eq!(d.secureon.as_deref(), Some("01:02:03:0a:0b:0c"));
        assert_eq!(d.ports, [22, 445]);
        assert_eq!(d.host.as_deref(), Some("pc.lan"));
    }

    #[test]
    fn visibility() {
        let mut d = dev("pc", "aa:bb:cc:dd:ee:f0");
        assert!(visible(&d, "admin", true));
        assert!(!visible(&d, "bob", false));
        d.users = vec!["bob".into()];
        assert!(visible(&d, "bob", false));
        assert!(!visible(&d, "carol", false));
    }

    #[test]
    fn backup_wake_decision() {
        let tz = jiff::tz::TimeZone::UTC;
        // Wednesday 2027-01-13 10:00 UTC
        let now = jiff::civil::date(2027, 1, 13).at(10, 0, 0, 0).to_zoned(tz.clone()).unwrap().timestamp().as_millisecond() as u64;
        let h = 3_600_000;
        // due, off, never woken for a backup: wake
        assert_eq!(backup_wake(Some(100), false, false, None, now, &tz), Some(100));
        // woken yesterday (even for the same due time, which failed): again today
        assert_eq!(backup_wake(Some(100), false, false, Some(now - 20 * h), now, &tz), Some(100));
        // already today: not again (no storm after a failed wake)
        assert_eq!(backup_wake(Some(100), false, false, Some(now - 2 * h), now, &tz), None);
        // just before midnight, then just after: the 30 minutes still hold
        let midnight = now + 14 * h;
        assert_eq!(backup_wake(Some(100), false, false, Some(midnight - 10 * 60_000), midnight + 10 * 60_000, &tz), None);
        assert_eq!(backup_wake(Some(100), false, false, Some(midnight - 10 * 60_000), midnight + 25 * 60_000, &tz), Some(100));
        // on, busy or nothing due
        assert_eq!(backup_wake(Some(100), true, false, None, now, &tz), None);
        assert_eq!(backup_wake(Some(100), false, true, None, now, &tz), None);
        assert_eq!(backup_wake(None, false, false, None, now, &tz), None);
    }

    #[test]
    fn step_lines_keep_the_log_text() {
        let l = |s: WolWakeStep| step_line("pc", &s);
        assert_eq!(l(WolWakeStep::Starting { name: "pc".into(), macs: vec!["aa:bb:cc:dd:ee:f0".into(), "aa:bb:cc:dd:ee:f1".into()] }), "Waking pc (aa:bb:cc:dd:ee:f0, aa:bb:cc:dd:ee:f1)");
        assert_eq!(l(WolWakeStep::Sent { ifaces: vec!["eth0".into(), "eth1".into()] }), "Sent on eth0, eth1 (3×)");
        let failed = vec![WolSendError { iface: "eth1".into(), error: "down".into() }, WolSendError { iface: "eth2".into(), error: "no carrier".into() }];
        assert_eq!(l(WolWakeStep::NotSent { failed }), "Not sent on eth1: down\nNot sent on eth2: no carrier");
        assert_eq!(l(WolWakeStep::NoInterface { iface: Some("eth9".into()) }), "no interface eth9 on this NAS");
        assert_eq!(l(WolWakeStep::SentAgain { ifaces: vec!["eth0".into()], secs: 30 }), "Sent again on eth0 after 30 s");
        assert_eq!(l(WolWakeStep::AlreadyOn), "pc is already on");
        assert_eq!(l(WolWakeStep::WaitAgent { secs: 12 }), "Network up after 12 s, waiting for the agent");
        assert_eq!(l(WolWakeStep::Online { secs: 20 }), "Online after 20 s");
        assert_eq!(l(WolWakeStep::Timeout { secs: 180 }), "No answer within 180 s");
    }

    #[test]
    fn what_other_users_see() {
        let mut d = dev("pc", "a8:bb:cc:dd:ee:f0");
        d.secureon = Some("01:02:03:04:05:06".into());
        d.users = vec!["bob".into(), "carol".into()];
        d.notes = "under the desk".into();
        let wake = |by: WolTrigger| WolWake { at_ms: 1, by, end: WolWakeEnd::Online, secs: Some(9), ifaces: vec![], upid: None, step: None };
        let v = WolDeviceView {
            device: d,
            status: WolStatus::Online,
            probe: None,
            since_ms: None,
            ip: None,
            send_iface: None,
            waking: None,
            waking_since_ms: None,
            wake_step: None,
            history: vec![wake(WolTrigger::User { name: "carol".into() }), wake(WolTrigger::User { name: "bob".into() }), wake(WolTrigger::Backup)],
            agent: None,
            warnings: vec![],
            can_manage: false,
        };
        let r = redact(v, "bob");
        assert!(r.device.secureon.is_none() && r.device.users.is_empty() && r.device.notes.is_empty());
        assert_eq!(r.history[0].by, WolTrigger::User { name: String::new() });
        assert_eq!(r.history[1].by, WolTrigger::User { name: "bob".into() });
        assert_eq!(r.history[2].by, WolTrigger::Backup);
    }

    #[test]
    fn device_warnings() {
        use rynas_api_types::pcagent::AgentNic;
        let ifaces = vec![NasIface {
            name: "eth0".into(),
            mac: Some("02:00:00:00:00:01".into()),
            up: true,
            broadcast: true,
            ipv4: vec![(Ipv4Addr::new(192, 0, 2, 5), 24)],
            ..Default::default()
        }];
        let d = dev("pc", "a8:bb:cc:dd:ee:f0");
        let mut facts = AgentFacts {
            nics: vec![AgentNic { name: "enp3s0".into(), mac: "a8:bb:cc:dd:ee:f0".into(), wol_supported: Some(true), wol_enabled: Some(false), ..Default::default() }],
            ..Default::default()
        };
        assert_eq!(warnings(&d, Some(&facts), Some(Ipv4Addr::new(192, 0, 2, 20)), &ifaces), [WolWarning::Disabled]);
        assert_eq!(warnings(&d, None, Some(Ipv4Addr::new(10, 0, 0, 20)), &ifaces), [WolWarning::NoRoute]);
        facts.nics[0].wireless = true;
        assert_eq!(warnings(&d, Some(&facts), None, &ifaces), [WolWarning::Wireless]);
        facts.nics[0].wireless = false;
        facts.nics[0].wol_supported = Some(false);
        assert_eq!(warnings(&d, Some(&facts), None, &ifaces), [WolWarning::Unsupported]);
        assert_eq!(warnings(&dev("pc", "da:00:00:00:00:01"), None, None, &ifaces), [WolWarning::RandomMac]);
        let own = own_macs(&ifaces);
        assert!(own.contains("02:00:00:00:00:01"));
    }
}
