//! Processes (D-PROCESSES, `docs/research/processes.md`): the list with CPU, memory and disk
//! I/O, grouped by service, container, session; details; terminate/kill, priority, restart
//! of a service. Administrators only, reads included (command lines can hold secrets).
//!
//! - `/proc` is read directly (no `ps`/`top`). A sample is taken only when a request asks and
//!   the last one is older than [`FRESH`]; rates are deltas against the previous sample of the
//!   same process (same PID **and** start time). The previous sample is dropped
//!   [`IDLE_DROP`] after the last request: nothing runs or stays in memory while nobody looks.
//! - Per sample: `/proc/stat`, `/proc/meminfo`, and per process `stat` and `io`. The command
//!   line, owner and cgroup are read once per process and again only when its name changes
//!   (an exec) or while it is younger than [`YOUNG_S`] (sshd and runc move their children).
//! - `/proc/<pid>/io` of another user's process needs `CAP_SYS_PTRACE`, which rynasd does not
//!   have: those rates are `None`. Signals and priorities need `CAP_KILL` and `CAP_SYS_NICE`.
//! - rynasd runs in a private cgroup namespace (`ProtectControlGroups=private`): other
//!   processes' cgroup paths start with `/..`, rynasd's own children have paths without it.
//! - Under `--dry-system` signals, priorities and restarts are logged, never done.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::processes::{
    GroupKind, IoClass, ListenPort, PriorityRequest, ProcSignal, ProcState, ProcessDetail, ProcessGroup,
    ProcessInfo, ProcessList, RestartRequest, SignalRequest,
};
use rynas_api_types::{TaskStarted, error_codes};
use rynas_tasks::Cmd;
use serde::Deserialize;

use crate::ops::{Error, Ops, Res, user};

/// A sample younger than this is answered again (several viewers share it).
pub const FRESH: Duration = Duration::from_millis(1000);
/// The previous sample is dropped this long after the last request.
pub const IDLE_DROP: Duration = Duration::from_secs(60);
/// Processes younger than this have their owner, command line and cgroup read every sample.
const YOUNG_S: u64 = 5;
/// Longest command line in the list (the detail has all of it).
const CMD_MAX: usize = 256;
/// Longest command line read at all.
const CMDLINE_MAX: usize = 16 << 10;
/// Owner names kept (cleared beyond).
const USERS_MAX: usize = 4096;
/// Longest wait for a sample (a `/proc` read can block on a process's memory lock).
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the container names are trusted before Docker is asked again.
const NAMES_TTL: Duration = Duration::from_secs(30);
const PF_KTHREAD: u64 = 0x0020_0000;

/// Units whose processes are never signalled, reniced or restarted from here.
pub const PROTECTED_UNITS: &[&str] = &[
    "rynasd.service",
    "rynas-web.service",
    "init.scope",
    "systemd-journald.service",
    "systemd-logind.service",
    "systemd-udevd.service",
    "systemd-networkd.service",
    "dbus.service",
    "dbus-broker.service",
];
/// The SSH daemon: its main process is protected (the way in), sessions are not; the
/// service is not restarted from here (the SSH page has its own safe apply).
const SSH_UNITS: &[&str] = &["ssh.service", "sshd.service"];

// ------------------------------------------------------------------ parsing

/// The fields of `/proc/<pid>/stat` rynas uses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stat {
    pub comm: String,
    pub state: char,
    pub ppid: u32,
    pub flags: u64,
    /// utime + stime, clock ticks.
    pub cpu_ticks: u64,
    pub nice: i32,
    pub threads: u32,
    /// Clock ticks after boot.
    pub start_ticks: u64,
    pub rss_pages: u64,
}

/// `/proc/<pid>/stat`; the name may hold spaces and parentheses (the last `)` ends it).
pub fn parse_stat(text: &str) -> Option<Stat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_string();
    let f: Vec<&str> = text.get(close + 1..)?.split_ascii_whitespace().collect();
    let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
    Some(Stat {
        comm,
        state: f.first()?.chars().next()?,
        ppid: n(1)? as u32,
        flags: n(6)?,
        cpu_ticks: n(11)? + n(12)?,
        nice: f.get(16)?.parse().ok()?,
        threads: n(17)? as u32,
        start_ticks: n(19)?,
        rss_pages: n(21)?,
    })
}

/// `read_bytes` and `write_bytes` of `/proc/<pid>/io` (what reached the block layer).
pub fn parse_io(text: &str) -> Option<(u64, u64)> {
    let (mut r, mut w) = (None, None);
    for l in text.lines() {
        if let Some(v) = l.strip_prefix("read_bytes:") {
            r = v.trim().parse().ok();
        } else if let Some(v) = l.strip_prefix("write_bytes:") {
            w = v.trim().parse().ok();
        }
    }
    Some((r?, w?))
}

fn state_of(c: char) -> ProcState {
    match c {
        'R' => ProcState::Running,
        'S' => ProcState::Sleeping,
        'D' => ProcState::DiskWait,
        'T' | 't' => ProcState::Stopped,
        'Z' | 'X' => ProcState::Zombie,
        'I' => ProcState::Idle,
        _ => ProcState::Other,
    }
}

/// The cgroup v2 path of `/proc/<pid>/cgroup` (`0::/path`).
pub fn cgroup_path(text: &str) -> Option<&str> {
    text.lines().find_map(|l| l.strip_prefix("0::")).map(str::trim)
}

