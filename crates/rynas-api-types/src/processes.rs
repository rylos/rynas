//! Processes (`/api/v1/processes`), administrators only (D-PROCESSES,
//! `docs/research/processes.md`).

use serde::{Deserialize, Serialize};

/// What a group of processes is, from its cgroup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GroupKind {
    /// A systemd system service (`smbd.service`).
    Service,
    /// A Docker container (named after its catalogue app when it is one).
    Container,
    /// A login session (`session-N.scope`: SSH, console).
    Session,
    /// A user's own services (`user@UID.service`).
    User,
    /// Kernel threads.
    Kernel,
    /// Anything else (init, scopes outside a service).
    #[default]
    System,
}

/// One group of the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProcessGroup {
    pub kind: GroupKind,
    /// Stable key: the unit name, the container id, `kernel`, `system`.
    pub id: String,
    /// What to show: the unit without `.service`, the container name, the session's user.
    pub name: String,
    /// The catalogue app of a container (its name), when the stack is an installed app.
    pub app: Option<String>,
    /// The compose project of a container.
    pub stack: Option<String>,
    /// A restart is offered: services through `restart-service`, containers through
    /// `/docker/containers/{id}/restart`. False for protected units and sessions.
    pub restartable: bool,
}

/// Process state (`/proc/<pid>/stat` field 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProcState {
    Running,
    Sleeping,
    /// Uninterruptible wait, usually disk I/O (`D`).
    DiskWait,
    Stopped,
    Zombie,
    Idle,
    #[default]
    Other,
}

/// One process of the list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    /// Start time, ms since the epoch; with `pid` it names the process in actions.
    pub start_ms: u64,
    /// Short name (`comm`).
    pub name: String,
    /// Command line, at most 256 characters (the detail has all of it).
    pub cmd: String,
    pub user: String,
    pub state: ProcState,
    pub threads: u32,
    pub nice: i32,
    /// Share of one CPU (100 = one core busy); `None` on the first sample of a process.
    pub cpu_pct: Option<f32>,
    pub rss_bytes: u64,
    /// Disk I/O since the last sample; `None` when unknown (first sample, or a process of
    /// another user whose counters the daemon may not read).
    pub read_bytes_s: Option<u64>,
    pub write_bytes_s: Option<u64>,
    /// Index into [`ProcessList::groups`].
    pub group: u32,
    /// No kill, priority or restart: the system or rynas needs it.
    pub protected: bool,
}

/// `GET /api/v1/processes` (`?kernel=true` adds kernel threads).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProcessList {
    pub read_at_ms: u64,
    pub cpus: u32,
    pub mem_total_bytes: u64,
    /// Whole-system CPU use since the last sample (100 = every core busy).
    pub cpu_pct: Option<f32>,
    pub groups: Vec<ProcessGroup>,
    pub processes: Vec<ProcessInfo>,
    /// Signals, priorities and restarts are only pretended (development `--dry-system`).
    pub dry: bool,
}

/// I/O scheduling class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IoClass {
    /// No class set: follows the nice value.
    #[default]
    None,
    Realtime,
    BestEffort,
    Idle,
}

/// A listening socket of a process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ListenPort {
    /// `tcp`, `udp`.
    pub protocol: String,
    pub address: String,
    pub port: u16,
}

/// `GET /api/v1/processes/{pid}?start_ms=`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProcessDetail {
    pub info: ProcessInfo,
    pub group: ProcessGroup,
    /// The whole command line.
    pub cmdline: String,
    /// Executable path, when readable.
    pub exe: Option<String>,
    /// cgroup v2 path as the host sees it, when known.
    pub cgroup: Option<String>,
    pub io_class: IoClass,
    /// 0 (highest) – 7 for best effort and realtime.
    pub io_level: u8,
    /// Proportional memory (shared pages split among their users), when readable.
    pub pss_bytes: Option<u64>,
    pub swap_bytes: Option<u64>,
    /// Bytes read and written since it started, when readable.
    pub read_total_bytes: Option<u64>,
    pub write_total_bytes: Option<u64>,
    pub ports: Vec<ListenPort>,
}

/// Signal of `POST /api/v1/processes/{pid}/signal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcSignal {
    /// SIGTERM: ask it to end.
    Term,
    /// SIGKILL: end it now.
    Kill,
}

/// Body of `POST /api/v1/processes/{pid}/signal` (204).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalRequest {
    pub signal: ProcSignal,
    /// [`ProcessInfo::start_ms`]: another start time is `not_found` (the PID was reused).
    pub start_ms: u64,
}

/// Body of `POST /api/v1/processes/{pid}/priority` (204).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityRequest {
    pub start_ms: u64,
    /// −20 … 19; `None` leaves it.
    pub nice: Option<i32>,
    /// `best_effort` or `idle` (`realtime` and `none` are refused); `None` leaves it.
    pub io_class: Option<IoClass>,
    /// 0 – 7 for `best_effort`.
    #[serde(default)]
    pub io_level: u8,
}

/// Body of `POST /api/v1/processes/{pid}/restart-service` (202, task `processes.restart-service`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartRequest {
    pub start_ms: u64,
}
