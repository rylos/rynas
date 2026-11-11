//! Date, time and time zone of the system (`/v1/system/time`, administrators only; D-TIME,
//! `docs/phase2/date-time.md`).
//!
//! The system holds the state, nothing is kept in rynas's configuration:
//! - the zone and the hardware clock go through timedated (`timedatectl`), which writes
//!   `/etc/localtime` and `/etc/adjtime` itself: rynasd's sandbox (ProtectClock) needs no change;
//! - the time daemon is detected: chrony when installed, else `systemd-timesyncd`; with none,
//!   turning sync on installs `systemd-timesyncd`;
//! - rynas's servers are one drop-in of that daemon (`timesyncd.conf.d/60-rynas.conf`, or
//!   `/etc/chrony/sources.d/rynas.sources`); without it the distribution's servers are used.
//!   chrony keeps the sources of `chrony.conf` besides rynas's (said in the page);
//! - after a zone change rynasd re-reads it (`tzset`), so snapshot names and quiet hours use it at
//!   once; `rynas_power::system_tz` reads the file every time. systemd recomputes its timers by
//!   itself, `systemd-analyze` and every child started later read the new zone.
//!
//! `--dry-system`: nothing is run; the zone, sync and hardware clock live in a small file below
//! the dry root.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::clock::{ClockSettings, ClockState, MAX_SERVERS, SetClock, TimeDaemon, TimeSource, ZoneInfo};
use rynas_api_types::notify::{NewEvent, NotifyCategory, NotifySeverity};
use rynas_api_types::{TaskStarted, error_codes, validate};
use rynas_hw::under;
use rynas_notify::Notifier;
use rynas_notify::event::{code, field, firing, resolved};
use rynas_tasks::{Cmd, TaskCtx};
use serde::{Deserialize, Serialize};

use crate::ops::{Error, Ops, Res, user};

const TIMESYNCD_BIN: &str = "/usr/lib/systemd/systemd-timesyncd";
const CHRONYD_BIN: &str = "/usr/sbin/chronyd";
const TIMESYNCD_UNIT: &str = "systemd-timesyncd.service";
const CHRONY_UNIT: &str = "chrony.service";
const TIMESYNCD_CONF: &str = "/etc/systemd/timesyncd.conf";
const TIMESYNCD_DROPIN: &str = "/etc/systemd/timesyncd.conf.d/60-rynas.conf";
const CHRONY_CONF: &str = "/etc/chrony/chrony.conf";
const CHRONY_SOURCES_DIR: &str = "/etc/chrony/sources.d";
const CHRONY_SOURCES: &str = "/etc/chrony/sources.d/rynas.sources";
const CHRONY_DHCP_DIR: &str = "/run/chrony-dhcp";
const ZONEINFO: &str = "/usr/share/zoneinfo";
const TZDATA: &str = "/usr/share/zoneinfo/tzdata.zi";
const DRY_STATE: &str = "/var/lib/rynas-dry-clock.json";
const MANAGED: &str = "# Managed by rynas (System > Date and time): changes here are replaced.\n";
const PACKAGE: &str = "systemd-timesyncd";
/// The regions of the IANA names offered (links like `US/Eastern` or `Etc/GMT+5` are not).
const REGIONS: [&str; 10] =
    ["Africa", "America", "Antarctica", "Arctic", "Asia", "Atlantic", "Australia", "Europe", "Indian", "Pacific"];

const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const ACTION_TIMEOUT: Duration = Duration::from_secs(60);
const APT_TIMEOUT: Duration = Duration::from_secs(1800);

// ------------------------------------------------------------------ parsing

/// What timedated says (`timedatectl show`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Timedate {
    pub timezone: String,
    pub local_rtc: bool,
    /// timedated's own idea of automatic sync (the units it knows).
    pub ntp: bool,
    pub synchronized: bool,
}

pub fn parse_show(text: &str) -> Timedate {
    let mut t = Timedate::default();
    for l in text.lines() {
        let Some((k, v)) = l.split_once('=') else { continue };
        let yes = v.trim() == "yes";
        match k {
            "Timezone" => t.timezone = v.trim().to_string(),
            "LocalRTC" => t.local_rtc = yes,
            "NTP" => t.ntp = yes,
            "NTPSynchronized" => t.synchronized = yes,
            _ => {}
        }
    }
    if t.timezone.is_empty() {
        t.timezone = "UTC".into();
    }
    t
}

/// The properties of `org.freedesktop.timesync1.Manager` rynas reads, in this order.
const TIMESYNC_PROPS: [&str; 6] =
    ["ServerName", "ServerAddress", "NTPMessage", "PollIntervalUSec", "FallbackNTPServers", "LinkNTPServers"];

/// timesyncd's answer: the source, the distribution's servers, the DHCP ones.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Timesync {
    pub source: Option<TimeSource>,
    pub fallback: Vec<String>,
    pub link: Vec<String>,
}

fn address(v: &serde_json::Value) -> Option<String> {
    let a = v.as_array()?;
    let bytes: Vec<u8> = a.get(1)?.as_array()?.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect();
    match bytes.len() {
        4 => Some(std::net::Ipv4Addr::from(<[u8; 4]>::try_from(bytes).ok()?).to_string()),
        16 => Some(std::net::Ipv6Addr::from(<[u8; 16]>::try_from(bytes).ok()?).to_string()),
        _ => None,
    }
}

fn strings(v: &serde_json::Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// `busctl --json=short get-property …` with [`TIMESYNC_PROPS`]: one JSON value per line.
/// NTPMessage `(uuuuittayttttbtt)`: leap, version, mode, stratum, precision, root delay,
/// root dispersion, reference, origin, receive, transmit and destination times (µs), spike,
/// packet count, jitter.
pub fn parse_timesync(text: &str) -> Timesync {
    let v: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|j| j.get("data").cloned().unwrap_or_default())
        .collect();
    let get = |i: usize| v.get(i).cloned().unwrap_or_default();
    let name = get(0).as_str().unwrap_or("").to_string();
    let addr = address(&get(1));
    let msg = get(2);
    let m = msg.as_array();
    let t = |i: usize| m.and_then(|m| m.get(i)).and_then(|x| x.as_i64()).unwrap_or(0);
    let (origin, recv, trans, dest) = (t(8), t(9), t(10), t(11));
    let answered = m.is_some() && dest > 0 && origin > 0;
    let poll = get(3).as_u64().map(|us| (us / 1_000_000) as u32);
    let source = (!name.is_empty() || addr.is_some()).then(|| TimeSource {
        server: if name.is_empty() { addr.clone().unwrap_or_default() } else { name },
        address: addr,
        stratum: answered.then_some(t(3) as u8).filter(|s| *s > 0),
        offset_us: answered.then_some(((recv - origin) + (trans - dest)) / 2),
        last_sync_ms: answered.then_some((dest / 1000) as u64),
        poll_s: poll.filter(|p| *p > 0),
    });
    Timesync { source, fallback: strings(&get(4)), link: strings(&get(5)) }
}