fn is_hex_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What a cgroup path belongs to: kind and key (unit name, container id). `ns_private`:
/// rynasd's own cgroup namespace hides the host tree, so a path without `/..` is rynasd's.
pub fn classify(path: &str, ns_private: bool) -> (GroupKind, String) {
    if ns_private && !path.starts_with("/..") {
        return (GroupKind::Service, "rynasd.service".into());
    }
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty() && *c != "..").collect();
    for (i, c) in comps.iter().enumerate() {
        if let Some(id) = c.strip_prefix("docker-").and_then(|r| r.strip_suffix(".scope")).filter(|id| is_hex_id(id)) {
            return (GroupKind::Container, id.into());
        }
        if *c == "docker"
            && let Some(id) = comps.get(i + 1).filter(|id| is_hex_id(id))
        {
            return (GroupKind::Container, (*id).into());
        }
    }
    if let Some(c) = comps.iter().find(|c| c.starts_with("user@") && c.ends_with(".service")) {
        return (GroupKind::User, (*c).into());
    }
    if let Some(c) = comps.iter().find(|c| c.starts_with("session-") && c.ends_with(".scope")) {
        return (GroupKind::Session, c.trim_end_matches(".scope").into());
    }
    if let Some(c) = comps.iter().rev().find(|c| c.ends_with(".service")) {
        return (GroupKind::Service, (*c).into());
    }
    match comps.last() {
        Some(c) => (GroupKind::System, (*c).into()),
        None => (GroupKind::System, "system".into()),
    }
}

/// The host's cgroup path: `/..` steps resolved against rynasd's own place
/// (`/system.slice/rynasd.service`) when it runs in its namespace.
fn host_cgroup(path: &str, ns_private: bool) -> String {
    if !ns_private {
        return path.into();
    }
    let mut out: Vec<&str> = vec!["system.slice", "rynasd.service"];
    for c in path.split('/').filter(|c| !c.is_empty()) {
        if c == ".." {
            out.pop();
        } else {
            out.push(c);
        }
    }
    format!("/{}", out.join("/"))
}

