//! Date, time and time zone of the system (`/v1/system/time`, administrators only; D-TIME,
//! `docs/phase2/date-time.md`).
//!
//! The system holds the state, nothing is kept in rynas's configuration:
//! - the zone and the hardware clock go through timedated (`timedatectl`), which writes
//!   `/etc/localtime` and `/etc/adjtime` itself: rynasd's sandbox (ProtectClock) needs no change;
//! - the time daemon is detected: chrony when installed, else another `ntpd` (ntpsec, OpenNTPD:
//!   shown read-only, managed outside rynas), else `systemd-timesyncd`; with none, turning sync
//!   on installs `systemd-timesyncd` (never over another daemon, and apt may remove nothing);
//! - rynas's servers are one drop-in of that daemon (`timesyncd.conf.d/60-rynas.conf`, or
//!   `/etc/chrony/sources.d/rynas.sources`); without it the distribution's servers and those of
//!   DHCP are used. chrony keeps the sources of `chrony.conf` besides rynas's (said in the page);
//! - snapshot names, quiet hours and the power schedule read the zone through `rynas_tz`, which
//!   reads `/etc/localtime` again when it changes, whoever changed it. systemd recomputes its
//!   timers by itself, `systemd-analyze` and every child started later read the new zone.
//!
//! `--dry-system`: nothing is run; the zone, sync and hardware clock live in a small file below
//! the dry root.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
use rynas_notify::event::{code, field, firing, quietly, resolved};
use rynas_tasks::{Cmd, TaskCtx};
use serde::{Deserialize, Serialize};

use crate::ops::{Error, Ops, Res, user};

const TIMESYNCD_BIN: &str = "/usr/lib/systemd/systemd-timesyncd";
const CHRONYD_BIN: &str = "/usr/sbin/chronyd";
const TIMESYNCD_UNIT: &str = "systemd-timesyncd.service";
const CHRONY_UNIT: &str = "chrony.service";
const TIMESYNCD_CONF: &str = "/etc/systemd/timesyncd.conf";
const TIMESYNCD_DROPIN: &str = "/etc/systemd/timesyncd.conf.d/60-rynas.conf";
/// Runtime drop-ins: Debian's dhclient hook writes `01-dhclient.conf` here with `NTP=`.
const TIMESYNCD_RUN_DIR: &str = "/run/systemd/timesyncd.conf.d";
/// ntpsec and OpenNTPD (both `/usr/sbin/ntpd` on Debian): another `time-daemon`.
const NTPD_BIN: &str = "/usr/sbin/ntpd";
const NTPD_UNITS: [&str; 3] = ["ntpsec.service", "openntpd.service", "ntp.service"];
const CHRONY_CONF: &str = "/etc/chrony/chrony.conf";
const CHRONY_CONF_DIR: &str = "/etc/chrony/conf.d";
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

/// chrony's own verdict, the leap status of `chronyc -c tracking` (last field): anything but
/// "Not synchronised". The kernel flag timedated reports is no use with chrony: chronyd clears
/// `STA_UNSYNC` only with `rtcsync` (sys_timex.c, "clear the UNSYNC flag only if rtcsync is
/// enabled").
pub fn tracking_synced(text: &str) -> bool {
    let Some(line) = text.lines().next() else { return false };
    let f: Vec<&str> = line.split(',').collect();
    f.len() >= 14 && f.last().is_some_and(|l| !l.trim().is_empty() && l.trim() != "Not synchronised")
}