/// `chronyc -c -n tracking`: reference id, address, stratum, reference time, system time
/// correction (s, positive: the clock is slow), last offset, …, update interval, leap status.
pub fn parse_tracking(text: &str) -> Option<TimeSource> {
    let f: Vec<&str> = text.lines().next()?.split(',').collect();
    if f.len() < 14 || f[0].trim_start_matches('0').is_empty() {
        return None;
    }
    let num = |i: usize| f.get(i).and_then(|x| x.parse::<f64>().ok());
    let stratum: Option<u8> = f[2].parse().ok().filter(|s| *s > 0);
    let at = num(3).filter(|t| *t > 0.0);
    Some(TimeSource {
        server: f[1].to_string(),
        address: None,
        stratum,
        offset_us: num(4).map(|s| (s * 1e6).round() as i64),
        last_sync_ms: at.map(|t| (t * 1000.0) as u64),
        poll_s: num(12).map(|s| s.round() as u32).filter(|p| *p > 0),
    })
}

/// `pool`/`server` names of chrony configuration text.
pub fn chrony_sources(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            matches!(w.next(), Some("pool" | "server")).then(|| w.next().map(str::to_string)).flatten()
        })
        .collect()
}

/// The servers of a timesyncd file: the last `NTP=` (or `FallbackNTP=`) line of `[Time]`.
pub fn timesyncd_list(text: &str, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    for l in text.lines() {
        if let Some((k, v)) = l.split_once('=')
            && k.trim() == key
        {
            out = v.split_whitespace().map(str::to_string).collect();
        }
    }
    out
}

pub fn render_timesyncd(servers: &[String]) -> String {
    // the empty assignment drops the servers of files read before this one
    format!("{MANAGED}[Time]\nNTP=\nNTP={}\n", servers.join(" "))
}

pub fn render_chrony(servers: &[String]) -> String {
    let mut out = MANAGED.to_string();
    for s in servers {
        let kind = if s.contains("pool") { "pool" } else { "server" };
        out.push_str(&format!("{kind} {s} iburst\n"));
    }
    out
}

/// The zones of `tzdata.zi` (`Z name …` and `L target name`) in the offered regions, sorted,
/// with `UTC`.
pub fn parse_zones(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            match w.next()? {
                "Z" => w.next(),
                "L" => w.nth(1),
                _ => None,
            }
        })
        .filter(|n| n.split_once('/').is_some_and(|(r, _)| REGIONS.contains(&r)) && validate::time_zone(n))
        .map(str::to_string)
        .collect();
    v.push("UTC".into());
    v.sort();
    v.dedup();
    v
}

/// The offset now of the zone in a TZif file.
fn tzif_offset(name: &str, data: &[u8]) -> Option<i32> {
    let tz = jiff::tz::TimeZone::tzif(name, data).ok()?;
    Some(tz.to_offset(jiff::Timestamp::now()).seconds())
}

// ------------------------------------------------------------------ checks