/// The listening sockets of `/proc/<pid>/net/{tcp,udp}{,6}` whose inode is one of `inodes`.
/// TCP in state LISTEN (`0A`), UDP unconnected (`07`).
pub fn parse_listen(text: &str, proto: &str, v6: bool, inodes: &HashSet<u64>) -> Vec<ListenPort> {
    let want = if proto == "tcp" { "0A" } else { "07" };
    let mut out = Vec::new();
    for l in text.lines().skip(1) {
        let f: Vec<&str> = l.split_ascii_whitespace().collect();
        let (Some(local), Some(st), Some(inode)) = (f.get(1), f.get(3), f.get(9)) else { continue };
        if *st != want || !inode.parse::<u64>().is_ok_and(|i| inodes.contains(&i)) {
            continue;
        }
        let Some((addr, port)) = local.split_once(':') else { continue };
        let Ok(port) = u16::from_str_radix(port, 16) else { continue };
        let Some(address) = hex_addr(addr, v6) else { continue };
        let p = ListenPort { protocol: proto.into(), address, port };
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// An address of `/proc/net/*`: network-order bytes printed as 32-bit words in host order.
fn hex_addr(h: &str, v6: bool) -> Option<String> {
    let word = |i: usize| u32::from_str_radix(h.get(i * 8..i * 8 + 8)?, 16).ok().map(u32::to_ne_bytes);
    if v6 {
        if h.len() != 32 {
            return None;
        }
        let mut b = [0u8; 16];
        for i in 0..4 {
            b[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
        }
        Some(std::net::Ipv6Addr::from(b).to_string())
    } else if h.len() == 8 {
        Some(std::net::Ipv4Addr::from(word(0)?).to_string())
    } else {
        None
    }
}

/// The `cpu` line of `/proc/stat`: (busy, total) ticks; `btime` in seconds.
pub fn parse_proc_stat(text: &str) -> (Option<(u64, u64)>, Option<u64>) {
    let mut cpu = None;
    let mut btime = None;
    for l in text.lines() {
        if let Some(rest) = l.strip_prefix("cpu ") {
            let v: Vec<u64> = rest.split_ascii_whitespace().filter_map(|x| x.parse().ok()).collect();
            // user nice system idle iowait irq softirq steal (guest is inside user)
            let total: u64 = v.iter().take(8).sum();
            let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
            cpu = Some((total.saturating_sub(idle), total));
        } else if let Some(rest) = l.strip_prefix("btime ") {
            btime = rest.trim().parse().ok();
        }
    }
    (cpu, btime)
}

fn meminfo_total(text: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.split_ascii_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

/// A field of `smaps_rollup` or `status` in kB, as bytes.
fn kb_field(text: &str, name: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix(name))
        .and_then(|v| v.split_ascii_whitespace().next()?.parse::<u64>().ok())
        .map(|kb| kb * 1024)
}

fn cmdline_text(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    s.trim_end_matches('\0').replace('\0', " ")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.into();
    }
    let mut t: String = s.chars().take(max - 1).collect();
    t.push('…');
    t
}

// ------------------------------------------------------------------ sampling

/// What is kept of a process between samples.
#[derive(Debug, Clone)]
struct Meta {
    comm: String,
    cmd: String,
    uid: u32,
    group: (GroupKind, String),
    kernel: bool,
    /// `io` was refused once: not asked again for this process.
    io_denied: bool,
}

#[derive(Debug, Clone)]
struct Prev {
    start_ticks: u64,
    cpu_ticks: u64,
    io: Option<(u64, u64)>,
    meta: Arc<Meta>,
}

struct Sample {
    at: Instant,
    cpu: Option<(u64, u64)>,
    procs: HashMap<u32, Prev>,
    list: Arc<ProcessList>,
}

/// Docker's names for container ids, and the catalogue's names for stacks.
#[derive(Default)]
struct Names {
    at: Option<Instant>,
    containers: HashMap<String, (String, Option<String>)>,
    apps: HashMap<String, String>,
}

/// The process view.
pub struct Processes {
    proc_root: PathBuf,
    dry: bool,
    hz: u64,
    page: u64,
    own_pid: u32,
    ns_private: bool,
    last: Mutex<Option<Sample>>,
    last_request: Mutex<Instant>,
    users: Mutex<HashMap<u32, String>>,
    names: Mutex<Names>,
    /// One sample at a time; the others wait and share it.
    flight: tokio::sync::Mutex<()>,
    pub docker: crate::docker::DkState,
    pub apps: crate::apps::AppsState,
}

impl Processes {
    pub fn new(dry: bool, docker: crate::docker::DkState, apps: crate::apps::AppsState) -> Arc<Self> {
        let own = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        let path = cgroup_path(&own).unwrap_or("/");
        Arc::new(Self::with_root(PathBuf::from("/proc"), dry, std::process::id(), ns_private_of(path), docker, apps))
    }

    pub fn with_root(
        proc_root: PathBuf,
        dry: bool,
        own_pid: u32,
        ns_private: bool,
        docker: crate::docker::DkState,
        apps: crate::apps::AppsState,
    ) -> Self {
        // SAFETY: sysconf only reads configuration values
        let (hz, page) = unsafe { (libc::sysconf(libc::_SC_CLK_TCK), libc::sysconf(libc::_SC_PAGESIZE)) };
        Processes {
            proc_root,
            dry,
            hz: u64::try_from(hz).ok().filter(|h| *h > 0).unwrap_or(100),
            page: u64::try_from(page).ok().filter(|p| *p > 0).unwrap_or(4096),
            own_pid,
            ns_private,
            last: Mutex::new(None),
            last_request: Mutex::new(Instant::now()),
            users: Mutex::new(HashMap::new()),
            names: Mutex::new(Names::default()),
            flight: tokio::sync::Mutex::new(()),
            docker,
            apps,
        }
    }

    fn p(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.proc_root.join(rel)
    }

    fn user_name(&self, uid: u32) -> String {
        let mut u = self.users.lock().unwrap();
        if let Some(n) = u.get(&uid) {
            return n.clone();
        }
        if u.len() >= USERS_MAX {
            u.clear();
        }
        let n = nix::unistd::User::from_uid(uid.into())
            .ok()
            .flatten()
            .map_or_else(|| uid.to_string(), |x| x.name);
        u.insert(uid, n.clone());
        n
    }

    fn read_meta(&self, pid: u32, st: &Stat, old: Option<&Meta>) -> Meta {
        let dir = self.p(pid.to_string());
        let kernel = st.flags & PF_KTHREAD != 0 || pid == 2 || st.ppid == 2;
        let uid = std::fs::metadata(&dir).map(|m| m.uid()).unwrap_or(0);
        let cmd = if kernel {
            String::new()
        } else {
            read_capped(&dir.join("cmdline"), CMDLINE_MAX).map(|b| cmdline_text(&b)).unwrap_or_default()
        };
        let group = if kernel {
            (GroupKind::Kernel, "kernel".into())
        } else {
            std::fs::read_to_string(dir.join("cgroup"))
                .ok()
                .and_then(|t| cgroup_path(&t).map(|p| classify(p, self.ns_private)))
                .unwrap_or((GroupKind::System, "system".into()))
        };
        Meta { comm: st.comm.clone(), cmd, uid, group, kernel, io_denied: old.is_some_and(|o| o.io_denied && o.uid == uid) }
    }

    /// One look at `/proc`: every process with rates against `prev`.
    fn take(&self, prev: Option<&Sample>, now_ms: u64) -> Sample {
        let at = Instant::now();
        let (cpu, btime) = parse_proc_stat(&std::fs::read_to_string(self.p("stat")).unwrap_or_default());
        let mem_total = meminfo_total(&std::fs::read_to_string(self.p("meminfo")).unwrap_or_default());
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
        let elapsed = prev.map(|p| at.duration_since(p.at).as_secs_f64()).filter(|e| *e > 0.05);
        let uptime_ticks = btime.map(|b| (now_ms / 1000).saturating_sub(b) * self.hz);
        let mut procs = HashMap::new();
        let mut groups: Vec<ProcessGroup> = Vec::new();
        let mut group_ix: HashMap<(GroupKind, String), u32> = HashMap::new();
        let mut out = Vec::new();
        for e in std::fs::read_dir(&self.proc_root).into_iter().flatten().flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            let dir = e.path();
            let Some(st) = std::fs::read_to_string(dir.join("stat")).ok().and_then(|t| parse_stat(&t)) else { continue };
            let old = prev.and_then(|p| p.procs.get(&pid)).filter(|o| o.start_ticks == st.start_ticks);
            let young = uptime_ticks.is_some_and(|u| u.saturating_sub(st.start_ticks) < YOUNG_S * self.hz);
            let meta = match old {
                Some(o) if o.meta.comm == st.comm && !young => o.meta.clone(),
                o => Arc::new(self.read_meta(pid, &st, o.map(|o| &*o.meta))),
            };
            let io = if meta.kernel || meta.io_denied {
                None
            } else {
                match std::fs::read_to_string(dir.join("io")) {
                    Ok(t) => parse_io(&t),
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        let mut m = (*meta).clone();
                        m.io_denied = true;
                        procs.insert(pid, Prev { start_ticks: st.start_ticks, cpu_ticks: st.cpu_ticks, io: None, meta: Arc::new(m) });
                        None
                    }
                    Err(_) => None,
                }
            };
            let meta = procs.get(&pid).map_or(meta, |p: &Prev| p.meta.clone());
            let (cpu_pct, rates) = match (old, elapsed) {
                (Some(o), Some(el)) => {
                    let pct = st.cpu_ticks.saturating_sub(o.cpu_ticks) as f64 / (el * self.hz as f64) * 100.0;
                    let rate = |a: u64, b: u64| (a.saturating_sub(b) as f64 / el).round() as u64;
                    let rates = match (io, o.io) {
                        (Some((r, w)), Some((r0, w0))) => Some((rate(r, r0), rate(w, w0))),
                        _ => None,
                    };
                    (Some(pct as f32), rates)
                }
                _ => (None, None),
            };
            let key = meta.group.clone();
            let gi = *group_ix.entry(key.clone()).or_insert_with(|| {
                groups.push(self.group_of(&key, meta.uid));
                (groups.len() - 1) as u32
            });
            let protected = self.is_protected(pid, st.ppid, &meta);
            out.push(ProcessInfo {
                pid,
                ppid: st.ppid,
                start_ms: btime.map_or(0, |b| b * 1000 + st.start_ticks * 1000 / self.hz),
                name: st.comm.clone(),
                cmd: truncate(&meta.cmd, CMD_MAX),
                user: self.user_name(meta.uid),
                state: state_of(st.state),
                threads: st.threads,
                nice: st.nice,
                cpu_pct,
                rss_bytes: st.rss_pages * self.page,
                read_bytes_s: rates.map(|r| r.0),
                write_bytes_s: rates.map(|r| r.1),
                group: gi,
                protected,
            });
            procs.insert(pid, Prev { start_ticks: st.start_ticks, cpu_ticks: st.cpu_ticks, io, meta });
        }
        let sys_pct = match (cpu, prev.and_then(|p| p.cpu)) {
            (Some((b, t)), Some((b0, t0))) if t > t0 => Some((b.saturating_sub(b0) as f64 / (t - t0) as f64 * 100.0) as f32),
            _ => None,
        };
        out.sort_by_key(|p| p.pid);
        let list = ProcessList { read_at_ms: now_ms, cpus, mem_total_bytes: mem_total, cpu_pct: sys_pct, groups, processes: out, dry: self.dry };
        Sample { at, cpu, procs, list: Arc::new(list) }
    }

    fn group_of(&self, key: &(GroupKind, String), uid: u32) -> ProcessGroup {
        let (kind, id) = (key.0, key.1.clone());
        let name = match kind {
            GroupKind::Service => id.trim_end_matches(".service").to_string(),
            GroupKind::Container => id.chars().take(12).collect(),
            GroupKind::Session => self.user_name(uid),
            GroupKind::User => id
                .trim_start_matches("user@")
                .trim_end_matches(".service")
                .parse::<u32>()
                .map_or_else(|_| id.clone(), |u| self.user_name(u)),
            GroupKind::Kernel => "kernel".into(),
            GroupKind::System => id.trim_end_matches(".scope").trim_end_matches(".slice").to_string(),
        };
        let restartable = match kind {
            GroupKind::Service => !PROTECTED_UNITS.contains(&id.as_str()) && !SSH_UNITS.contains(&id.as_str()),
            GroupKind::Container => self.docker.is_some(),
            _ => false,
        };
        ProcessGroup { kind, id, name, app: None, stack: None, restartable }
    }

    fn is_protected(&self, pid: u32, ppid: u32, meta: &Meta) -> bool {
        let unit = meta.group.1.as_str();
        pid <= 1
            || pid == self.own_pid
            || meta.kernel
            || (meta.group.0 == GroupKind::Service && PROTECTED_UNITS.contains(&unit))
            || (SSH_UNITS.contains(&unit) && ppid == 1)
            || unit == "init.scope"
    }

    /// The list, from a sample at most [`FRESH`] old (taken now if needed).
    pub async fn list(self: &Arc<Self>) -> Res<Arc<ProcessList>> {
        *self.last_request.lock().unwrap() = Instant::now();
        let _flight = self.flight.lock().await;
        let fresh = self.last.lock().unwrap().as_ref().filter(|s| s.at.elapsed() < FRESH).map(|s| s.list.clone());
        if let Some(l) = fresh {
            return Ok(l);
        }
        let me = self.clone();
        let job = tokio::task::spawn_blocking(move || {
            let prev = me.last.lock().unwrap().take();
            let s = me.take(prev.as_ref(), rynas_tasks::now_ms());
            let list = s.list.clone();
            *me.last.lock().unwrap() = Some(s);
            list
        });
        let list = tokio::time::timeout(SAMPLE_TIMEOUT, job)
            .await
            .map_err(|_| Error::new(StatusCode::GATEWAY_TIMEOUT, error_codes::TIMEOUT, "reading /proc took too long".into()))?
            .map_err(Error::internal)?;
        let named = Arc::new(self.with_names(&list).await);
        if let Some(s) = self.last.lock().unwrap().as_mut() {
            s.list = named.clone();
        }
        Ok(named)
    }

    /// Container groups named from Docker and the catalogue (asked again only when a new
    /// container shows up, at most every [`NAMES_TTL`]).
    async fn with_names(&self, list: &ProcessList) -> ProcessList {
        let ids: Vec<&str> = list.groups.iter().filter(|g| g.kind == GroupKind::Container).map(|g| g.id.as_str()).collect();
        let mut out = list.clone();
        if ids.is_empty() {
            return out;
        }
        let stale = {
            let n = self.names.lock().unwrap();
            let missing = ids.iter().any(|id| !n.containers.contains_key(*id));
            n.at.is_none_or(|t| missing && t.elapsed() > NAMES_TTL)
        };
        if stale && let Some(d) = &self.docker {
            let mut containers = HashMap::new();
            if let Ok(cs) = d.containers().await {
                for s in cs {
                    containers.insert(s.c.id.clone(), (s.c.name.clone(), s.c.stack.clone()));
                }
            }
            let mut apps = HashMap::new();
            if let Some(a) = &self.apps {
                for i in a.installed().await {
                    apps.insert(i.stack, i.name);
                }
            }
            *self.names.lock().unwrap() = Names { at: Some(Instant::now()), containers, apps };
        }
        let n = self.names.lock().unwrap();
        for g in out.groups.iter_mut().filter(|g| g.kind == GroupKind::Container) {
            if let Some((name, stack)) = n.containers.get(&g.id) {
                g.name = name.clone();
                g.stack = stack.clone();
                g.app = stack.as_ref().and_then(|s| n.apps.get(s)).cloned();
            }
        }
        out
    }

    /// Drops the previous sample when nobody has asked for [`IDLE_DROP`].
    pub fn trim_idle(&self) -> bool {
        if self.last_request.lock().unwrap().elapsed() < IDLE_DROP {
            return false;
        }
        let dropped = self.last.lock().unwrap().take().is_some();
        if dropped {
            *self.names.lock().unwrap() = Names::default();
            self.users.lock().unwrap().clear();
        }
        dropped
    }

    /// How far a start time may move between two reads: `btime` of `/proc/stat` is derived
    /// from the wall clock and can step by a second, plus one tick of rounding.
    fn slack_ms(&self) -> u64 {
        1000 + 1000 / self.hz + 1
    }

    /// The process `pid` if it still is the one that started at `start_ms` (within
    /// [`Self::slack_ms`]): its stat and meta.
    fn identify(&self, pid: u32, start_ms: u64) -> Res<(Stat, Meta)> {
        let gone = || Error::not_found("no such process (it ended, or the number now belongs to another one)");
        let st = std::fs::read_to_string(self.p(format!("{pid}/stat"))).ok().and_then(|t| parse_stat(&t)).ok_or_else(gone)?;
        let (_, btime) = parse_proc_stat(&std::fs::read_to_string(self.p("stat")).unwrap_or_default());
        let started = btime.map_or(0, |b| b * 1000 + st.start_ticks * 1000 / self.hz);
        if started.abs_diff(start_ms) > self.slack_ms() {
            return Err(gone());
        }
        let meta = self.read_meta(pid, &st, None);
        Ok((st, meta))
    }

    fn refuse_protected(&self, pid: u32, st: &Stat, meta: &Meta) -> Res<()> {
        if self.is_protected(pid, st.ppid, meta) {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                error_codes::PROCESS_PROTECTED,
                format!("process {pid} ({}) is needed by the system or rynas", st.comm),
            ));
        }
        Ok(())
    }

    /// Everything about one process.
    pub fn detail(&self, pid: u32, start_ms: u64) -> Res<ProcessDetail> {
        let (st, meta) = self.identify(pid, start_ms)?;
        let dir = self.p(pid.to_string());
        let mut info = self
            .last
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|s| s.list.processes.iter().find(|p| p.pid == pid && p.start_ms.abs_diff(start_ms) <= self.slack_ms()).cloned())
            .unwrap_or_default();
        let mut group = self.group_of(&meta.group, meta.uid);
        if let Some(g) = self.last.lock().unwrap().as_ref().and_then(|s| s.list.groups.get(info.group as usize)).filter(|g| g.id == group.id) {
            group = g.clone();
        }
        info.pid = pid;
        info.ppid = st.ppid;
        info.start_ms = start_ms;
        info.name = st.comm.clone();
        info.cmd = truncate(&meta.cmd, CMD_MAX);
        info.user = self.user_name(meta.uid);
        info.state = state_of(st.state);
        info.threads = st.threads;
        info.nice = st.nice;
        info.rss_bytes = st.rss_pages * self.page;
        info.protected = self.is_protected(pid, st.ppid, &meta);
        let rollup = std::fs::read_to_string(dir.join("smaps_rollup")).ok();
        let io = std::fs::read_to_string(dir.join("io")).ok().and_then(|t| parse_io(&t));
        let cgroup = std::fs::read_to_string(dir.join("cgroup"))
            .ok()
            .and_then(|t| cgroup_path(&t).map(|p| host_cgroup(p, self.ns_private)));
        let (io_class, io_level) = ioprio(pid);
        Ok(ProcessDetail {
            info,
            group,
            cmdline: meta.cmd.clone(),
            exe: std::fs::read_link(dir.join("exe")).ok().map(|p| p.to_string_lossy().into_owned()),
            cgroup,
            io_class,
            io_level,
            pss_bytes: rollup.as_deref().and_then(|t| kb_field(t, "Pss:")),
            swap_bytes: rollup.as_deref().and_then(|t| kb_field(t, "Swap:")),
            read_total_bytes: io.map(|x| x.0),
            write_total_bytes: io.map(|x| x.1),
            ports: self.ports(pid),
        })
    }

    /// Listening sockets of `pid`, in its own network namespace (containers).
    fn ports(&self, pid: u32) -> Vec<ListenPort> {
        let dir = self.p(pid.to_string());
        let mut inodes = HashSet::new();
        for e in std::fs::read_dir(dir.join("fd")).into_iter().flatten().flatten().take(65_536) {
            if let Ok(l) = std::fs::read_link(e.path())
                && let Some(i) = l.to_str().and_then(|s| s.strip_prefix("socket:[")).and_then(|s| s.strip_suffix(']')).and_then(|s| s.parse().ok())
            {
                inodes.insert(i);
            }
        }
        if inodes.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (file, proto, v6) in [("tcp", "tcp", false), ("tcp6", "tcp", true), ("udp", "udp", false), ("udp6", "udp", true)] {
            if let Ok(t) = std::fs::read_to_string(dir.join("net").join(file)) {
                out.extend(parse_listen(&t, proto, v6, &inodes));
            }
        }
        out.sort_by(|a, b| (a.port, &a.protocol, &a.address).cmp(&(b.port, &b.protocol, &b.address)));
        out
    }

    /// The threads of `pid` (nice and I/O priority are per thread).
    fn threads(&self, pid: u32) -> Vec<u32> {
        let mut v: Vec<u32> = std::fs::read_dir(self.p(format!("{pid}/task")))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect();
        if v.is_empty() {
            v.push(pid);
        }
        v
    }

    pub fn signal(&self, pid: u32, req: &SignalRequest, who: &str) -> Res<()> {
        let (st, meta) = self.identify(pid, req.start_ms)?;
        self.refuse_protected(pid, &st, &meta)?;
        let (sig, name) = match req.signal {
            ProcSignal::Term => (libc::SIGTERM, "term"),
            ProcSignal::Kill => (libc::SIGKILL, "kill"),
        };
        audit(who, &format!("signal {name}"), pid, &st.comm, self.dry);
        if self.dry {
            return Ok(());
        }
        // SAFETY: kill(2) with a checked PID and a fixed signal
        if unsafe { libc::kill(pid as libc::pid_t, sig) } != 0 {
            return Err(os_error("signal", pid));
        }
        Ok(())
    }

    pub fn priority(&self, pid: u32, req: &PriorityRequest, who: &str) -> Res<()> {
        if let Some(n) = req.nice
            && !(-20..=19).contains(&n)
        {
            return Err(Error::invalid("nice", "nice goes from -20 to 19"));
        }
        let io = match req.io_class {
            None => None,
            Some(IoClass::BestEffort) if req.io_level <= 7 => Some((2, u32::from(req.io_level))),
            Some(IoClass::BestEffort) => return Err(Error::invalid("io_level", "best-effort levels go from 0 to 7")),
            Some(IoClass::Idle) => Some((3, 0)),
            Some(_) => return Err(Error::invalid("io_class", "only best_effort and idle")),
        };
        let (st, meta) = self.identify(pid, req.start_ms)?;
        self.refuse_protected(pid, &st, &meta)?;
        audit(who, &format!("priority nice={:?} io={:?}/{}", req.nice, req.io_class, req.io_level), pid, &st.comm, self.dry);
        if self.dry {
            return Ok(());
        }
        for tid in self.threads(pid) {
            if let Some(n) = req.nice {
                // SAFETY: setpriority(2) on one thread id
                if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, n) } != 0 && tid == pid {
                    return Err(os_error("nice", pid));
                }
            }
            if let Some((class, level)) = io {
                // SAFETY: ioprio_set(2): IOPRIO_WHO_PROCESS (1), one thread id
                let r = unsafe { libc::syscall(libc::SYS_ioprio_set, 1, tid as libc::c_int, ((class << 13) | level) as libc::c_int) };
                if r != 0 && tid == pid {
                    return Err(os_error("I/O priority", pid));
                }
            }
        }
        Ok(())
    }

    /// The unit `restart-service` may restart for `pid`, after the checks.
    pub fn service_of(&self, pid: u32, start_ms: u64) -> Res<String> {
        let (st, meta) = self.identify(pid, start_ms)?;
        let g = self.group_of(&meta.group, meta.uid);
        if g.kind != GroupKind::Service {
            return Err(Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, "this process does not belong to a system service".into()));
        }
        if !g.restartable {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                error_codes::PROCESS_PROTECTED,
                format!("{} ({}) is needed by the system or rynas", g.id, st.comm),
            ));
        }
        if !valid_unit(&g.id) {
            return Err(Error::bad_request("unexpected unit name"));
        }
        Ok(g.id)
    }
}