/// The configuration reads `/etc/chrony/sources.d` (`sourcedir`), where rynas writes.
pub fn chrony_reads_sources<'a>(texts: impl IntoIterator<Item = &'a str>) -> bool {
    texts.into_iter().flat_map(str::lines).any(|l| {
        let mut w = l.split_whitespace();
        w.next() == Some("sourcedir") && w.next().is_some_and(|d| d.trim_end_matches('/') == CHRONY_SOURCES_DIR)
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

/// `zone_offered`: the zone is one of [`Clock::zones`] (or the current one).
pub fn validate(req: &ClockSettings, now: &ClockState, zone_offered: impl Fn(&str) -> bool) -> Res<()> {
    if !validate::time_zone(&req.timezone) || !zone_offered(&req.timezone) {
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
    if now.daemon == TimeDaemon::Other && (req.sync_enabled != now.sync_enabled || req.servers != now.servers) {
        return Err(Error::invalid("sync_enabled", "time sync is managed outside rynas on this system"));
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
    // another daemon: sync and servers are not rynas's (validation refuses changes)
    if now.daemon == TimeDaemon::Other {
        return v;
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
        // only when no time daemon is there; `--no-remove` still: systemd-timesyncd conflicts
        // with `time-daemon`, and -y would otherwise remove one rynas did not see
        Step::Install => crate::updates::apt(&["install", "--no-remove", "--", PACKAGE], APT_TIMEOUT),
        Step::Servers => return None,
        Step::SyncOn | Step::SyncOff | Step::Reload if daemon == TimeDaemon::Other => return None,
        Step::SyncOn if chrony => sys(&["enable", "--now", CHRONY_UNIT]),
        Step::SyncOn => td(&["set-ntp", "true"]),
        Step::SyncOff if chrony => sys(&["disable", "--now", CHRONY_UNIT]),
        Step::SyncOff => td(&["set-ntp", "false"]),
        Step::Reload if chrony => Cmd::new("chronyc").args(["reload", "sources"]).timeout(ACTION_TIMEOUT),
        Step::Reload => sys(&["restart", TIMESYNCD_UNIT]),
    })
}

/// `timedatectl set-time` argument of a manual setting: a real date and time from
/// [`MIN_YEAR`](rynas_api_types::clock::MIN_YEAR) to [`MAX_YEAR`](rynas_api_types::clock::MAX_YEAR),
/// the error on the field that was given.
pub fn set_time_arg(req: &SetClock) -> Res<String> {
    use rynas_api_types::clock::{MAX_YEAR, MIN_YEAR};
    let years = format!("a date from {MIN_YEAR} to {MAX_YEAR} is needed");
    match (req.epoch_ms, req.local.as_deref()) {
        (Some(ms), None) => {
            let year = i64::try_from(ms)
                .ok()
                .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
                .map(|t| i32::from(t.to_zoned(jiff::tz::TimeZone::UTC).year()));
            match year {
                Some(y) if (MIN_YEAR..=MAX_YEAR).contains(&y) => Ok(format!("@{}", ms / 1000)),
                _ => Err(Error::invalid("epoch_ms", years)),
            }
        }
        (None, Some(l)) => {
            if !rynas_api_types::clock::local_time_ok(l) {
                return Err(Error::invalid("local", "the date and time need the shape YYYY-MM-DD HH:MM[:SS]"));
            }
            let n = |r: std::ops::Range<usize>| l.get(r).and_then(|x| x.parse::<i16>().ok()).unwrap_or(-1);
            let sec = if l.len() == 19 { n(17..19) } else { 0 };
            let real = jiff::civil::DateTime::new(n(0..4), n(5..7) as i8, n(8..10) as i8, n(11..13) as i8, n(14..16) as i8, sec as i8, 0);
            match real {
                Ok(dt) if (MIN_YEAR..=MAX_YEAR).contains(&i32::from(dt.year())) => Ok(l.to_string()),
                Ok(_) => Err(Error::invalid("local", years)),
                Err(_) => Err(Error::invalid("local", format!("{l:?} is not a real date and time"))),
            }
        }
        _ => Err(Error::bad_request("give epoch_ms or local, not both")),
    }
}

/// Looks (2 s apart) a sync now waits for a fresh answer.
const SYNC_WAIT_LOOKS: u64 = 15;

/// A server answered after `before` (the last answer when sync now was asked).
pub fn answered_since(before: Option<u64>, s: &ClockState) -> bool {
    s.source.as_ref().and_then(|x| x.last_sync_ms).is_some_and(|t| before.is_none_or(|b| t > b))
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
    /// The offered zones, parsed, until the zone database changes.
    zones: Mutex<Option<Zones>>,
}

/// What identifies one version of the zone database: `tzdata.zi` and the zoneinfo folder.
type ZoneKey = Option<(u64, u64, i64, i64, u64, i64, i64)>;

fn zone_key(root: &Path) -> ZoneKey {
    let zi = std::fs::metadata(under(root, TZDATA)).ok()?;
    let dir = std::fs::metadata(under(root, ZONEINFO)).ok()?;
    Some((zi.dev(), zi.ino(), zi.mtime(), zi.mtime_nsec(), zi.size(), dir.mtime(), dir.mtime_nsec()))
}

/// The zones of [`parse_zones`] that have a TZif file, parsed once (about 600 files).
struct Zones {
    key: ZoneKey,
    list: Arc<Vec<(String, jiff::tz::TimeZone)>>,
}

fn q(program: &str, args: &[&str]) -> Cmd {
    Cmd::new(program).args(args.iter().copied()).timeout(QUERY_TIMEOUT)
}

impl Clock {
    pub fn new(root: PathBuf, dry: bool) -> Self {
        Clock { root, probe: Arc::new(HostProbe), dry, busy: AtomicBool::new(false), zones: Mutex::new(None) }
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
        let has = |p: &str| under(&self.root, p).exists();
        if self.dry {
            TimeDaemon::Timesyncd
        } else if has(CHRONYD_BIN) {
            TimeDaemon::Chrony
        } else if has(NTPD_BIN) {
            // before timesyncd: never manage (or install) over another daemon
            TimeDaemon::Other
        } else if has(TIMESYNCD_BIN) {
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

    fn enabled(&self, unit: &str) -> bool {
        self.probe.query(&q("systemctl", &["is-enabled", unit])).is_ok_and(|t| t.trim() == "enabled")
    }

    /// The text of a configuration file and of the `*.<ext>` files of a folder, sorted.
    fn texts(&self, file: Option<&str>, dir: &str, ext: &str, before: Option<&str>) -> Vec<String> {
        let mut out: Vec<String> = file.and_then(|f| self.read(f)).into_iter().collect();
        if let Ok(rd) = std::fs::read_dir(under(&self.root, dir)) {
            let mut files: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == ext))
                .filter(|p| before.is_none_or(|b| p.file_name().is_some_and(|n| n.to_string_lossy().as_ref() < b)))
                .collect();
            files.sort();
            out.extend(files.iter().filter_map(|p| std::fs::read_to_string(p).ok()));
        }
        out
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

    fn offset_of(&self, name: &str) -> Option<i32> {
        tzif_offset(name, &self.zone_file(name)?)
    }

    /// The offered zones (`tzdata.zi` in the IANA regions, with a TZif file), parsed once
    /// and kept until `tzdata.zi` or the zoneinfo folder changes (a tzdata upgrade).
    fn zone_list(&self) -> Arc<Vec<(String, jiff::tz::TimeZone)>> {
        let key = zone_key(self.zone_root());
        let mut st = self.zones.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(z) = st.as_ref().filter(|z| z.key == key) {
            return z.list.clone();
        }
        let zi = std::fs::read_to_string(under(self.zone_root(), TZDATA)).unwrap_or_default();
        let list: Vec<(String, jiff::tz::TimeZone)> = parse_zones(&zi)
            .into_iter()
            .filter_map(|n| {
                let tz = jiff::tz::TimeZone::tzif(&n, &self.zone_file(&n)?).ok()?;
                Some((n, tz))
            })
            .collect();
        let list = Arc::new(list);
        *st = Some(Zones { key, list: list.clone() });
        list
    }

    /// A zone an apply accepts: one of [`Clock::zones`] (never `right/…`, `posix/…`,
    /// `Factory` or legacy links), or the current one.
    pub fn zone_offered(&self, name: &str, current: &str) -> bool {
        let listed = self.zone_list().iter().any(|(n, _)| n == name);
        (listed || name == current) && self.zone_file(name).is_some()
    }

    /// `GET /system/time/zones`.
    pub fn zones(&self) -> Vec<ZoneInfo> {
        let now = jiff::Timestamp::now();
        let mut out: Vec<ZoneInfo> =
            self.zone_list().iter().map(|(n, tz)| ZoneInfo { name: n.clone(), offset_s: tz.to_offset(now).seconds() }).collect();
        let current = self.timedate().timezone;
        if !out.iter().any(|z| z.name == current)
            && let Some(offset_s) = self.offset_of(&current)
        {
            out.push(ZoneInfo { name: current, offset_s });
        }
        out
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
                // dhclient's hook (01-dhclient.conf): read before rynas's drop-in, whose reset
                // drops them while rynas has its own list
                s.dhcp_hook_servers = self
                    .texts(None, TIMESYNCD_RUN_DIR, "conf", Some("60-rynas.conf"))
                    .iter()
                    .flat_map(|t| timesyncd_list(t, "NTP"))
                    .collect();
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
                // "on" is the unit wanted: a failed chronyd stays on (and unsynchronized)
                s.sync_enabled = self.active(CHRONY_UNIT) || self.enabled(CHRONY_UNIT);
                s.servers = chrony_sources(&self.read(CHRONY_SOURCES).unwrap_or_default());
                s.default_servers = chrony_sources(&self.read(CHRONY_CONF).unwrap_or_default());
                s.default_servers.extend(self.dir_sources(CHRONY_SOURCES_DIR, Some("rynas.sources")));
                s.dhcp_servers = self.dir_sources(CHRONY_DHCP_DIR, None);
                let conf = self.texts(Some(CHRONY_CONF), CHRONY_CONF_DIR, "conf", None);
                s.servers_ignored = !chrony_reads_sources(conf.iter().map(String::as_str));
                s.synchronized = false;
                if s.sync_enabled
                    && let Ok(t) = self.probe.query(&q("chronyc", &["-c", "-n", "tracking"]))
                {
                    s.synchronized = tracking_synced(&t);
                    s.source = parse_tracking(&t);
                }
            }
            TimeDaemon::Other => {
                // read-only: whatever runs it, the kernel's flag says whether it keeps time
                s.sync_enabled = td.ntp || NTPD_UNITS.iter().any(|u| self.active(u));
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
                // rynasd's own readers (rynas_tz) see the new /etc/localtime by themselves
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
    /// up to 30 s for an answer newer than `before` (the last one when it was asked).
    async fn sync_now(self: Arc<Self>, ctx: &TaskCtx, daemon: TimeDaemon, before: Option<u64>) -> Result<(), String> {
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
        // done only when a server has answered after the request: the kernel's flag (and the
        // last answer) are usually still those of before
        for _ in 0..SYNC_WAIT_LOOKS {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if ctx.is_cancelled() {
                return Err("cancelled".into());
            }
            let c = self.clone();
            let s = tokio::task::spawn_blocking(move || c.state()).await.map_err(|e| e.to_string())?;
            if answered_since(before, &s) {
                ctx.log(&match s.source.as_ref().and_then(|x| x.offset_us) {
                    Some(o) => format!("the time server answered (offset {:.1} ms)", o as f64 / 1000.0),
                    None => "the time server answered".to_string(),
                });
                return Ok(());
            }
        }
        Err(format!("no time server answered within {} s", SYNC_WAIT_LOOKS * 2))
    }
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
    /// Looks over the limit, each on a new answer of the server.
    drift_seen: u32,
    drift_open: bool,
    /// The answer of the last look counted (a server asked every 34 min gives the same
    /// offset to several looks: one measurement, not several).
    drift_answer: Option<u64>,
}

fn seconds(us: i64) -> String {
    format!("{:+.1} s", us as f64 / 1e6)
}

impl Watch {
    pub fn step(&mut self, s: &ClockState) -> Vec<NewEvent> {
        let mut out = Vec::new();
        if !s.sync_enabled {
            // the administrator's choice: no alarm, and what was open closes without a message
            // ("synchronized again" would be false)
            self.unsynced_since = None;
            self.drift_seen = 0;
            self.drift_answer = None;
            for (open, key) in [(&mut self.unsynced_open, KEY_UNSYNCED), (&mut self.drift_open, KEY_DRIFT)] {
                if std::mem::take(open) {
                    let ev = resolved(NotifyCategory::System, SRC, key, "Automatic time sync was turned off");
                    out.push(quietly(code(ev, "system.clock_sync_off")));
                }
            }
            return out;
        }
        if !s.synchronized {
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
        let src = s.source.as_ref();
        let answer = src.and_then(|x| x.last_sync_ms);
        match src.and_then(|x| x.offset_us) {
            Some(o) if o.abs() > DRIFT_US => {
                // a new measurement only: the same answer seen again counts once
                if answer.is_none() || answer != self.drift_answer {
                    self.drift_seen += 1;
                    self.drift_answer = answer;
                }
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
            off => {
                // below the limit: the next alarm needs two new looks in a row again
                self.drift_seen = 0;
                self.drift_answer = None;
                // an open alarm closes only well below the limit (or without a measurement)
                let back = off.is_none_or(|o| o.abs() <= DRIFT_US / 2);
                if back && std::mem::take(&mut self.drift_open) {
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
    // first: the state the request is checked and planned against stays true until it ends
    let guard = claim(&c)?;
    let now = current(&c).await?;
    let (c2, zone, cur) = (c.clone(), req.timezone.clone(), now.timezone.clone());
    let offered = tokio::task::spawn_blocking(move || c2.zone_offered(&zone, &cur)).await.map_err(Error::internal)?;
    validate(&req, &now, |_| offered)?;
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
    // before reading the state: an apply turning sync on cannot slip in between
    let guard = claim(&c)?;
    if current(&c).await?.sync_enabled {
        return Err(conflict("turn automatic sync off before setting the time by hand"));
    }
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
    let guard = claim(&c)?;
    let now = current(&c).await?;
    if now.daemon == TimeDaemon::Other {
        return Err(conflict("time sync is managed outside rynas on this system"));
    }
    if !now.sync_enabled {
        return Err(conflict("automatic sync is off"));
    }
    let before = now.source.as_ref().and_then(|x| x.last_sync_ms);
    tracing::info!(target: "rynas::audit", "time sync now by {who}");
    let id = a.ops.tasks.spawn("time.sync", &who, move |ctx| async move {
        let _guard = guard;
        c.sync_now(&ctx, now.daemon, before).await
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
        assert!(tracking_synced(t), "chrony's leap status, whatever the kernel flag says");
        assert!(!tracking_synced("00000000,,0,0.0,0,0,0,0,0,0,0,0,0,Not synchronised\n"));
        assert!(!tracking_synced("506 Cannot talk to daemon\n") && !tracking_synced(""));
        assert!(chrony_reads_sources(["pool x iburst\nsourcedir /etc/chrony/sources.d/\n"]));
        assert!(chrony_reads_sources(["# none here", "sourcedir /etc/chrony/sources.d"]), "or in conf.d");
        assert!(!chrony_reads_sources(["sourcedir /run/chrony-dhcp\n#sourcedir /etc/chrony/sources.d\n"]));
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
        // another daemon: the zone changes, sync and servers stay as they are
        let other = ClockState { daemon: TimeDaemon::Other, sync_enabled: true, ..now.clone() };
        assert!(validate(&settings("Europe/Rome", true, &[]), &other, ok).is_ok());
        assert!(validate(&settings("UTC", false, &[]), &other, ok).is_err());
        assert!(validate(&settings("UTC", true, &["ntp.example.lan"]), &other, ok).is_err());
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
        let install = step_cmd(&Step::Install, TimeDaemon::None).unwrap().display();
        assert!(install.contains("install --no-remove -- systemd-timesyncd"), "{install}");
        // another daemon: never installed over, never driven
        let other = ClockState { daemon: TimeDaemon::Other, sync_enabled: false, ..now.clone() };
        assert_eq!(plan(&settings("Asia/Tokyo", true, &["a"]), &other), [Step::Zone("Asia/Tokyo".into())]);
        assert!(step_cmd(&Step::SyncOn, TimeDaemon::Other).is_none());
    }

    #[test]
    fn manual_time() {
        let a = |e: Option<u64>, l: Option<&str>| set_time_arg(&SetClock { epoch_ms: e, local: l.map(str::to_string) });
        let field = |r: Res<String>| match r {
            Err(e) => e.3.unwrap_or_default(),
            Ok(v) => panic!("accepted {v}"),
        };
        assert_eq!(a(Some(1_794_353_944_999), None).unwrap(), "@1794353944");
        assert_eq!(a(None, Some("2026-11-11 09:30:00")).unwrap(), "2026-11-11 09:30:00");
        assert_eq!(a(None, Some("2099-12-31 23:59")).unwrap(), "2099-12-31 23:59");
        assert!(a(None, Some("now; reboot")).is_err());
        assert_eq!(field(a(Some(5), None)), "epoch_ms", "before 2000, blamed on the right field");
        assert_eq!(field(a(Some(4_102_444_800_000), None)), "epoch_ms", "2100");
        assert_eq!(field(a(Some(u64::MAX), None)), "epoch_ms");
        assert_eq!(field(a(None, Some("1970-01-01 00:00"))), "local");
        assert_eq!(field(a(None, Some("2100-01-01 00:00"))), "local");
        assert_eq!(field(a(None, Some("9999-01-01 00:00"))), "local");
        assert_eq!(field(a(None, Some("2026-02-30 10:00"))), "local", "not a real day");
        assert_eq!(field(a(None, Some("2026-11-11 24:00"))), "local");
        assert!(a(Some(1_794_353_944_999), Some("2026-11-11 09:30")).is_err());
        assert!(a(None, None).is_err());
    }

    #[test]
    fn state_with_timesyncd() {
        let d = tempfile::tempdir().unwrap();
        let (c, fake) = machine(
            d.path(),
            &[("timedatectl show", SHOW), ("busctl", BUS)],
            &[
                (TIMESYNCD_BIN, ""),
                (TIMESYNCD_DROPIN, &render_timesyncd(&["ntp.example.lan".into()])),
                // Debian's dhclient hook
                ("/run/systemd/timesyncd.conf.d/01-dhclient.conf", "[Time]\nNTP=192.0.2.5 192.0.2.6\n"),
                ("/run/systemd/timesyncd.conf.d/90-late.conf", "[Time]\nNTP=192.0.2.9\n"),
            ],
        );
        let s = c.state();
        assert_eq!(s.daemon, TimeDaemon::Timesyncd);
        assert!(s.sync_enabled && s.synchronized && !s.local_rtc);
        assert_eq!(s.servers, ["ntp.example.lan"]);
        assert_eq!(s.dhcp_hook_servers, ["192.0.2.5", "192.0.2.6"], "only the files rynas's reset drops");
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
            // no rtcsync: the kernel flag says unsynchronized while chrony is
            &[("timedatectl show", "Timezone=UTC\nNTP=no\nNTPSynchronized=no\n"), ("systemctl is-active", "active\n"), ("chronyc", tracking)],
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
        assert!(s.synchronized, "chrony's leap status");
        assert!(s.servers_ignored, "chrony.conf without the sources folder");
        std::fs::create_dir_all(under(d.path(), CHRONY_CONF_DIR)).unwrap();
        std::fs::write(under(d.path(), "/etc/chrony/conf.d/x.conf"), "sourcedir /etc/chrony/sources.d\n").unwrap();
        assert!(!c.state().servers_ignored);
    }

    #[test]
    fn chrony_not_synchronised_and_stopped() {
        let d = tempfile::tempdir().unwrap();
        let tracking = "00000000,,0,0.0,0,0,0,0,0,0,0,0,0,Not synchronised\n";
        let show = "Timezone=UTC\nNTPSynchronized=yes\n";
        let (c, _) = machine(d.path(), &[("timedatectl show", show), ("systemctl is-enabled", "enabled\n"), ("chronyc", tracking)], &[(CHRONYD_BIN, "")]);
        let s = c.state();
        assert!(s.sync_enabled, "enabled but not running: still on");
        assert!(!s.synchronized && s.source.is_none());
    }

    #[test]
    fn another_daemon_is_read_only() {
        let d = tempfile::tempdir().unwrap();
        let (c, fake) = machine(
            d.path(),
            &[("timedatectl show", "Timezone=UTC\nNTP=no\nNTPSynchronized=yes\n"), ("systemctl is-active ntpsec.service", "active\n")],
            &[(NTPD_BIN, ""), (TIMESYNCD_BIN, "")],
        );
        let s = c.state();
        assert_eq!(s.daemon, TimeDaemon::Other, "never timesyncd (or an install) over ntpd");
        assert!(s.sync_enabled && s.synchronized && s.source.is_none());
        assert!(!fake.asked.lock().unwrap().iter().any(|l| l.contains("busctl") || l.contains("chronyc")));
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
        let d = tempfile::tempdir().unwrap();
        let (c, _) = machine(
            d.path(),
            &[("timedatectl show", "Timezone=Etc/GMT-3\n")],
            &[(TZDATA, "Z Europe/Rome 1\nL Europe/Rome Europe/Vatican\nZ Factory 0\n")],
        );
        let zone = |n: &str, hours: i32| {
            let f = under(d.path(), &format!("{ZONEINFO}/{n}"));
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, tzif(hours)).unwrap();
        };
        for n in ["Europe/Rome", "Europe/Vatican", "right/Europe/Rome", "posix/Europe/Rome", "Factory"] {
            zone(n, 1);
        }
        zone("Etc/GMT-3", 3);
        let z = c.zones();
        let names = |z: &[ZoneInfo]| z.iter().map(|z| z.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&z), ["Europe/Rome", "Europe/Vatican", "Etc/GMT-3"], "UTC has no file here; the current zone is added");
        assert_eq!((z[0].offset_s, z[2].offset_s), (3600, 10800));
        assert!(c.zone_offered("Europe/Rome", "UTC"));
        assert!(c.zone_offered("Etc/GMT-3", "Etc/GMT-3"), "the current zone");
        for n in ["Etc/GMT-3", "right/Europe/Rome", "posix/Europe/Rome", "Factory", "Europe/Paris", "../x"] {
            assert!(!c.zone_offered(n, "UTC"), "{n}");
        }
        assert_eq!(c.state().utc_offset_s, 10800);
        // kept until the database changes
        std::fs::remove_file(under(d.path(), &format!("{ZONEINFO}/Europe/Vatican"))).unwrap();
        assert_eq!(c.zones().len(), 3, "cached");
        zone("Asia/Tokyo", 9);
        std::fs::write(under(d.path(), TZDATA), "Z Europe/Rome 1\nZ Asia/Tokyo 9\n# a longer file\n").unwrap();
        assert_eq!(names(&c.zones()), ["Asia/Tokyo", "Europe/Rome", "Etc/GMT-3"], "a new tzdata is read again");
    }

    /// A TZif v2 file of a fixed offset (hours east of UTC), no transitions.
    fn tzif(hours: i32) -> Vec<u8> {
        let header = |out: &mut Vec<u8>| {
            out.extend(b"TZif2");
            out.extend([0u8; 15]);
            for n in [0u32, 0, 0, 0, 1, 4] {
                out.extend(n.to_be_bytes());
            }
            out.extend((hours * 3600).to_be_bytes());
            out.extend([0u8, 0]);
            out.extend(b"XYZ\0");
        };
        let mut out = Vec::new();
        header(&mut out);
        header(&mut out);
        out.extend(format!("\nXYZ{}\n", -hours).as_bytes());
        out
    }

    fn st(sync: bool, synced: bool, at: u64, offset: Option<i64>) -> ClockState {
        ClockState {
            now_ms: at,
            sync_enabled: sync,
            synchronized: synced,
            // every look a new answer of the server, unless a test says otherwise
            source: offset.map(|o| TimeSource { offset_us: Some(o), last_sync_ms: Some(at), ..Default::default() }),
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
        assert!(!ev[0].quiet);
        for i in 0..20 {
            assert!(w.step(&st(false, false, (300 + i * 5) * m, None)).is_empty(), "sync off: the administrator's choice");
        }
    }

    #[test]
    fn turning_sync_off_closes_quietly() {
        let mut w = Watch::default();
        let m = 60_000;
        w.step(&st(true, false, 0, Some(2_000_000)));
        let ev = w.step(&st(true, false, 61 * m, Some(2_000_000)));
        let codes: Vec<&str> = ev.iter().map(|e| e.code.as_str()).collect();
        assert_eq!(codes, ["system.clock_unsynced", "system.clock_drift"]);
        let ev = w.step(&st(false, false, 62 * m, None));
        assert_eq!(ev.len(), 2);
        assert!(ev.iter().all(|e| e.quiet && e.code == "system.clock_sync_off" && e.state == rynas_api_types::notify::EventState::Resolved));
        assert!(w.step(&st(false, false, 63 * m, None)).is_empty(), "closed once");
        // on again: counting starts over, nothing "again" to say
        assert!(w.step(&st(true, true, 64 * m, Some(1_000))).is_empty());
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

    #[test]
    fn drift_counts_new_answers_in_a_row() {
        let mut w = Watch::default();
        // timesyncd asks every 34 min: the same answer seen by several looks is one measurement
        let same = |at: u64| ClockState {
            source: Some(TimeSource { offset_us: Some(1_500_000), last_sync_ms: Some(7), ..Default::default() }),
            ..st(true, true, at, None)
        };
        for at in 0..6 {
            assert!(w.step(&same(at)).is_empty(), "look {at}");
        }
        // over, between half and the limit, over: not two in a row
        let mut w = Watch::default();
        assert!(w.step(&st(true, true, 10, Some(1_500_000))).is_empty());
        assert!(w.step(&st(true, true, 11, Some(700_000))).is_empty());
        assert!(w.step(&st(true, true, 12, Some(1_500_000))).is_empty());
        assert_eq!(w.step(&st(true, true, 13, Some(1_500_000)))[0].code, "system.clock_drift");
    }

    #[test]
    fn sync_now_waits_for_a_new_answer() {
        let at = |t: Option<u64>| ClockState { source: Some(TimeSource { last_sync_ms: t, ..Default::default() }), ..Default::default() };
        assert!(!answered_since(Some(100), &at(Some(100))), "the answer of before");
        assert!(!answered_since(Some(100), &at(None)), "restarted, no answer yet");
        assert!(!answered_since(None, &ClockState::default()));
        assert!(answered_since(Some(100), &at(Some(2_100))));
        assert!(answered_since(None, &at(Some(5))));
    }
}