pub fn validate(req: &ClockSettings, now: &ClockState, zone_exists: impl Fn(&str) -> bool) -> Res<()> {
    if !validate::time_zone(&req.timezone) || !zone_exists(&req.timezone) {
        return Err(Error::invalid("timezone", format!("{:?} is not a time zone of this system", req.timezone)));
    }
    if req.servers.len() > MAX_SERVERS {
        return Err(Error::invalid("servers", format!("at most {MAX_SERVERS} servers")));
    }
    for (i, s) in req.servers.iter().enumerate() {
        if !validate::ntp_server(s) {
            return Err(Error::invalid("servers", format!("{s:?} is not a host name or an address")));
        }
        if req.servers[..i].contains(s) {
            return Err(Error::invalid("servers", format!("{s} is listed twice")));
        }
    }
    if req.local_rtc && !now.local_rtc {
        return Err(Error::invalid("local_rtc", "the hardware clock is only moved to UTC"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Zone(String),
    RtcUtc,
    Install,
    Servers,
    SyncOn,
    SyncOff,
    Reload,
}

/// What an apply does, in order.
pub fn plan(req: &ClockSettings, now: &ClockState) -> Vec<Step> {
    let mut v = Vec::new();
    if req.timezone != now.timezone {
        v.push(Step::Zone(req.timezone.clone()));
    }
    if now.local_rtc && !req.local_rtc {
        v.push(Step::RtcUtc);
    }
    let servers = req.servers != now.servers;
    // before the package: its postinst starts the daemon, which then reads rynas's servers
    if servers && (now.daemon != TimeDaemon::None || req.sync_enabled) {
        v.push(Step::Servers);
    }
    if req.sync_enabled && now.daemon == TimeDaemon::None {
        v.push(Step::Install);
    }
    match (now.sync_enabled, req.sync_enabled) {
        (false, true) => v.push(Step::SyncOn),
        (true, false) => v.push(Step::SyncOff),
        (true, true) if servers => v.push(Step::Reload),
        _ => {}
    }
    v
}

/// The command of a step (`None`: done in rynasd).
pub fn step_cmd(step: &Step, daemon: TimeDaemon) -> Option<Cmd> {
    let td = |args: &[&str]| Cmd::new("timedatectl").args(args.iter().copied()).timeout(ACTION_TIMEOUT);
    let sys = |args: &[&str]| Cmd::new("systemctl").args(args.iter().copied()).timeout(ACTION_TIMEOUT);
    let chrony = daemon == TimeDaemon::Chrony;
    Some(match step {
        Step::Zone(z) => td(&["set-timezone", z]),
        Step::RtcUtc => td(&["set-local-rtc", "0"]),
        Step::Install => crate::updates::apt(&["install", "--", PACKAGE], APT_TIMEOUT),
        Step::Servers => return None,
        Step::SyncOn if chrony => sys(&["enable", "--now", CHRONY_UNIT]),
        Step::SyncOn => td(&["set-ntp", "true"]),
        Step::SyncOff if chrony => sys(&["disable", "--now", CHRONY_UNIT]),
        Step::SyncOff => td(&["set-ntp", "false"]),
        Step::Reload if chrony => Cmd::new("chronyc").args(["reload", "sources"]).timeout(ACTION_TIMEOUT),
        Step::Reload => sys(&["restart", TIMESYNCD_UNIT]),
    })
}

/// `timedatectl set-time` argument of a manual setting.
pub fn set_time_arg(req: &SetClock) -> Res<String> {
    match (req.epoch_ms, req.local.as_deref()) {
        (Some(ms), None) if ms >= 946_684_800_000 => Ok(format!("@{}", ms / 1000)),
        (None, Some(l)) if rynas_api_types::clock::local_time_ok(l) && !l.starts_with("19") => Ok(l.to_string()),
        (Some(_), None) | (None, Some(_)) => Err(Error::invalid("local", "a date after 2000 is needed")),
        _ => Err(Error::bad_request("give epoch_ms or local, not both")),
    }
}

// ------------------------------------------------------------------ the machine

/// Read-only questions (fake in tests): the output of a command that succeeded.
pub trait ClockProbe: Send + Sync {
    fn query(&self, cmd: &Cmd) -> Result<String, String>;
}

pub struct HostProbe;

impl ClockProbe for HostProbe {
    fn query(&self, cmd: &Cmd) -> Result<String, String> {
        let o = cmd.output_blocking().map_err(|e| format!("{}: {e}", cmd.display()))?;
        if o.success() { Ok(o.stdout) } else { Err(o.stderr.trim().to_string()) }
    }
}

/// `--dry-system`: what a real machine would keep.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DryState {
    timezone: String,
    sync_enabled: bool,
    local_rtc: bool,
}

pub struct Clock {
    /// Where the files are ("/", the dry root, or a test directory).
    pub root: PathBuf,
    pub probe: Arc<dyn ClockProbe>,
    pub dry: bool,
    pub busy: AtomicBool,
}

fn q(program: &str, args: &[&str]) -> Cmd {
    Cmd::new(program).args(args.iter().copied()).timeout(QUERY_TIMEOUT)
}

impl Clock {
    pub fn new(root: PathBuf, dry: bool) -> Self {
        Clock { root, probe: Arc::new(HostProbe), dry, busy: AtomicBool::new(false) }
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(under(&self.root, path)).ok()
    }

    fn dry_state(&self) -> DryState {
        self.read(DRY_STATE).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(DryState {
            timezone: "UTC".into(),
            sync_enabled: true,
            local_rtc: false,
        })
    }

    fn save_dry(&self, s: &DryState) -> Result<(), String> {
        self.write_file(DRY_STATE, &serde_json::to_string(s).unwrap_or_default())
    }

    pub fn daemon(&self) -> TimeDaemon {
        if self.dry {
            TimeDaemon::Timesyncd
        } else if under(&self.root, CHRONYD_BIN).exists() {
            TimeDaemon::Chrony
        } else if under(&self.root, TIMESYNCD_BIN).exists() {
            TimeDaemon::Timesyncd
        } else {
            TimeDaemon::None
        }
    }

    fn timedate(&self) -> Timedate {
        if self.dry {
            let d = self.dry_state();
            return Timedate { timezone: d.timezone, local_rtc: d.local_rtc, ntp: d.sync_enabled, synchronized: d.sync_enabled };
        }
        self.probe.query(&q("timedatectl", &["show"])).map(|t| parse_show(&t)).unwrap_or_else(|e| {
            tracing::warn!("timedatectl show: {e}");
            parse_show("")
        })
    }

    fn active(&self, unit: &str) -> bool {
        self.probe.query(&q("systemctl", &["is-active", unit])).is_ok_and(|t| t.trim() == "active")
    }

    /// Where the zone database is: the machine's even on a dry system (read only).
    fn zone_root(&self) -> &Path {
        if self.dry { Path::new("/") } else { &self.root }
    }

    fn zone_file(&self, name: &str) -> Option<Vec<u8>> {
        if !validate::time_zone(name) {
            return None;
        }
        std::fs::read(under(self.zone_root(), &format!("{ZONEINFO}/{name}"))).ok().filter(|d| d.starts_with(b"TZif"))
    }

    /// The zone file exists below the zoneinfo folder and is a TZif file.
    pub fn zone_exists(&self, name: &str) -> bool {
        self.zone_file(name).is_some()
    }

    fn offset_of(&self, name: &str) -> Option<i32> {
        tzif_offset(name, &self.zone_file(name)?)
    }

    /// `GET /system/time/zones`.
    pub fn zones(&self) -> Vec<ZoneInfo> {
        let zi = std::fs::read_to_string(under(self.zone_root(), TZDATA)).unwrap_or_default();
        let mut names = parse_zones(&zi);
        let current = self.timedate().timezone;
        if !names.contains(&current) {
            names.push(current);
        }
        names.into_iter().filter_map(|n| Some(ZoneInfo { offset_s: self.offset_of(&n)?, name: n })).collect()
    }

    fn dir_sources(&self, dir: &str, skip: Option<&str>) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(under(&self.root, dir)) else { return Vec::new() };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "sources") && skip.is_none_or(|s| !p.ends_with(s)))
            .collect();
        files.sort();
        files.iter().flat_map(|p| chrony_sources(&std::fs::read_to_string(p).unwrap_or_default())).collect()
    }

    pub fn state(&self) -> ClockState {
        let td = self.timedate();
        let daemon = self.daemon();
        let mut s = ClockState {
            now_ms: rynas_tasks::now_ms(),
            utc_offset_s: self.offset_of(&td.timezone).unwrap_or(0),
            timezone: td.timezone,
            local_rtc: td.local_rtc,
            daemon,
            synchronized: td.synchronized,
            ..Default::default()
        };
        match daemon {
            TimeDaemon::Timesyncd => {
                s.sync_enabled = td.ntp;
                s.servers = timesyncd_list(&self.read(TIMESYNCD_DROPIN).unwrap_or_default(), "NTP");
                s.default_servers = timesyncd_list(&self.read(TIMESYNCD_CONF).unwrap_or_default(), "FallbackNTP");
                if self.dry && s.sync_enabled {
                    s.source = Some(TimeSource {
                        server: s.servers.first().cloned().unwrap_or_else(|| "pool.ntp.example".into()),
                        offset_us: Some(-1200),
                        last_sync_ms: Some(s.now_ms.saturating_sub(64_000)),
                        poll_s: Some(2048),
                        stratum: Some(2),
                        address: None,
                    });
                } else if s.sync_enabled {
                    // never wakes a stopped timesyncd up through D-Bus activation
                    let mut args = vec!["--json=short", "--auto-start=no", "get-property", "org.freedesktop.timesync1"];
                    args.extend(["/org/freedesktop/timesync1", "org.freedesktop.timesync1.Manager"]);
                    args.extend(TIMESYNC_PROPS);
                    if let Ok(out) = self.probe.query(&q("busctl", &args)) {
                        let t = parse_timesync(&out);
                        s.source = t.source;
                        if !t.fallback.is_empty() {
                            s.default_servers = t.fallback;
                        }
                        s.dhcp_servers = t.link;
                    }
                }
            }
            TimeDaemon::Chrony => {
                s.sync_enabled = self.active(CHRONY_UNIT);
                s.servers = chrony_sources(&self.read(CHRONY_SOURCES).unwrap_or_default());
                s.default_servers = chrony_sources(&self.read(CHRONY_CONF).unwrap_or_default());
                s.default_servers.extend(self.dir_sources(CHRONY_SOURCES_DIR, Some("rynas.sources")));
                s.dhcp_servers = self.dir_sources(CHRONY_DHCP_DIR, None);
                if s.sync_enabled {
                    s.source = self.probe.query(&q("chronyc", &["-c", "-n", "tracking"])).ok().and_then(|t| parse_tracking(&t));
                }
            }
            TimeDaemon::None => {}
        }
        s
    }

    fn write_file(&self, path: &str, text: &str) -> Result<(), String> {
        let p = under(&self.root, path);
        let fail = |e: std::io::Error| format!("{}: {e}", p.display());
        let dir = p.parent().unwrap_or(Path::new("/"));
        if !dir.exists() {
            std::fs::create_dir_all(dir).map_err(fail)?;
            std::fs::set_permissions(dir, PermissionsExt::from_mode(0o755)).map_err(fail)?;
        }
        let new = p.with_extension("rynas-new");
        std::fs::write(&new, text).map_err(fail)?;
        std::fs::set_permissions(&new, PermissionsExt::from_mode(0o644)).map_err(fail)?;
        std::fs::rename(&new, &p).map_err(fail)
    }

    fn write_servers(&self, daemon: TimeDaemon, servers: &[String]) -> Result<(), String> {
        let (path, text) = match daemon {
            TimeDaemon::Chrony => (CHRONY_SOURCES, render_chrony(servers)),
            _ => (TIMESYNCD_DROPIN, render_timesyncd(servers)),
        };
        if servers.is_empty() {
            match std::fs::remove_file(under(&self.root, path)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("{path}: {e}")),
                _ => return Ok(()),
            }
        }
        self.write_file(path, &text)
    }

    async fn run(&self, ctx: &TaskCtx, cmd: Cmd) -> Result<(), String> {
        if self.dry {
            ctx.log(&format!("dry system, not run: {}", cmd.display()));
            return Ok(());
        }
        ctx.run(&cmd).await.map(|_| ()).map_err(|e| e.to_string())
    }

    /// The task behind `POST /v1/system/time`.
    async fn apply(&self, ctx: &TaskCtx, req: &ClockSettings, now: &ClockState) -> Result<(), String> {
        let mut daemon = now.daemon;
        for step in plan(req, now) {
            match &step {
                Step::Servers => {
                    let d = if daemon == TimeDaemon::None { TimeDaemon::Timesyncd } else { daemon };
                    ctx.log(&if req.servers.is_empty() {
                        "time servers: the distribution's".to_string()
                    } else {
                        format!("time servers: {}", req.servers.join(", "))
                    });
                    self.write_servers(d, &req.servers)?;
                }
                Step::Install => {
                    ctx.log(&format!("installing {PACKAGE} from the Debian archive"));
                    self.run(ctx, step_cmd(&step, daemon).expect("a command")).await?;
                    daemon = TimeDaemon::Timesyncd;
                }
                _ => {
                    if let Some(cmd) = step_cmd(&step, daemon) {
                        self.run(ctx, cmd).await?;
                    }
                }
            }
            if let Step::Zone(z) = &step {
                refresh_zone();
                ctx.log(&format!("time zone: {z}; running apps keep the old one until they restart"));
            }
        }
        if self.dry {
            let mut d = self.dry_state();
            d.timezone = req.timezone.clone();
            d.sync_enabled = req.sync_enabled;
            d.local_rtc = req.local_rtc;
            self.save_dry(&d)?;
        }
        Ok(())
    }

    /// The task behind `POST /v1/system/time/sync-now`: a fresh question to the servers, then
    /// up to 30 s for the kernel to count the clock as synchronized.
    async fn sync_now(&self, ctx: &TaskCtx, daemon: TimeDaemon) -> Result<(), String> {
        match daemon {
            TimeDaemon::Chrony => {
                self.run(ctx, Cmd::new("chronyc").args(["burst", "4/4"]).timeout(ACTION_TIMEOUT)).await?;
                if !self.dry {
                    tokio::time::sleep(Duration::from_secs(8)).await;
                }
                self.run(ctx, Cmd::new("chronyc").arg("makestep").timeout(ACTION_TIMEOUT)).await?;
            }
            _ => self.run(ctx, Cmd::new("systemctl").args(["restart", TIMESYNCD_UNIT]).timeout(ACTION_TIMEOUT)).await?,
        }
        for _ in 0..15 {
            if ctx.is_cancelled() {
                return Err("cancelled".into());
            }
            if self.timedate().synchronized {
                ctx.log("the clock is synchronized");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err("no time server answered within 30 s".into())
    }
}

unsafe extern "C" {
    fn tzset();
}

/// glibc keeps the zone it read first (`localtime_r` never looks again): read
/// `/etc/localtime` again after a change.
pub fn refresh_zone() {
    // SAFETY: tzset takes glibc's own zone lock; it only reads TZ and the zone file.
    unsafe { tzset() };
}

// ------------------------------------------------------------------ notifications

/// Not synchronized for this long while automatic sync is on: notified.
pub const UNSYNCED_AFTER_MS: u64 = 60 * 60 * 1000;
/// Further than this from the source in two looks in a row: notified; back below half: over.
pub const DRIFT_US: i64 = 1_000_000;
const SRC: &str = "time";
const KEY_UNSYNCED: &str = "time:unsynced";
const KEY_DRIFT: &str = "time:drift";

/// The state between two looks of [`spawn_watch`].
#[derive(Debug, Default)]
pub struct Watch {
    unsynced_since: Option<u64>,
    unsynced_open: bool,
    drift_seen: u32,
    drift_open: bool,
}

fn seconds(us: i64) -> String {
    format!("{:+.1} s", us as f64 / 1e6)
}

impl Watch {
    pub fn step(&mut self, s: &ClockState) -> Vec<NewEvent> {
        let mut out = Vec::new();
        let lost = s.sync_enabled && !s.synchronized;
        if lost {
            let since = *self.unsynced_since.get_or_insert(s.now_ms);
            let minutes = s.now_ms.saturating_sub(since) / 60_000;
            if !self.unsynced_open && s.now_ms.saturating_sub(since) >= UNSYNCED_AFTER_MS {
                self.unsynced_open = true;
                let ev = firing(NotifyCategory::System, NotifySeverity::Warning, SRC, KEY_UNSYNCED, "The clock is not synchronized");
                let mut ev = code(ev, "system.clock_unsynced");
                ev.body = format!(
                    "No time server has answered for {minutes} minutes. Certificates, two-factor codes, schedules and snapshots need the right time: check the network and the time servers."
                );
                out.push(field(ev, "minutes", minutes));
            }
        } else {
            self.unsynced_since = None;
            if std::mem::take(&mut self.unsynced_open) {
                let ev = resolved(NotifyCategory::System, SRC, KEY_UNSYNCED, "The clock is synchronized again");
                out.push(code(ev, "system.clock_synced"));
            }
        }
        let off = s.source.as_ref().and_then(|x| x.offset_us).filter(|_| s.sync_enabled);
        match off {
            Some(o) if o.abs() > DRIFT_US => {
                self.drift_seen += 1;
                if self.drift_seen >= 2 && !self.drift_open {
                    self.drift_open = true;
                    let ev = firing(NotifyCategory::System, NotifySeverity::Warning, SRC, KEY_DRIFT, "The clock is drifting");
                    let mut ev = code(ev, "system.clock_drift");
                    ev.body = format!(
                        "The clock is {} away from the time server and is not catching up. Check the time servers.",
                        seconds(o)
                    );
                    out.push(field(ev, "offset", seconds(o)));
                }
            }
            Some(o) if o.abs() > DRIFT_US / 2 => {}
            _ => {
                self.drift_seen = 0;
                if std::mem::take(&mut self.drift_open) {
                    let ev = resolved(NotifyCategory::System, SRC, KEY_DRIFT, "The clock is in step again");
                    out.push(code(ev, "system.clock_in_step"));
                }
            }
        }
        out
    }
}

/// Every five minutes: the sync state turned into events.
pub fn spawn_watch(clock: Arc<Clock>, notifier: Arc<dyn Notifier>) {
    tokio::spawn(async move {
        let mut w = Watch::default();
        loop {
            tokio::time::sleep(Duration::from_secs(300)).await;
            let c = clock.clone();
            let Ok(s) = tokio::task::spawn_blocking(move || c.state()).await else { continue };
            for ev in w.step(&s) {
                notifier.emit(ev);
            }
        }
    });
}

// ------------------------------------------------------------------ API

pub struct ClockApi {
    pub clock: Arc<Clock>,
    pub ops: Arc<Ops>,
}

pub type ClockHandle = Option<Arc<ClockApi>>;

fn api(st: &ClockHandle) -> Res<Arc<ClockApi>> {
    st.clone().ok_or_else(|| {
        Error::new(StatusCode::SERVICE_UNAVAILABLE, error_codes::INTERNAL, "date and time are not available".into())
    })
}

fn conflict(msg: &str) -> Error {
    Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, msg.into())
}