/// rynasd sits in its own cgroup namespace when its cgroup path names no unit.
fn ns_private_of(path: &str) -> bool {
    !path.contains(".service") && !path.contains(".scope")
}

fn valid_unit(u: &str) -> bool {
    u.ends_with(".service")
        && u.len() <= 256
        && !u.starts_with('-')
        && u.bytes().all(|b| b.is_ascii_alphanumeric() || b":_.@-\\".contains(&b))
}

fn read_capped(p: &Path, max: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(p).ok()?.take(max as u64).read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn ioprio(pid: u32) -> (IoClass, u8) {
    // SAFETY: ioprio_get(2): IOPRIO_WHO_PROCESS (1)
    let v = unsafe { libc::syscall(libc::SYS_ioprio_get, 1, pid as libc::c_int) };
    if v < 0 {
        return (IoClass::None, 4);
    }
    let class = match v >> 13 {
        1 => IoClass::Realtime,
        2 => IoClass::BestEffort,
        3 => IoClass::Idle,
        _ => IoClass::None,
    };
    (class, (v & 0x7) as u8)
}

fn os_error(what: &str, pid: u32) -> Error {
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ESRCH) => Error::not_found(format!("process {pid} ended")),
        Some(libc::EPERM | libc::EACCES) => Error::new(
            StatusCode::FORBIDDEN,
            error_codes::PERMISSION_DENIED,
            format!("{what} of process {pid}: not allowed ({e})"),
        ),
        _ => Error::internal(format!("{what} of process {pid}: {e}")),
    }
}

fn audit(who: &str, action: &str, pid: u32, name: &str, dry: bool) {
    let dry = if dry { " (dry system: not done)" } else { "" };
    tracing::info!(target: "rynas::audit", "process {action} {pid} ({name}) by {who}{dry}");
}

/// Drops the previous sample of an idle view every [`IDLE_DROP`].
pub fn spawn_trim(p: Arc<Processes>) {
    tokio::spawn(async move {
        let mut t = tokio::time::interval(IDLE_DROP);
        loop {
            t.tick().await;
            p.trim_idle();
        }
    });
}

// ------------------------------------------------------------------ API

pub struct ProcessesApi {
    pub processes: Arc<Processes>,
    pub ops: Arc<Ops>,
}

pub type ProcHandle = Option<Arc<ProcessesApi>>;

fn api(st: &ProcHandle) -> Res<Arc<ProcessesApi>> {
    st.clone().ok_or_else(|| {
        Error::new(StatusCode::SERVICE_UNAVAILABLE, error_codes::BACKEND_UNAVAILABLE, "processes are not available".into())
    })
}

pub fn router(st: ProcHandle) -> Router {
    let admins = st.as_ref().map(|s| s.ops.admins.clone()).unwrap_or_default();
    Router::new()
        .route("/v1/processes", get(list_get))
        .route("/v1/processes/{pid}", get(detail_get))
        .route("/v1/processes/{pid}/signal", post(signal_post))
        .route("/v1/processes/{pid}/priority", post(priority_post))
        .route("/v1/processes/{pid}/restart-service", post(restart_post))
        .with_state(st)
        .route_layer(axum::middleware::from_fn_with_state(
            crate::roles::AdminGate { policy: admins, what: "processes" },
            crate::roles::admin_only,
        ))
}