pub fn router(st: ClockHandle) -> Router {
    let admins = st.as_ref().map(|s| s.ops.admins.clone()).unwrap_or_default();
    Router::new()
        .route("/v1/system/time", get(state_get).post(apply_post))
        .route("/v1/system/time/zones", get(zones_get))
        .route("/v1/system/time/set", post(set_post))
        .route("/v1/system/time/sync-now", post(sync_post))
        .with_state(st)
        .route_layer(axum::middleware::from_fn_with_state(
            crate::roles::AdminGate { policy: admins, what: "date and time" },
            crate::roles::admin_only,
        ))
}

async fn current(c: &Arc<Clock>) -> Res<ClockState> {
    let c = c.clone();
    tokio::task::spawn_blocking(move || c.state()).await.map_err(Error::internal)
}

async fn state_get(State(st): State<ClockHandle>) -> Res<Json<ClockState>> {
    current(&api(&st)?.clock).await.map(Json)
}

async fn zones_get(State(st): State<ClockHandle>) -> Res<Json<Vec<ZoneInfo>>> {
    let c = api(&st)?.clock.clone();
    tokio::task::spawn_blocking(move || c.zones()).await.map(Json).map_err(Error::internal)
}

/// Clears the busy flag when the task ends, however it ends.
struct Busy(Arc<Clock>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

fn claim(c: &Arc<Clock>) -> Res<Busy> {
    if c.busy.swap(true, Ordering::SeqCst) {
        return Err(conflict("another date and time change is running"));
    }
    Ok(Busy(c.clone()))
}

fn started(id: String) -> Response {
    (StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response()
}

async fn apply_post(
    State(st): State<ClockHandle>,
    hd: HeaderMap,
    body: Result<Json<ClockSettings>, JsonRejection>,
) -> Res<Response> {
    let (a, who) = (api(&st)?, user(&hd)?);
    let Json(req) = body?;
    let c = a.clock.clone();
    let now = current(&c).await?;
    validate(&req, &now, |z| c.zone_exists(z))?;
    let guard = claim(&c)?;
    tracing::info!(target: "rynas::audit", "time apply zone={} sync={} servers={} by {who}", req.timezone, req.sync_enabled, req.servers.join(","));
    let id = a.ops.tasks.spawn("time.apply", &who, move |ctx| async move {
        let _guard = guard;
        c.apply(&ctx, &req, &now).await
    })?;
    Ok(started(id))
}

async fn set_post(State(st): State<ClockHandle>, hd: HeaderMap, body: Result<Json<SetClock>, JsonRejection>) -> Res<Response> {
    let (a, who) = (api(&st)?, user(&hd)?);
    let Json(req) = body?;
    let arg = set_time_arg(&req)?;
    let c = a.clock.clone();
    if current(&c).await?.sync_enabled {
        return Err(conflict("turn automatic sync off before setting the time by hand"));
    }
    let guard = claim(&c)?;
    tracing::info!(target: "rynas::audit", "time set {arg} by {who}");
    let id = a.ops.tasks.spawn("time.set", &who, move |ctx| async move {
        let _guard = guard;
        c.run(&ctx, Cmd::new("timedatectl").args(["set-time", arg.as_str()]).timeout(ACTION_TIMEOUT)).await
    })?;
    Ok(started(id))
}

async fn sync_post(State(st): State<ClockHandle>, hd: HeaderMap) -> Res<Response> {
    let (a, who) = (api(&st)?, user(&hd)?);
    let c = a.clock.clone();
    let now = current(&c).await?;
    if !now.sync_enabled {
        return Err(conflict("automatic sync is off"));
    }
    let guard = claim(&c)?;
    tracing::info!(target: "rynas::audit", "time sync now by {who}");
    let id = a.ops.tasks.spawn("time.sync", &who, move |ctx| async move {
        let _guard = guard;
        c.sync_now(&ctx, now.daemon).await
    })?;
    Ok(started(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Canned answers by command line; every question recorded.
    #[derive(Default)]
    struct Fake {
        answers: HashMap<String, String>,
        asked: Mutex<Vec<String>>,
    }

    impl ClockProbe for Fake {
        fn query(&self, cmd: &Cmd) -> Result<String, String> {
            let line = cmd.display();
            self.asked.lock().unwrap().push(line.clone());
            self.answers.iter().find(|(k, _)| line.starts_with(k.as_str())).map(|(_, v)| v.clone()).ok_or(line)
        }
    }

    const SHOW: &str = "Timezone=Europe/Rome\nLocalRTC=no\nCanNTP=yes\nNTP=yes\nNTPSynchronized=yes\nTimeUSec=Wed 2026-11-11 01:09:19 CET\n";
    const BUS: &str = r#"{"type":"s","data":"2.debian.pool.ntp.org"}
{"type":"(iay)","data":[2,[192,0,2,10]]}
{"type":"(uuuuittayttttbtt)","data":[0,4,4,2,-23,19805,31173,[131,188,3,221],1794353944520596,1794353944533835,1794353944533866,1794353944542530,false,93,2820]}
{"type":"t","data":2048000000}
{"type":"as","data":["0.debian.pool.ntp.org","1.debian.pool.ntp.org"]}
{"type":"as","data":["192.0.2.1"]}
"#;

    fn machine(dir: &Path, answers: &[(&str, &str)], files: &[(&str, &str)]) -> (Clock, Arc<Fake>) {
        let fake = Arc::new(Fake { answers: answers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), ..Default::default() });
        let mut c = Clock::new(dir.to_path_buf(), false);
        c.probe = fake.clone();
        for (p, t) in files {
            let f = under(dir, p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, t).unwrap();
        }
        (c, fake)
    }

    fn settings(tz: &str, sync: bool, servers: &[&str]) -> ClockSettings {
        ClockSettings { timezone: tz.into(), sync_enabled: sync, servers: servers.iter().map(|s| s.to_string()).collect(), local_rtc: false }
    }

    #[test]
    fn timedated_answer() {
        let t = parse_show(SHOW);
        assert_eq!(t, Timedate { timezone: "Europe/Rome".into(), local_rtc: false, ntp: true, synchronized: true });
        assert_eq!(parse_show("").timezone, "UTC");
        assert!(parse_show("LocalRTC=yes\n").local_rtc);
    }

    #[test]
    fn timesyncd_answer() {
        let t = parse_timesync(BUS);
        let s = t.source.unwrap();
        assert_eq!(s.server, "2.debian.pool.ntp.org");
        assert_eq!(s.address.as_deref(), Some("192.0.2.10"));
        assert_eq!(s.stratum, Some(2));
        assert_eq!(s.offset_us, Some(2287), "((T2-T1)+(T3-T4))/2");
        assert_eq!(s.last_sync_ms, Some(1_794_353_944_542));
        assert_eq!(s.poll_s, Some(2048));
        assert_eq!(t.fallback.len(), 2);
        assert_eq!(t.link, ["192.0.2.1"]);
        // no server yet: no source
        let none = parse_timesync("{\"type\":\"s\",\"data\":\"\"}\n{\"type\":\"(iay)\",\"data\":[0,[]]}\n");
        assert_eq!(none.source, None);
        let v6 = address(&serde_json::json!([10, [32, 1, 13, 184, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]]));
        assert_eq!(v6.as_deref(), Some("2001:db8::1"));
    }

    #[test]
    fn chrony_answers() {
        let t = "C0000201,192.0.2.1,3,1794353944.123456,-0.000012345,0.000001,0.00002,-12.3,0.001,0.02,0.001,0.0002,64.5,Normal\n";
        let s = parse_tracking(t).unwrap();
        assert_eq!((s.server.as_str(), s.stratum, s.offset_us, s.poll_s), ("192.0.2.1", Some(3), Some(-12), Some(65)));
        assert_eq!(s.last_sync_ms, Some(1_794_353_944_123));
        assert_eq!(parse_tracking("00000000,,0,0.0,0,0,0,0,0,0,0,0,0,Not synchronised\n"), None);
        let conf = "# x\npool 2.debian.pool.ntp.org iburst\nsourcedir /run/chrony-dhcp\nserver ntp.example.lan iburst prefer\n";
        assert_eq!(chrony_sources(conf), ["2.debian.pool.ntp.org", "ntp.example.lan"]);
    }

    #[test]
    fn drop_ins_round_trip() {
        let v: Vec<String> = vec!["ntp.example.lan".into(), "pool.ntp.org".into()];
        let t = render_timesyncd(&v);
        assert!(t.contains("[Time]\nNTP=\nNTP=ntp.example.lan pool.ntp.org\n"), "{t}");
        assert_eq!(timesyncd_list(&t, "NTP"), v);
        let c = render_chrony(&v);
        assert!(c.contains("server ntp.example.lan iburst\npool pool.ntp.org iburst\n"), "{c}");
        assert_eq!(chrony_sources(&c), v);
        assert_eq!(timesyncd_list("[Time]\n#FallbackNTP=x\nFallbackNTP=a b\n", "FallbackNTP"), ["a", "b"]);
    }

    #[test]
    fn zone_list() {
        let zi = "# version\nZ Europe/Rome 0:49:56 - LMT 1866 D 22\nL Europe/Brussels Europe/Amsterdam\nL Etc/GMT GMT\nL America/New_York US/Eastern\nZ Etc/GMT+5 -5 - %z\nR x 1 2\n";
        assert_eq!(parse_zones(zi), ["Europe/Amsterdam", "Europe/Rome", "UTC"]);
    }

    #[test]
    fn checks() {
        let now = ClockState { timezone: "UTC".into(), ..Default::default() };
        let ok = |_: &str| true;
        assert!(validate(&settings("Europe/Rome", true, &["ntp.example.lan"]), &now, ok).is_ok());
        assert!(validate(&settings("Europe/Rome", true, &[]), &now, |_| false).is_err(), "not on this system");
        assert!(validate(&settings("../../etc/passwd", true, &[]), &now, ok).is_err());
        assert!(validate(&settings("UTC", true, &["a b"]), &now, ok).is_err());
        assert!(validate(&settings("UTC", true, &["x\nallow"]), &now, ok).is_err(), "never reaches a file");
        assert!(validate(&settings("UTC", true, &["a", "a"]), &now, ok).is_err());
        assert!(validate(&settings("UTC", true, &["a"; 9]), &now, ok).is_err());
        let local = ClockSettings { local_rtc: true, ..settings("UTC", true, &[]) };
        assert!(validate(&local, &now, ok).is_err(), "never to local time");
        assert!(validate(&local, &ClockState { local_rtc: true, ..now.clone() }, ok).is_ok());
    }

    #[test]
    fn plans() {
        let now = ClockState {
            timezone: "UTC".into(),
            daemon: TimeDaemon::Timesyncd,
            sync_enabled: true,
            local_rtc: true,
            ..Default::default()
        };
        assert_eq!(
            plan(&settings("Europe/Rome", true, &["ntp.example.lan"]), &now),
            [Step::Zone("Europe/Rome".into()), Step::RtcUtc, Step::Servers, Step::Reload]
        );
        let now = ClockState { local_rtc: false, ..now };
        assert_eq!(plan(&settings("UTC", true, &[]), &now), [], "nothing changed");
        assert_eq!(plan(&settings("UTC", false, &[]), &now), [Step::SyncOff]);
        let none = ClockState { daemon: TimeDaemon::None, sync_enabled: false, ..now.clone() };
        assert_eq!(plan(&settings("UTC", true, &[]), &none), [Step::Install, Step::SyncOn]);
        assert_eq!(plan(&settings("UTC", false, &["a"]), &none), [], "no daemon to write for");
        assert_eq!(plan(&settings("UTC", true, &["a"]), &none), [Step::Servers, Step::Install, Step::SyncOn], "the drop-in is there when the package starts");
        let cmds: Vec<String> = [Step::SyncOn, Step::SyncOff, Step::Reload]
            .iter()
            .map(|s| step_cmd(s, TimeDaemon::Chrony).unwrap().display())
            .collect();
        assert!(cmds[0].contains("systemctl enable --now chrony.service"), "{cmds:?}");
        assert!(cmds[2].contains("chronyc reload sources"), "{cmds:?}");
        assert!(step_cmd(&Step::SyncOn, TimeDaemon::Timesyncd).unwrap().display().contains("timedatectl set-ntp true"));
        assert!(step_cmd(&Step::Zone("Asia/Tokyo".into()), TimeDaemon::Timesyncd).unwrap().display().contains("set-timezone Asia/Tokyo"));
    }

    #[test]
    fn manual_time() {
        let a = |e: Option<u64>, l: Option<&str>| set_time_arg(&SetClock { epoch_ms: e, local: l.map(str::to_string) });
        assert_eq!(a(Some(1_794_353_944_999), None).unwrap(), "@1794353944");
        assert_eq!(a(None, Some("2026-11-11 09:30:00")).unwrap(), "2026-11-11 09:30:00");
        assert!(a(None, Some("now; reboot")).is_err());
        assert!(a(Some(5), None).is_err(), "before 2000");
        assert!(a(None, Some("1970-01-01 00:00")).is_err());
        assert!(a(Some(1_794_353_944_999), Some("2026-11-11 09:30")).is_err());
        assert!(a(None, None).is_err());
    }

    #[test]
    fn state_with_timesyncd() {
        let d = tempfile::tempdir().unwrap();
        let (c, fake) = machine(
            d.path(),
            &[("timedatectl show", SHOW), ("busctl", BUS)],
            &[(TIMESYNCD_BIN, ""), (TIMESYNCD_DROPIN, &render_timesyncd(&["ntp.example.lan".into()]))],
        );
        let s = c.state();
        assert_eq!(s.daemon, TimeDaemon::Timesyncd);
        assert!(s.sync_enabled && s.synchronized && !s.local_rtc);
        assert_eq!(s.servers, ["ntp.example.lan"]);
        assert_eq!(s.default_servers, ["0.debian.pool.ntp.org", "1.debian.pool.ntp.org"]);
        assert_eq!(s.dhcp_servers, ["192.0.2.1"]);
        assert_eq!(s.source.unwrap().server, "2.debian.pool.ntp.org");
        let asked = fake.asked.lock().unwrap().clone();
        assert!(asked.iter().any(|l| l.contains("--auto-start=no")), "{asked:?}");
    }

    #[test]
    fn state_with_chrony() {
        let d = tempfile::tempdir().unwrap();
        let tracking = "C0000201,192.0.2.1,3,1794353944.1,0.0005,0,0,0,0,0,0,0,64,Normal\n";
        let (c, _) = machine(
            d.path(),
            &[("timedatectl show", "Timezone=UTC\nNTP=no\nNTPSynchronized=yes\n"), ("systemctl is-active", "active\n"), ("chronyc", tracking)],
            &[
                (CHRONYD_BIN, ""),
                (TIMESYNCD_BIN, ""),
                (CHRONY_CONF, "pool 2.debian.pool.ntp.org iburst\n"),
                (CHRONY_SOURCES, &render_chrony(&["ntp.example.lan".into()])),
                ("/etc/chrony/sources.d/other.sources", "server other.example.lan\n"),
                ("/run/chrony-dhcp/eth0.sources", "server 192.0.2.1 iburst\n"),
            ],
        );
        let s = c.state();
        assert_eq!(s.daemon, TimeDaemon::Chrony, "chrony wins when both are there");
        assert!(s.sync_enabled, "the unit runs, whatever timedated thinks");
        assert_eq!(s.servers, ["ntp.example.lan"]);
        assert_eq!(s.default_servers, ["2.debian.pool.ntp.org", "other.example.lan"]);
        assert_eq!(s.dhcp_servers, ["192.0.2.1"]);
        assert_eq!(s.source.unwrap().offset_us, Some(500));
    }

    #[test]
    fn no_daemon_and_sync_off_ask_nothing_more() {
        let d = tempfile::tempdir().unwrap();
        let (c, fake) = machine(d.path(), &[("timedatectl show", "Timezone=UTC\nNTP=no\n")], &[(TIMESYNCD_BIN, "")]);
        let s = c.state();
        assert!(!s.sync_enabled && s.source.is_none());
        assert_eq!(fake.asked.lock().unwrap().len(), 1, "a stopped timesyncd is never woken");
        std::fs::remove_file(under(d.path(), TIMESYNCD_BIN)).unwrap();
        assert_eq!(c.state().daemon, TimeDaemon::None);
    }

    #[test]
    fn servers_files() {
        let d = tempfile::tempdir().unwrap();
        let (c, _) = machine(d.path(), &[], &[]);
        c.write_servers(TimeDaemon::Timesyncd, &["a.example".into()]).unwrap();
        let p = under(d.path(), TIMESYNCD_DROPIN);
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o644);
        c.write_servers(TimeDaemon::Timesyncd, &[]).unwrap();
        assert!(!p.exists(), "empty: back to the distribution's servers");
        c.write_servers(TimeDaemon::Timesyncd, &[]).unwrap();
        c.write_servers(TimeDaemon::Chrony, &["b.example".into()]).unwrap();
        assert!(under(d.path(), CHRONY_SOURCES).exists());
    }

    #[test]
    fn zones_of_the_system() {
        // the host's zone files, read only; skipped where there are none
        let host = Path::new("/usr/share/zoneinfo/Europe/Rome");
        let Ok(rome) = std::fs::read(host) else { return };
        let d = tempfile::tempdir().unwrap();
        let (c, _) = machine(d.path(), &[("timedatectl show", "Timezone=Europe/Rome\n")], &[(TZDATA, "Z Europe/Rome 1\nL Europe/Rome Europe/Vatican\n")]);
        for n in ["Europe/Rome", "Europe/Vatican"] {
            let f = under(d.path(), &format!("{ZONEINFO}/{n}"));
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, &rome).unwrap();
        }
        let z = c.zones();
        assert_eq!(z.iter().map(|z| z.name.as_str()).collect::<Vec<_>>(), ["Europe/Rome", "Europe/Vatican"], "UTC has no file here");
        assert!(z[0].offset_s == 3600 || z[0].offset_s == 7200);
        assert!(c.zone_exists("Europe/Rome") && !c.zone_exists("Europe/Paris") && !c.zone_exists("../x"));
        assert_eq!(c.state().utc_offset_s, z[0].offset_s);
    }

    fn st(sync: bool, synced: bool, at: u64, offset: Option<i64>) -> ClockState {
        ClockState {
            now_ms: at,
            sync_enabled: sync,
            synchronized: synced,
            source: offset.map(|o| TimeSource { offset_us: Some(o), ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn unsynced_for_an_hour_is_notified_once() {
        let mut w = Watch::default();
        let m = 60_000;
        assert!(w.step(&st(true, false, 0, None)).is_empty());
        assert!(w.step(&st(true, false, 59 * m, None)).is_empty());
        let ev = w.step(&st(true, false, 61 * m, None));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].code, "system.clock_unsynced");
        assert_eq!(ev[0].fields["minutes"], "61");
        assert!(w.step(&st(true, false, 120 * m, None)).is_empty(), "once");
        let ev = w.step(&st(true, true, 125 * m, None));
        assert_eq!(ev[0].code, "system.clock_synced");
        assert!(w.step(&st(false, false, 300 * m, None)).is_empty(), "sync off: the administrator's choice");
    }

    #[test]
    fn drift_needs_two_looks() {
        let mut w = Watch::default();
        assert!(w.step(&st(true, true, 0, Some(1_500_000))).is_empty());
        let ev = w.step(&st(true, true, 1, Some(-1_500_000)));
        assert_eq!(ev[0].code, "system.clock_drift");
        assert_eq!(ev[0].fields["offset"], "-1.5 s");
        assert!(w.step(&st(true, true, 2, Some(700_000))).is_empty(), "between half and the limit: still open");
        assert_eq!(w.step(&st(true, true, 3, Some(1_000))).first().map(|e| e.code.as_str()), Some("system.clock_in_step"));
    }
}