#[derive(Deserialize, Default)]
struct ListQuery {
    kernel: Option<bool>,
}

async fn list_get(State(st): State<ProcHandle>, Query(q): Query<ListQuery>) -> Res<Response> {
    let a = api(&st)?;
    let list = a.processes.list().await?;
    // kernel threads only when asked for: the groups stay, their indexes with them
    let body = if q.kernel.unwrap_or(false) {
        serde_json::to_vec(&*list)
    } else {
        let kernel = list.groups.iter().position(|g| g.kind == GroupKind::Kernel).map(|i| i as u32);
        let mut view = (*list).clone();
        view.processes.retain(|p| Some(p.group) != kernel);
        serde_json::to_vec(&view)
    }
    .map_err(Error::internal)?;
    Ok(([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response())
}

#[derive(Deserialize)]
struct StartQuery {
    start_ms: u64,
}

async fn detail_get(State(st): State<ProcHandle>, UrlPath(pid): UrlPath<u32>, Query(q): Query<StartQuery>) -> Res<Json<ProcessDetail>> {
    let p = api(&st)?.processes.clone();
    let job = tokio::task::spawn_blocking(move || p.detail(pid, q.start_ms));
    tokio::time::timeout(SAMPLE_TIMEOUT, job)
        .await
        .map_err(|_| Error::new(StatusCode::GATEWAY_TIMEOUT, error_codes::TIMEOUT, "reading /proc took too long".into()))?
        .map_err(Error::internal)?
        .map(Json)
}

async fn signal_post(
    State(st): State<ProcHandle>,
    hd: HeaderMap,
    UrlPath(pid): UrlPath<u32>,
    body: Result<Json<SignalRequest>, JsonRejection>,
) -> Res<StatusCode> {
    let (a, who) = (api(&st)?, user(&hd)?);
    a.ops.admins.require_admin(&who, "processes")?;
    let Json(req) = body?;
    let p = a.processes.clone();
    tokio::task::spawn_blocking(move || p.signal(pid, &req, &who)).await.map_err(Error::internal)??;
    Ok(StatusCode::NO_CONTENT)
}

async fn priority_post(
    State(st): State<ProcHandle>,
    hd: HeaderMap,
    UrlPath(pid): UrlPath<u32>,
    body: Result<Json<PriorityRequest>, JsonRejection>,
) -> Res<StatusCode> {
    let (a, who) = (api(&st)?, user(&hd)?);
    a.ops.admins.require_admin(&who, "processes")?;
    let Json(req) = body?;
    let p = a.processes.clone();
    tokio::task::spawn_blocking(move || p.priority(pid, &req, &who)).await.map_err(Error::internal)??;
    Ok(StatusCode::NO_CONTENT)
}

async fn restart_post(
    State(st): State<ProcHandle>,
    hd: HeaderMap,
    UrlPath(pid): UrlPath<u32>,
    body: Result<Json<RestartRequest>, JsonRejection>,
) -> Res<Response> {
    let (a, who) = (api(&st)?, user(&hd)?);
    a.ops.admins.require_admin(&who, "processes")?;
    let Json(req) = body?;
    let p = a.processes.clone();
    let unit = tokio::task::spawn_blocking(move || p.service_of(pid, req.start_ms)).await.map_err(Error::internal)??;
    let dry = a.processes.dry;
    tracing::info!(target: "rynas::audit", "process restart-service {unit} (from {pid}) by {who}{}", if dry { " (dry system: not done)" } else { "" });
    let id = a.ops.tasks.spawn("processes.restart-service", &who, move |ctx| async move {
        ctx.log(&format!("restart {unit}"));
        if dry {
            ctx.log("(dry system: not done)");
            return Ok(());
        }
        let cmd = Cmd::new("systemctl").args(["restart", "--", unit.as_str()]).timeout(Duration::from_secs(120));
        ctx.run(&cmd).await.map(|_| ()).map_err(|e| e.to_string())
    })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_odd_names() {
        let t = "1234 (my (odd) name) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 -5 3 0 98765 1000000 512 18446744073709551615";
        let s = parse_stat(t).unwrap();
        assert_eq!(s.comm, "my (odd) name");
        assert_eq!((s.state, s.ppid, s.cpu_ticks, s.nice, s.threads, s.start_ticks, s.rss_pages), ('S', 1, 300, -5, 3, 98765, 512));
        assert_eq!(s.flags & PF_KTHREAD, 0);
        assert!(parse_stat("garbage").is_none());
    }

    #[test]
    fn io_counters() {
        let t = "rchar: 10\nwchar: 20\nsyscr: 1\nsyscw: 1\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";
        assert_eq!(parse_io(t), Some((4096, 8192)));
        assert_eq!(parse_io("rchar: 1\n"), None);
    }

    #[test]
    fn groups_from_cgroups() {
        let id = "a".repeat(64);
        assert_eq!(classify("/system.slice/smbd.service", false), (GroupKind::Service, "smbd.service".into()));
        assert_eq!(classify("/../smbd.service", true), (GroupKind::Service, "smbd.service".into()));
        assert_eq!(classify(&format!("/../docker-{id}.scope"), true), (GroupKind::Container, id.clone()));
        assert_eq!(classify(&format!("/docker/{id}"), false), (GroupKind::Container, id.clone()));
        assert_eq!(classify("/../../user.slice/user-1000.slice/session-3.scope", true), (GroupKind::Session, "session-3".into()));
        assert_eq!(
            classify("/user.slice/user-1000.slice/user@1000.service/app.slice/foo.service", false),
            (GroupKind::User, "user@1000.service".into())
        );
        assert_eq!(classify("/../../init.scope", true), (GroupKind::System, "init.scope".into()));
        assert_eq!(classify("/daemon", true), (GroupKind::Service, "rynasd.service".into()));
        assert_eq!(classify("/", false), (GroupKind::System, "system".into()));
        assert!(ns_private_of("/daemon") && !ns_private_of("/system.slice/rynasd.service"));
        assert_eq!(host_cgroup("/../smbd.service", true), "/system.slice/smbd.service");
        assert_eq!(host_cgroup("/workers", true), "/system.slice/rynasd.service/workers");
    }

    #[test]
    fn listening_sockets() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 00000000:01BD 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 111 1 0 100 0 0 10 0\n   1: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 222 1\n   2: 0100007F:1F91 0100007F:9C40 01 00000000:00000000 00:00000000 00000000     0        0 111 1\n";
        let mine: HashSet<u64> = [111].into_iter().collect();
        let v = parse_listen(tcp, "tcp", false, &mine);
        assert_eq!(v, vec![ListenPort { protocol: "tcp".into(), address: "0.0.0.0".into(), port: 445 }]);
        let tcp6 = "header\n   0: 00000000000000000000000000000000:0016 00000000000000000000000000000000:0000 0A 0 0 0 0 0 333 1\n   1: 00000000000000000000000001000000:0050 00000000000000000000000000000000:0000 0A 0 0 0 0 0 333 1\n";
        let mine: HashSet<u64> = [333].into_iter().collect();
        let v = parse_listen(tcp6, "tcp", true, &mine);
        assert_eq!(v[0], ListenPort { protocol: "tcp".into(), address: "::".into(), port: 22 });
        assert_eq!(v[1].address, "::1");
    }

    #[test]
    fn units_and_text() {
        assert!(valid_unit("smbd.service") && valid_unit("getty@tty1.service") && valid_unit("dev-x\\x2dy.service"));
        assert!(!valid_unit("-x.service") && !valid_unit("x.scope") && !valid_unit("a b.service"));
        assert_eq!(cmdline_text(b"/usr/sbin/smbd\0--foreground\0"), "/usr/sbin/smbd --foreground");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(parse_proc_stat("cpu  10 0 10 70 10 0 0 0 0 0\nbtime 1000\n"), (Some((20, 100)), Some(1000)));
    }

    /// A fake `/proc`: two samples give CPU % and I/O rates, a reused PID starts over,
    /// protected processes are flagged, kernel threads are their own group.
    #[test]
    fn samples_and_rates() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        let w = |p: &str, t: &str| {
            let f = r.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, t).unwrap();
        };
        let proc_stat = |busy: u64| format!("cpu  {busy} 0 0 {} 0 0 0 0 0 0\nbtime 1000\n", 10 * busy);
        let stat = |pid: u32, comm: &str, ppid: u32, flags: u64, ticks: u64, start: u64| {
            format!("{pid} ({comm}) S {ppid} 0 0 0 -1 {flags} 0 0 0 0 {ticks} 0 0 0 20 0 1 0 {start} 0 10")
        };
        w("stat", &proc_stat(100));
        w("meminfo", "MemTotal:  2048 kB\n");
        w("1/stat", &stat(1, "systemd", 0, 0, 5, 1));
        w("1/cgroup", "0::/../../init.scope\n");
        w("2/stat", &stat(2, "kthreadd", 0, PF_KTHREAD, 0, 1));
        w("50/stat", &stat(50, "smbd", 1, 0, 100, 500));
        w("50/cgroup", "0::/../smbd.service\n");
        w("50/cmdline", "/usr/sbin/smbd\0--foreground\0");
        w("50/io", "read_bytes: 1000\nwrite_bytes: 0\n");
        w("60/stat", &stat(60, "rynasd", 1, 0, 0, 500));
        w("60/cgroup", "0::/daemon\n");
        let p = Processes::with_root(r.into(), true, 60, true, None, None);
        let s1 = p.take(None, 2_000_000);
        assert!(s1.list.processes.iter().all(|x| x.cpu_pct.is_none()));
        let kinds: Vec<GroupKind> = s1.list.groups.iter().map(|g| g.kind).collect();
        assert!(kinds.contains(&GroupKind::Kernel) && kinds.contains(&GroupKind::Service));
        let by = |l: &ProcessList, pid: u32| l.processes.iter().find(|x| x.pid == pid).cloned().unwrap();
        assert!(by(&s1.list, 1).protected && by(&s1.list, 2).protected && by(&s1.list, 60).protected);
        assert!(!by(&s1.list, 50).protected);
        assert_eq!(by(&s1.list, 50).cmd, "/usr/sbin/smbd --foreground");
        assert_eq!(s1.list.mem_total_bytes, 2048 * 1024);
        let g = &s1.list.groups[by(&s1.list, 50).group as usize];
        assert_eq!((g.name.as_str(), g.restartable), ("smbd", true));
        assert!(!s1.list.groups[by(&s1.list, 60).group as usize].restartable);

        std::thread::sleep(Duration::from_millis(100));
        w("stat", &proc_stat(200));
        w("50/stat", &stat(50, "smbd", 1, 0, 100 + p.hz / 10, 500));
        w("50/io", "read_bytes: 2000\nwrite_bytes: 512\n");
        let s2 = p.take(Some(&s1), 2_000_100);
        let x = by(&s2.list, 50);
        let pct = x.cpu_pct.unwrap();
        assert!(pct > 30.0 && pct <= 105.0, "{pct}");
        assert!(x.read_bytes_s.unwrap() > 0 && x.write_bytes_s.unwrap() > 0);
        assert!(s2.list.cpu_pct.is_some());

        // the PID now belongs to another process: no rates
        w("50/stat", &stat(50, "smbd", 1, 0, 0, 900));
        let s3 = p.take(Some(&s2), 2_000_200);
        assert!(by(&s3.list, 50).cpu_pct.is_none());
    }
}
