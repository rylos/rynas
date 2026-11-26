//! PC backup with `rynas-agent`, file method (D-PCBACKUP, taken 2026-11-11;
//! `docs/research/pc-backup.md` §4, API in `docs/phase1/plan.md` "API v1: PC backup"). The
//! other method, btrfs snapshots received with btrbk, is [`crate::pc_backup`].
//!
//! - One restic-format repository per **backup group** under the folder of the settings
//!   (`<folder>/<group>/`, a btrfs subvolume when the folder is on btrfs, `0700 root`). By
//!   default every PC gets a group of its own (isolation: any key of a restic repository reads
//!   all of it); PCs joined into one group deduplicate against each other. The NAS creates the
//!   repository with `rynas-backup-worker` (`init_pc`, then `add_key` for each PC and for the
//!   recovery kit); the NAS key's password is sealed with rynasd's age key. Versions carry the
//!   tag `pc-<device>`: listings and retention are per PC inside a group.
//! - [`repo`] serves the restic REST protocol for the agents: a token per PC (only its own
//!   group's path), append-only (no overwrite, no delete but locks), the group's quota with
//!   the bytes each PC uploads counted, writes refused while the NAS maintains the repository.
//! - [`maint`] runs retention, prune and check on the NAS (never on the PC) inside a memory
//!   budget sized from the RAM, the daily schedule, the late/critical notifications, pairing,
//!   revocation, the recovery kit and the streamed downloads.
//! - The console (`/v1/pcbackup/…`, administrators only) and the agent's calls
//!   (`/v1/pcbackup/agent/…`: pairing, plan with a long poll, progress, report, restore login
//!   with a one-time restore code) are below.
//!
//! State: `<state>/pc-agent/registry.json` (written atomically; codes and tokens only as
//! SHA-256), the newest manifest per PC in `<state>/pc-agent/manifests/`. `--dry-system`
//! creates plain folders instead of subvolumes; everything else is real (the worker runs as a
//! child process).

pub mod maint;
pub mod repo;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use base64::Engine;
use rynas_api_types::backup::{
    NodeEntry, RepoSpec, SnapshotEntry, SnapshotScope, WorkerEvent, WorkerOp, WorkerRequest,
};
use rynas_api_types::pcagent::*;
use rynas_api_types::{TaskStarted, error_codes};
use rynas_backup::{Launch, Limits, run_worker};
use rynas_notify::Notifier;
use rynas_tasks::CancellationToken;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ops::{Error, Ops, Res, user};

const PAIRING_TTL_MS: u64 = 15 * 60 * 1000;
const RESTORE_CODE_TTL_MS: u64 = 3600 * 1000;
/// The codes of a restore TUI's administrator login: the PC is chosen right away.
const ADMIN_CODE_TTL_MS: u64 = 5 * 60 * 1000;
/// A restore session ends after this long without a request (each accepted request pushes
/// the end out again) …
pub const SESSION_IDLE_MS: u64 = 3600 * 1000;
/// … and at the latest this long after the login, however busy it is.
pub const SESSION_MAX_MS: u64 = 48 * 3600 * 1000;
pub const HISTORY: usize = 30;
/// Wrong codes allowed from one client (an IPv4 address, an IPv6 /64) in [`FAIL_WINDOW`]
/// before that client's codes are refused for that long.
const MAX_FAILURES: usize = 10;
/// Wrong codes from everybody in [`FAIL_WINDOW`] after which every code check is slowed
/// down by [`SLOW_CHECK`] (never refused: nobody can lock the others out).
const SLOW_FAILURES: usize = 200;
const SLOW_CHECK: Duration = Duration::from_secs(2);
const FAIL_WINDOW: Duration = Duration::from_secs(600);
/// Clients remembered with wrong codes at most (the oldest are dropped).
const MAX_FAIL_SOURCES: usize = 4096;
/// The registry changes that need no immediate write (bytes uploaded, last seen, the end of
/// a restore session) are written at most this often.
pub const FLUSH_EVERY: Duration = Duration::from_secs(30);
/// Reads (versions, browse) give up after this long.
pub const READ_TIMEOUT: Duration = Duration::from_secs(110);
/// Downloads and zips running at once.
const MAX_STREAMS: usize = 2;
/// Longest long poll of the plan.
const MAX_WAIT: u64 = 55;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn sha_hex(s: &[u8]) -> String {
    let d = Sha256::digest(s);
    let mut out = String::with_capacity(64);
    for b in d.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn random(n: usize) -> Res<Vec<u8>> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).map_err(|e| Error::internal(e.to_string()))?;
    Ok(b)
}

/// 256-bit token, hex.
pub fn new_token() -> Res<String> {
    Ok(random(32)?.iter().map(|b| format!("{b:02x}")).collect())
}

const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// `XXXX-XXXX-XXXX`: 60 bits, Crockford base32 (no I, L, O, U).
fn new_code() -> Res<String> {
    let b = random(12)?;
    let mut s = String::with_capacity(14);
    for (i, x) in b.iter().enumerate() {
        if i > 0 && i % 4 == 0 {
            s.push('-');
        }
        s.push(CROCKFORD[(*x & 31) as usize] as char);
    }
    Ok(s)
}

/// A typed code in canonical form: upper case, without separators, the look-alike letters
/// mapped as Crockford does (O→0, I/L→1).
pub fn normalize_code(c: &str) -> String {
    c.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

fn code_hash(c: &str) -> String {
    sha_hex(normalize_code(c).as_bytes())
}

// ------------------------------------------------------------------ records

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Alert {
    #[default]
    None,
    Late,
    Critical,
}

/// A paired PC.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Dev {
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub agent_version: String,
    #[serde(default)]
    pub machine_id: String,
    pub created_ms: u64,
    #[serde(default)]
    pub last_seen_ms: Option<u64>,
    /// When the PC came online this time: its first contact after more than
    /// [`OFFLINE_AFTER_MS`] without any ([`Dev::contact`]). A due backup is late only when the
    /// agent would have started it within this stretch online ([`missed_online`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub online_since_ms: Option<u64>,
    pub group: String,
    /// SHA-256 of the device token; empty once revoked.
    #[serde(default)]
    pub token_sha: String,
    /// The PC's key file in the group's repository.
    #[serde(default)]
    pub pc_key: String,
    #[serde(default)]
    pub uploaded_bytes: u64,
    pub settings: PcDeviceSettings,
    #[serde(default)]
    pub last_ok_ms: Option<u64>,
    /// When the last good run started (its interval counts from there, [`last_ok`]).
    #[serde(default)]
    pub last_ok_start_ms: Option<u64>,
    #[serde(default)]
    pub history: VecDeque<PcRun>,
    #[serde(default)]
    pub versions: u32,
    #[serde(default)]
    pub command: Option<AgentCommand>,
    /// When `command` was queued: the PC has not taken it while its last contact is older.
    #[serde(default)]
    pub command_at_ms: Option<u64>,
    #[serde(default)]
    pub rev: u64,
    #[serde(default)]
    pub alert: Alert,
    #[serde(default)]
    pub failed_alerted: bool,
    #[serde(default)]
    pub manifest_ms: Option<u64>,
    #[serde(default)]
    pub manifest_reasons: Vec<ReadyReason>,
    /// What makes the layout need manual steps (the LVM groups the restore refuses, and why).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manifest_details: Vec<String>,
    #[serde(default)]
    pub revoked: bool,
    /// The btrfs method's folder this PC sends into ([`crate::pc_backup::PcBackup::agent_folder_id`]),
    /// set when the method is chosen: a folder removed and made again does not take it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btrfs_folder: Option<u64>,
    /// The PC's root filesystem is btrfs (from pairing, then every report).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_btrfs: Option<bool>,
    /// What the agent last reported of the PC (network cards, sleep modes; agents from
    /// 0.8.26, [`AGENT_FACTS`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<AgentFacts>,
    /// The NAS woke the PC for a due backup at this time ([`AgentPlan::woken_ms`]); cleared by
    /// the next run report, ignored after [`WOKEN_FOR_MS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub woken_ms: Option<u64>,
}

/// How long a "woken by the NAS" mark stays in the plan without a run report.
pub const WOKEN_FOR_MS: u64 = 2 * 3600 * 1000;

/// The first agent version that reads [`AgentCommand::Sleep`] and [`AgentCommand::EnableWol`]
/// (older ones fail to read a plan that holds them).
pub const POWER_AGENT: (u32, u32, u32) = (0, 8, 26);

/// The agent takes power commands: it reported its facts and its version is known and at
/// least [`POWER_AGENT`] (an unknown version is never trusted here).
pub fn takes_power_commands(d: &Dev) -> bool {
    let mut p = d.agent_version.trim().trim_start_matches('v').split(['.', '-', '+']);
    let mut num = || p.next().and_then(|x| x.parse::<u32>().ok());
    let known = match (num(), num(), num()) {
        (Some(a), Some(b), Some(c)) => (a, b, c) >= POWER_AGENT,
        _ => false,
    };
    known && d.facts.is_some()
}

/// A power command (sleep, enable Wake-on-LAN) that the plan carries.
fn is_power_command(c: Option<&AgentCommand>) -> bool {
    matches!(c, Some(AgentCommand::Sleep { .. } | AgentCommand::EnableWol { .. }))
}

/// A backup group: one repository.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Grp {
    pub settings: PcGroupSettings,
    pub repo: String,
    pub created_ms: u64,
    /// The NAS key's password, sealed (base64).
    pub sealed: String,
    #[serde(default)]
    pub recovery_key: Option<String>,
    #[serde(default)]
    pub recovery_kit_ms: Option<u64>,
    #[serde(default)]
    pub maintenance: PcMaintenance,
    /// Local day number (days since the epoch) of the last scheduled maintenance.
    #[serde(default)]
    pub maintained_day: Option<i64>,
    /// Next twelfth read back by the check (1–12).
    #[serde(default)]
    pub check_slice: u32,
    /// Average chunk of the repository in MiB (groups made before the choice existed: 2).
    #[serde(default = "two_mib")]
    pub chunk_mib: u8,
}

fn two_mib() -> u8 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pairing {
    pub id: String,
    pub code_sha: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub quota_gib: Option<u32>,
    pub expires_ms: u64,
    /// A PC for the btrfs method: it takes this folder's name, no repository.
    #[serde(default)]
    pub btrfs_target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreCode {
    pub code_sha: String,
    pub device: String,
    pub expires_ms: u64,
    /// The codes of one administrator login (one per PC): using one ends the others.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Registry {
    #[serde(default)]
    pub settings: PcAgentSettings,
    #[serde(default)]
    pub groups: BTreeMap<String, Grp>,
    #[serde(default)]
    pub devices: BTreeMap<String, Dev>,
    #[serde(default)]
    pub pairings: Vec<Pairing>,
    #[serde(default)]
    pub restore_codes: Vec<RestoreCode>,
    /// Restore sessions, kept here so that a rynasd restart does not cut a restore off.
    #[serde(default)]
    pub sessions: Vec<Session>,
    #[serde(default)]
    pub seq: u64,
}

/// A restore session: a token (only its SHA-256 here) that reads its devices' groups.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Session {
    pub token_sha: String,
    pub devices: Vec<String>,
    pub created_ms: u64,
    /// Pushed to now + [`SESSION_IDLE_MS`] by every request, never past created + [`SESSION_MAX_MS`].
    pub expires_ms: u64,
    /// The group of the session's temporary key and its id: the password the agent got opens
    /// only this key file, which goes when the session ends.
    #[serde(default)]
    pub key_group: String,
    #[serde(default)]
    pub key_id: String,
}

impl Session {
    /// The session is still valid at `now`: its devices, and the end pushed out.
    pub fn slide(&mut self, now: u64) -> Option<Vec<String>> {
        if self.expires_ms <= now {
            return None;
        }
        self.expires_ms = (now + SESSION_IDLE_MS).min(self.created_ms + SESSION_MAX_MS);
        Some(self.devices.clone())
    }
}

/// What is happening to a group's repository right now (memory only).
#[derive(Debug, Default)]
pub struct Live {
    /// The NAS maintains the repository (or deletes the group): agent writes answer 423,
    /// browsing and downloads 423 too.
    pub maintaining: bool,
    pub writes: u32,
    pub last_write: Option<Instant>,
    /// Bytes on disk, counted once and kept up to date by the uploads.
    pub used: Option<u64>,
    /// Bytes the uploads running now may still add (their Content-Length, or what is left of
    /// the quota): counted against the quota with `used`.
    pub reserved: u64,
}

impl Live {
    /// Room for an upload of `declared` bytes (unknown: up to `max`) in a group with `quota`
    /// (0: none): the bytes to reserve and the most the body may carry.
    pub fn reserve(&mut self, quota: u64, declared: Option<u64>, max: u64) -> Result<(u64, u64), &'static str> {
        if quota == 0 {
            return Ok((0, declared.unwrap_or(max).min(max)));
        }
        let free = quota.saturating_sub(self.used.unwrap_or(0).saturating_add(self.reserved));
        let want = declared.unwrap_or(free.min(max));
        if want > free || free == 0 {
            return Err("the group's quota is used up");
        }
        self.reserved += want;
        Ok((want, want))
    }
}

/// Wrong codes lately, per client and in all.
#[derive(Debug, Default)]
struct Failures {
    by_source: HashMap<String, VecDeque<Instant>>,
    all: VecDeque<Instant>,
}

impl Failures {
    fn trim(&mut self) {
        let old = |t: &Instant| t.elapsed() > FAIL_WINDOW;
        while self.all.front().is_some_and(old) {
            self.all.pop_front();
        }
        self.by_source.retain(|_, q| {
            while q.front().is_some_and(old) {
                q.pop_front();
            }
            !q.is_empty()
        });
    }

    /// `source` may try a code: `Err` when it sent too many wrong ones, `Ok(true)` when
    /// everybody together did and the check is slowed down.
    fn check(&mut self, source: &str) -> Result<bool, ()> {
        self.trim();
        if self.by_source.get(source).is_some_and(|q| q.len() >= MAX_FAILURES) {
            return Err(());
        }
        Ok(self.all.len() >= SLOW_FAILURES)
    }

    fn record(&mut self, source: &str) {
        let now = Instant::now();
        self.all.push_back(now);
        if self.all.len() > MAX_FAIL_SOURCES * MAX_FAILURES {
            self.all.pop_front();
        }
        if !self.by_source.contains_key(source) && self.by_source.len() >= MAX_FAIL_SOURCES {
            // the source whose last failure is oldest makes room
            if let Some(k) = self.by_source.iter().min_by_key(|(_, q)| q.back().copied()).map(|(k, _)| k.clone()) {
                self.by_source.remove(&k);
            }
        }
        self.by_source.entry(source.to_string()).or_default().push_back(now);
    }
}

/// The client a code comes from, as rynas-web saw it: an IPv4 address or an IPv6 /64 (one
/// host usually holds a whole /64).
pub fn source_of(h: &HeaderMap) -> String {
    let raw = h
        .get(rynas_auth::ipc::CLIENT_ADDR_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim();
    match raw.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(a)) => a.to_string(),
        Ok(std::net::IpAddr::V6(a)) => match a.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = a.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
        Err(_) => "local".to_string(),
    }
}

pub struct PcAgent {
    pub ops: Arc<Ops>,
    pub notifier: Arc<dyn Notifier>,
    pub state_dir: PathBuf,
    /// rynasd's age key folder (`<state>/notify`).
    pub secret_dir: PathBuf,
    pub dry: bool,
    pub launch: Launch,
    /// The backup worker slots shared with the NAS backups.
    pub workers: Arc<tokio::sync::Semaphore>,
    pub streams: Arc<tokio::sync::Semaphore>,
    pub tls: Option<Arc<crate::tls::TlsSvc>>,
    /// rynas-web's own HTTPS port (the addresses agents are told, [`PcAgent::trust`]).
    web_port: std::sync::atomic::AtomicU16,
    /// This NAS' host name, followed when it changes ([`PcAgent::set_hostname`]).
    hostname: Mutex<String>,
    reg: Mutex<Registry>,
    /// Per group.
    pub live: Mutex<HashMap<String, Live>>,
    /// Per device: the run's progress and the bytes seen at the last speed sample.
    progress: Mutex<HashMap<String, (PcProgress, SpeedSample)>>,
    failures: Mutex<Failures>,
    /// The registry in memory has changes not written yet ([`PcAgent::touch`]).
    dirty: std::sync::atomic::AtomicBool,
    /// Memory the running maintenances and reads expect to use, against the budget.
    pub mem_used: Mutex<u64>,
    /// Woken on every plan or command change (the agents' long polls).
    pub changed: tokio::sync::Notify,
    /// The btrfs method's folders ([`crate::pc_backup`]): PCs with [`PcMethod::Btrfs`] send
    /// into the folder of their name there. Set once at start.
    pub btrfs: std::sync::OnceLock<Arc<crate::pc_backup::PcBackup>>,
    /// The scheduler's history and gate ([`PcAgent::bind_scheduler`]). Set once at start.
    pub scheduler: std::sync::OnceLock<SchedulerLink>,
    /// Per device: the last agent request (ms), every one, in memory only (the registry
    /// records a contact at most once a minute).
    contacts: Mutex<HashMap<String, u64>>,
    /// Per device: when its agent took a sleep or shut-down command (ms, memory only): the
    /// contacts until shortly after it do not prove the PC is still on.
    slept: Mutex<HashMap<String, u64>>,
}

/// Contacts within this span after a taken sleep command do not count ([`PcAgent::last_contact`]):
/// the agent waits 2 s before it suspends, and its next poll may start meanwhile.
const SLEPT_GRACE_MS: u64 = 10_000;

/// A contact counts only when it came after a taken sleep command and its grace span: an
/// earlier one was made by a PC that has gone to sleep since.
fn contact_after_sleep(contact: Option<u64>, slept: Option<u64>) -> Option<u64> {
    contact.filter(|t| slept.is_none_or(|s| *t > s + SLEPT_GRACE_MS))
}

/// What the PC agents share with the scheduler (S6).
pub struct SchedulerLink {
    pub history: Arc<rynas_scheduler::history::History>,
    pub gate: Arc<crate::gate::Gate>,
}

/// The id of a PC's backup plan in the scheduler's history and overview.
pub fn task_id_of(device: &str) -> String {
    format!("pc:{device}")
}

/// Bit set in the revision an agent sees while every task is paused: the plan changes (an
/// agent that compares revisions, any version, takes it), and changes back when resumed.
const PAUSED_ALL_REV: u64 = 1 << 62;

pub type PaState = Option<Arc<PcAgent>>;

/// Bytes done at the last speed sample, and when.
type SpeedSample = Option<(u64, Instant)>;

impl PcAgent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ops: Arc<Ops>,
        notifier: Arc<dyn Notifier>,
        state_dir: PathBuf,
        dry: bool,
        launch: Launch,
        workers: Arc<tokio::sync::Semaphore>,
        tls: Option<Arc<crate::tls::TlsSvc>>,
        hostname: String,
    ) -> Arc<Self> {
        let path = Self::reg_path(&state_dir);
        let mut reg: Registry = match std::fs::read(&path).map(|b| serde_json::from_slice(&b)) {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                // kept aside: the next write would replace it, and it holds the groups'
                // sealed repository passwords
                let aside = path.with_extension(format!("json.unreadable-{}", now_ms()));
                let kept = std::fs::rename(&path, &aside);
                tracing::error!("pc backup: {} does not parse ({e}): starting without PCs; the file is kept as {} ({kept:?})", path.display(), aside.display());
                Registry::default()
            }
            Err(_) => Registry::default(),
        };
        // a share of memory left at 0 by an older version is the default it always meant (the
        // settings shown can then be saved back as they are)
        reg.settings.maintenance_memory_pct = reg.settings.effective_memory_pct();
        Arc::new(PcAgent {
            ops,
            notifier,
            secret_dir: state_dir.join("notify"),
            state_dir,
            dry,
            launch,
            workers,
            streams: Arc::new(tokio::sync::Semaphore::new(MAX_STREAMS)),
            tls,
            web_port: std::sync::atomic::AtomicU16::new(8443),
            hostname: Mutex::new(hostname),
            reg: Mutex::new(reg),
            live: Mutex::new(HashMap::new()),
            progress: Mutex::new(HashMap::new()),
            failures: Mutex::new(Failures::default()),
            dirty: std::sync::atomic::AtomicBool::new(false),
            mem_used: Mutex::new(0),
            changed: tokio::sync::Notify::new(),
            btrfs: std::sync::OnceLock::new(),
            scheduler: std::sync::OnceLock::new(),
            contacts: Mutex::new(HashMap::new()),
            slept: Mutex::new(HashMap::new()),
        })
    }

    /// An agent request of `device` arrived now (memory only, [`Self::last_contact`]).
    pub fn note_contact(&self, device: &str) {
        let now = now_ms();
        let mut c = self.contacts.lock().unwrap_or_else(|p| p.into_inner());
        match c.get_mut(device) {
            Some(t) => *t = now,
            None => {
                c.insert(device.to_string(), now);
            }
        }
    }

    /// The PC's last contact: any agent request since rynasd started (every long poll, at
    /// least every [`MAX_WAIT`] s while the agent runs), else the recorded one.
    pub fn last_contact(&self, device: &str, d: &Dev) -> Option<u64> {
        let mem = self.contacts.lock().unwrap_or_else(|p| p.into_inner()).get(device).copied();
        let slept = self.slept.lock().unwrap_or_else(|p| p.into_inner()).get(device).copied();
        contact_after_sleep(mem.max(d.last_seen_ms), slept)
    }

    /// The agent of `device` took a sleep or shut-down command now.
    pub fn note_slept(&self, device: &str) {
        self.slept.lock().unwrap_or_else(|p| p.into_inner()).insert(device.to_string(), now_ms());
    }

    /// Wires the scheduler (once, at start; S6 of `docs/research/scheduling.md`): the runs
    /// the agents report go into its history (task `pc:<device>`), and its "Pause all"
    /// reaches the agents' plans.
    pub fn bind_scheduler(&self, history: Arc<rynas_scheduler::history::History>, gate: Arc<crate::gate::Gate>) {
        let _ = self.scheduler.set(SchedulerLink { history, gate });
    }

    /// Every scheduled task of the NAS is paused now (Schedule › Pause all).
    pub fn paused_all(&self) -> bool {
        self.scheduler.get().is_some_and(|s| s.gate.pause().all.is_some_and(|a| a.active(now_ms())))
    }

    /// Wires the btrfs method's side (once, at start): PCs of that method send into its
    /// folders, and a folder removed there unbinds its PC here ([`Self::folder_removed`]).
    pub fn bind_btrfs(self: &Arc<Self>, side: Arc<crate::pc_backup::PcBackup>) {
        let me = Arc::downgrade(self);
        let _ = side.on_agent_removed.set(Box::new(move |device, folder| {
            if let Some(a) = me.upgrade() {
                a.folder_removed(device, folder);
            }
        }));
        // the folder's lateness follows its live agent's contacts ([`missed_online`])
        let me = Arc::downgrade(self);
        let _ = side.agent_missed.set(Box::new(move |device| {
            let a = me.upgrade()?;
            let d = a.device(device).ok().filter(|d| !d.revoked && d.settings.method == PcMethod::Btrfs)?;
            Some(missed_online(&d, a.paused_all(), &rynas_power::system_tz(), now_ms()))
        }));
        // the run its agent is sending now: its live progress, under the run's id
        // (`run-<started ms>`)
        let me = Arc::downgrade(self);
        let _ = side.agent_running.set(Box::new(move |device| {
            let a = me.upgrade()?;
            let progress = a.progress.lock().unwrap_or_else(|p| p.into_inner());
            let (p, _) = progress.get(device).filter(|(p, _)| now_ms().saturating_sub(p.updated_ms) < 120_000)?;
            p.run_id.strip_prefix("run-")?.parse().ok()
        }));
        // a new agent folder takes the PC paired under its name (`pcbackup.create`)
        let me = Arc::downgrade(self);
        let _ = side.agent_link.set(Box::new(move |device| {
            let a = me.upgrade().ok_or("rynas-agent's side stopped")?;
            a.link_btrfs(device).map_err(|e| e.2)
        }));
        let _ = self.btrfs.set(side);
    }

    /// Moves the paired PC `device` to the btrfs method, bound to its agent folder (made just
    /// before by `pcbackup.create`): what "Save" with the btrfs method does in the PC's
    /// settings, the other settings kept.
    pub fn link_btrfs(&self, device: &str) -> Res<()> {
        let folder = self.btrfs_side()?.agent_folder_id(device);
        if folder.is_none() {
            return Err(Error::invalid("method", "the PC's folder for rynas-agent is missing"));
        }
        self.edit(|r| {
            let d = r.devices.get_mut(device).ok_or_else(|| not_found(device))?;
            if d.revoked {
                return Err(Error::invalid("device", "this PC's pairing was revoked"));
            }
            if d.settings.method != PcMethod::Btrfs {
                d.rev += 1;
            }
            d.settings.method = PcMethod::Btrfs;
            d.btrfs_folder = folder;
            Ok(())
        })?;
        self.changed.notify_waiters();
        tracing::info!(target: "rynas::audit", "PC {device} moved to its btrfs folder");
        Ok(())
    }

    /// The btrfs method's side, or 409 when it is not running.
    pub fn btrfs_side(&self) -> Res<&Arc<crate::pc_backup::PcBackup>> {
        self.btrfs.get().ok_or_else(|| {
            Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, "the btrfs method is not available on this NAS".into())
        })
    }


    /// `device` has a folder of btrfs snapshots on this NAS (the agent's btrfs method or btrbk
    /// over SSH): a restore can read it.
    pub fn has_btrfs_folder(&self, device: &str) -> bool {
        self.btrfs.get().is_some_and(|b| b.has_target(device))
    }

    pub fn dir(state_dir: &Path) -> PathBuf {
        state_dir.join("pc-agent")
    }

    fn reg_path(state_dir: &Path) -> PathBuf {
        Self::dir(state_dir).join("registry.json")
    }

    pub fn manifest_path(&self, device: &str) -> PathBuf {
        Self::dir(&self.state_dir)
            .join("manifests")
            .join(format!("{device}.json"))
    }

    /// A copy of the registry.
    pub fn snapshot(&self) -> Registry {
        self.reg.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Changes the registry and writes it (the change is kept only when the write works).
    pub fn edit<T>(&self, f: impl FnOnce(&mut Registry) -> Res<T>) -> Res<T> {
        let mut reg = self.reg.lock().unwrap_or_else(|p| p.into_inner());
        let mut next = reg.clone();
        let out = f(&mut next)?;
        let bytes = serde_json::to_vec_pretty(&next).map_err(Error::internal)?;
        let dir = Self::dir(&self.state_dir);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(Error::internal)?;
        crate::tls::write_atomic(&Self::reg_path(&self.state_dir), &bytes, 0o600, None)
            .map_err(Error::internal)?;
        *reg = next;
        // the touches made before are in this write too (same lock)
        self.dirty.store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(out)
    }

    /// Changes the registry in memory only; the change is written by the next [`Self::edit`]
    /// or [`Self::flush`] (within [`FLUSH_EVERY`]). For counters and times whose loss in a
    /// crash costs nothing (bytes uploaded, last seen, a session's sliding end): the hot
    /// paths never rewrite and sync the file.
    pub fn touch<T>(&self, f: impl FnOnce(&mut Registry) -> T) -> T {
        let mut reg = self.reg.lock().unwrap_or_else(|p| p.into_inner());
        let out = f(&mut reg);
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        out
    }

    /// Writes the touched changes, if any (blocking: call it from a blocking thread).
    pub fn flush(&self) {
        if self.dirty.load(std::sync::atomic::Ordering::Relaxed)
            && let Err(e) = self.edit(|_| Ok(()))
        {
            tracing::warn!("pc backup: writing the registry: {}", e.2);
        }
    }

    /// This NAS' host name (the name agents are told at pairing).
    pub fn hostname(&self) -> String {
        self.hostname.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The host name changed (an apply of `system.hostname`).
    pub fn set_hostname(&self, name: &str) {
        *self.hostname.lock().unwrap_or_else(|e| e.into_inner()) = name.to_string();
    }

    /// rynas-web's HTTPS port (`--web-tls-port`), for the addresses agents are told.
    pub fn set_web_port(&self, port: u16) {
        self.web_port.store(port, std::sync::atomic::Ordering::Relaxed);
    }

    /// What agents pin of this NAS (blocking: reads the TLS store and the addresses), or
    /// `None` without the local CA (dry systems).
    pub fn trust(&self) -> Option<AgentTrust> {
        let port = self.web_port.load(std::sync::atomic::Ordering::Relaxed);
        match self.tls.as_ref()?.agent_trust(port) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!("pc backup: the agents' trust: {e}");
                None
            }
        }
    }

    pub fn fingerprint(&self) -> Option<String> {
        let st = self.tls.as_ref()?.status().ok()?;
        Some(st.certificate.fingerprint_sha256)
    }

    pub fn seal(&self, secret: &str) -> Res<String> {
        let sb = crate::ups_netserver::secret_box(&self.secret_dir).map_err(Error::internal)?;
        let mut m = BTreeMap::new();
        m.insert("nas".to_string(), secret.to_string());
        let sealed = sb.seal(&m).map_err(Error::internal)?;
        Ok(base64::engine::general_purpose::STANDARD.encode(sealed))
    }

    pub fn nas_password(&self, g: &Grp) -> Res<String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&g.sealed)
            .map_err(|_| Error::internal("sealed password: not base64"))?;
        let sb = crate::ups_netserver::secret_box(&self.secret_dir).map_err(Error::internal)?;
        let m = sb.open_sealed(&bytes).map_err(|_| {
            Error::internal("the repository password cannot be opened with this NAS' key")
        })?;
        m.get("nas")
            .cloned()
            .ok_or_else(|| Error::internal("sealed password missing"))
    }

    /// The worker request for a group's repository, with the NAS key.
    pub fn request(&self, g: &Grp, op: WorkerOp) -> Res<WorkerRequest> {
        Ok(WorkerRequest {
            repo: RepoSpec {
                repository: g.repo.clone(),
                password: self.nas_password(g)?,
                ..Default::default()
            },
            op,
            cache_dir: None,
        })
    }

    /// Too many wrong codes from this client lately: 429. Many from everybody: the check
    /// waits a moment (60-bit codes make guessing hopeless anyway; the others still get in).
    async fn check_rate(&self, source: &str) -> Res<()> {
        let slow = self.failures.lock().unwrap_or_else(|p| p.into_inner()).check(source);
        match slow {
            Err(()) => Err(Error::new(
                StatusCode::TOO_MANY_REQUESTS,
                error_codes::RATE_LIMITED,
                "too many wrong codes: try again in ten minutes".into(),
            )),
            Ok(true) => {
                tokio::time::sleep(SLOW_CHECK).await;
                Ok(())
            }
            Ok(false) => Ok(()),
        }
    }

    fn failed(&self, source: &str) -> Error {
        self.failures
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .record(source);
        tracing::warn!(target: "rynas::audit", "pc backup: a wrong pairing or restore code");
        Error::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_CREDENTIALS,
            "unknown or expired code".into(),
        )
    }

    /// The view of a group for the console.
    pub fn group_view(&self, name: &str, g: &Grp, reg: &Registry) -> PcGroup {
        let live = self.live.lock().unwrap_or_else(|p| p.into_inner());
        let l = live.get(name);
        let used = l.and_then(|l| l.used).unwrap_or(0);
        let maintaining = l.is_some_and(|l| l.maintaining);
        drop(live);
        let mut maintenance = g.maintenance.clone();
        maintenance.running |= maintaining;
        maintenance.blobs = maint::blobs_estimate(Path::new(&g.repo));
        maintenance.estimate_bytes = maint::prune_estimate(maintenance.blobs);
        PcGroup {
            group: name.to_string(),
            settings: g.settings.clone(),
            repository: g.repo.clone(),
            created_ms: g.created_ms,
            devices: reg
                .devices
                .iter()
                .filter(|(_, d)| d.group == name)
                .map(|(n, _)| n.clone())
                .collect(),
            used_bytes: used,
            maintenance,
            recovery_kit_ms: g.recovery_kit_ms,
            chunk_mib: g.chunk_mib,
        }
    }

    /// The view of a device for the console.
    pub fn view(&self, name: &str, d: &Dev, reg: &Registry) -> PcDevice {
        let progress = self
            .progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .map(|(p, _)| p.clone())
            .filter(|p| now_ms().saturating_sub(p.updated_ms) < 120_000);
        let kit = reg.groups.get(&d.group).and_then(|g| g.recovery_kit_ms);
        let (now, tz, paused_all) = (now_ms(), rynas_power::system_tz(), self.paused_all());
        // a PC of the btrfs method has one set of settings, its folder's: the alert time,
        // retention, the backups held and the restore readiness come from there
        let folder = (d.settings.method == PcMethod::Btrfs).then(|| self.btrfs.get().and_then(|b| b.agent_facts(name))).flatten();
        let d = &with_folder(d, folder.as_ref());
        PcDevice {
            device: name.to_string(),
            description: d.settings.description.clone(),
            hostname: d.hostname.clone(),
            os: d.os.clone(),
            arch: d.arch.clone(),
            agent_version: d.agent_version.clone(),
            root_btrfs: d.root_btrfs,
            created_ms: d.created_ms,
            last_seen_ms: d.last_seen_ms,
            state: state_of(d, progress.is_some(), missed_online(d, paused_all, &tz, now), now),
            next_due_ms: next_due(d, paused_all, &tz, now),
            last_ok_ms: d.last_ok_ms,
            interval_from_ms: last_ok(d).map(|l| l.started_ms),
            last_run: d.history.iter().rev().find(|r| !r.restore).cloned(),
            progress,
            group: d.group.clone(),
            uploaded_bytes: d.uploaded_bytes,
            versions: folder.as_ref().map_or(d.versions, |f| f.runs),
            settings: d.settings.clone(),
            bare_metal: match &folder {
                Some(f) => folder_ready(&f.bare_metal),
                None => ready_of(d, kit),
            },
            // the backup commands only (Run now, Cancel): a power command shows on the
            // Wake-on-LAN page (`WolAgent::pending`)
            command: d.command.clone().filter(|c| !is_power_command(Some(c))),
            command_at_ms: d.command_at_ms.filter(|_| !is_power_command(d.command.as_ref())),
            revoked: d.revoked,
        }
    }

    /// Records progress (and the speed) of a run.
    fn progress(&self, device: &str, s: AgentStatus) {
        let mut all = self.progress.lock().unwrap_or_else(|p| p.into_inner());
        let (old, last) = match all.remove(device) {
            Some((p, l)) if p.run_id == s.run_id => (Some(p), l),
            _ => (None, None),
        };
        let speed = match last {
            Some((b, at)) if s.bytes_done >= b && at.elapsed() > Duration::from_millis(500) => {
                let now = (s.bytes_done - b) as f64 / at.elapsed().as_secs_f64();
                let prev = old.as_ref().map(|p| p.speed_bps as f64).unwrap_or(now);
                (prev * 0.7 + now * 0.3) as u64
            }
            _ => old.as_ref().map(|p| p.speed_bps).unwrap_or(0),
        };
        let last = match last {
            Some((_, at)) if at.elapsed() <= Duration::from_millis(500) => last,
            _ => Some((s.bytes_done, Instant::now())),
        };
        let p = PcProgress {
            run_id: s.run_id,
            phase: s.phase,
            bytes_done: s.bytes_done,
            bytes_total: s.bytes_total,
            files_done: s.files_done,
            files_total: s.files_total,
            speed_bps: speed,
            message: s.message,
            btrfs: s.btrfs,
            updated_ms: now_ms(),
        };
        all.insert(device.to_string(), (p, last));
    }

    /// The run is over: its progress goes.
    pub fn clear_progress(&self, device: &str) {
        self.progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(device);
    }

    /// A PC of `group` reported progress in the last two minutes.
    pub fn group_running(&self, group: &str) -> bool {
        let reg = self.reg.lock().unwrap_or_else(|p| p.into_inner());
        let p = self.progress.lock().unwrap_or_else(|p| p.into_inner());
        reg.devices.iter().any(|(n, d)| {
            d.group == group
                && p.get(n)
                    .is_some_and(|(p, _)| now_ms().saturating_sub(p.updated_ms) < 120_000)
        })
    }

    /// The device a token belongs to (not revoked) and its group.
    pub fn device_of_token(&self, token: &str) -> Option<(String, String)> {
        let h = sha_hex(token.as_bytes());
        let reg = self.reg.lock().unwrap_or_else(|p| p.into_inner());
        reg.devices
            .iter()
            .find(|(_, d)| !d.revoked && !d.token_sha.is_empty() && d.token_sha == h)
            .map(|(n, d)| (n.clone(), d.group.clone()))
    }

    /// The groups a restore session may read.
    pub fn session_groups(&self, token: &str) -> Option<Vec<String>> {
        let devices = self.session_devices(token)?;
        let reg = self.reg.lock().unwrap_or_else(|p| p.into_inner());
        Some(
            devices
                .iter()
                .filter_map(|d| reg.devices.get(d).map(|x| x.group.clone()))
                .collect(),
        )
    }

    /// The devices a restore session may read; the request keeps the session alive
    /// ([`Session::slide`], written within [`FLUSH_EVERY`]).
    pub fn session_devices(&self, token: &str) -> Option<Vec<String>> {
        let h = sha_hex(token.as_bytes());
        let now = now_ms();
        let mut reg = self.reg.lock().unwrap_or_else(|p| p.into_inner());
        let out = reg.sessions.iter_mut().find(|s| s.token_sha == h).and_then(|s| s.slide(now));
        if out.is_some() {
            self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        out
    }

    /// [`PcAgent::read_op`] whose answer is kept per group repository under `key` until the
    /// repository changes (an agent's report, a maintenance operation: [`repo_changed`]).
    pub async fn read_op_cached(self: &Arc<Self>, device: &str, op: WorkerOp, key: &str) -> Res<WorkerEvent> {
        let group = self.device(device)?.group;
        crate::read_cache::cached(&repo_cache_key(&group), key, self.read_op(device, op)).await
    }

    /// One worker read on a device's repository (with a timeout, no task slot), inside the
    /// memory budget and never while the group is maintained.
    pub async fn read_op(self: &Arc<Self>, device: &str, op: WorkerOp) -> Res<WorkerEvent> {
        let d = self.device(device)?;
        let g = self.group(&d.group)?;
        let (_mem, launch) = maint::admit_read(self, &d.group, &g).await?;
        let req = self.request(&g, op)?;
        let limits = Limits {
            timeout: Some(READ_TIMEOUT),
            ..Default::default()
        };
        run_worker(
            &launch,
            &format!("pc-{}", d.group),
            &req,
            &CancellationToken::new(),
            &limits,
            |_| {},
        )
        .await
        .map_err(|e| Error::new(StatusCode::BAD_GATEWAY, error_codes::BACKEND_UNAVAILABLE, e))
    }

    pub fn device(&self, device: &str) -> Res<Dev> {
        self.reg
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .devices
            .get(device)
            .cloned()
            .ok_or_else(|| not_found(device))
    }

    pub fn group(&self, group: &str) -> Res<Grp> {
        self.reg
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .groups
            .get(group)
            .cloned()
            .ok_or_else(|| group_not_found(group))
    }

    fn manifest(&self, device: &str) -> Option<Manifest> {
        let b = std::fs::read(self.manifest_path(device)).ok()?;
        serde_json::from_slice(&b).ok()
    }
}

/// The [`crate::read_cache`] key of a group's repository.
pub(crate) fn repo_cache_key(group: &str) -> String {
    format!("pc:{group}")
}

/// Something may change the group's repository (an agent's run ended, a maintenance
/// operation): its kept read answers go.
pub(crate) fn repo_changed(group: &str) {
    crate::read_cache::global().invalidate(&repo_cache_key(group));
}

fn not_found(device: &str) -> Error {
    Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, format!("no PC {device}"))
}

fn group_not_found(group: &str) -> Error {
    Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, format!("no group {group}"))
}

/// The console state of a device. `missed`: a scheduled backup was due while the PC was
/// online and none ran ([`missed_online`]): only then is it late, critical once its last good
/// version (or the pairing) is older than `critical_hours`. A PC that is off keeps the state
/// its last backup left (the console says how long it has been off).
pub fn state_of(d: &Dev, running: bool, missed: bool, now: u64) -> PcState {
    if running {
        return PcState::Running;
    }
    let hours_since = |ms: u64| now.saturating_sub(ms) / 3_600_000;
    let critical = |since: u64| d.settings.alert_hours > 0 && d.settings.critical_hours > 0 && hours_since(since) >= u64::from(d.settings.critical_hours);
    let late = |since: u64| if critical(since) { PcState::Critical } else { PcState::Late };
    let Some(ok) = d.last_ok_ms else {
        // never a good version: late only counted from the pairing
        if d.history.iter().any(|r| !r.restore && !r.ok) {
            return PcState::Failed;
        }
        return if missed { late(d.created_ms) } else { PcState::Waiting };
    };
    match d.history.iter().rev().find(|r| !r.restore) {
        Some(r) if !r.ok => PcState::Failed,
        _ if missed => late(ok),
        _ => PcState::Ok,
    }
}

/// No contact for this long and the PC counts as off (asleep, away): the agent asks for its
/// plan at least every 50 s and the NAS writes the contact once a minute, so about three
/// missed contacts. The console uses the same span.
pub const OFFLINE_AFTER_MS: u64 = 5 * 60_000;

/// How long after the moment the agent would have started a due backup (its due time, the
/// delay after the PC came online, the allowed window) the PC must still have been online for
/// the backup to count as missed: the start itself is at once, so this covers the clocks of
/// the PC and the NAS disagreeing, a slow first contact and the minute between contacts.
pub const LATE_MARGIN_MS: u64 = 30 * 60_000;

impl Dev {
    /// A contact of the PC at `seen`: a new stretch online after more than
    /// [`OFFLINE_AFTER_MS`] without one (or when none is known yet).
    pub fn contact(&mut self, seen: u64) {
        if self.online_since_ms.is_none() || self.last_seen_ms.is_none_or(|t| seen.saturating_sub(t) > OFFLINE_AFTER_MS) {
            self.online_since_ms = Some(seen);
        }
        self.last_seen_ms = Some(seen);
    }
}

/// The schedule this device's agent follows, as its version reads the plan the NAS sends
/// ([`plan_at`]): before 0.8.20 only the two old fields (every so many hours, the delay
/// after the start: no window, no set times, nothing at every start); 0.8.20 the interval and
/// the window. `None` while paused (also by Pause all) or revoked.
fn followed(d: &Dev, paused_all: bool, tz: &jiff::tz::TimeZone, now: u64) -> Option<(AgentSchedule, Option<rynas_schedule::CalendarSpec>)> {
    if d.revoked || d.settings.plan.paused || paused_all {
        return None;
    }
    let mut s = plan_at(d, false, tz, now).schedule();
    if !agent_follows_window(&d.agent_version) {
        s.window = None;
        s.calendar = None;
    }
    if !agent_follows_calendar(&d.agent_version) {
        s.at_boot = false;
    }
    let spec = s.calendar.as_deref().and_then(|c| rynas_schedule::CalendarSpec::parse(c).ok());
    Some((s, spec))
}

/// A btrfs PC as its folder rules it ([`crate::pc_backup::PcBackup::agent_facts`]), the one set
/// of settings of such a PC: the folder's alert time (late when a backup was missed while the
/// PC was on, as for every PC; the folder notifies once nothing arrived for that long; no
/// critical tier), its retention and minimum age. The record's own values stay stored,
/// unused. Without a folder: the record as it is.
fn with_folder<'a>(d: &'a Dev, folder: Option<&crate::pc_backup::AgentFolder>) -> std::borrow::Cow<'a, Dev> {
    let Some(f) = folder else { return std::borrow::Cow::Borrowed(d) };
    let mut x = d.clone();
    x.settings.alert_hours = f.alert_hours;
    x.settings.critical_hours = 0;
    x.settings.retention = rynas_api_types::backup::Retention {
        last: f.keep_last,
        hourly: f.retention.hourly,
        daily: f.retention.daily,
        weekly: f.retention.weekly,
        monthly: f.retention.monthly,
        yearly: f.retention.yearly,
    };
    x.settings.min_age_days = f.min_age_days;
    std::borrow::Cow::Owned(x)
}

/// A btrfs folder's restore readiness in the agent record's terms (the same answer in both
/// views of the PC).
fn folder_ready(b: &rynas_api_types::pcbackup::BareMetal) -> BareMetalReady {
    use rynas_api_types::pcbackup::BareMetalCode as C;
    let mut reasons = Vec::new();
    for g in &b.missing {
        let r = match g.code {
            C::NoSnapshots => ReadyReason::NoVersion,
            C::SubvolumeMissing => ReadyReason::VolumeSkipped,
            C::LoaderUnknown => ReadyReason::LoaderUnknown,
            C::ManifestStale => ReadyReason::ManifestStale,
            _ => ReadyReason::NoManifest,
        };
        if !reasons.contains(&r) {
            reasons.push(r);
        }
    }
    BareMetalReady { ready: b.ready, reasons, details: Vec::new() }
}

/// The device's last good run as its agent counts the next one from it: the interval from the
/// run's start (agents from [`rynas_api_types::pcagent::START_ANCHORED_AGENT`]), else from its
/// end, as an older agent does; a calendar always from the end.
pub fn last_ok(d: &Dev) -> Option<rynas_schedule::window::LastOk> {
    let start = d.last_ok_start_ms.filter(|_| rynas_api_types::pcagent::agent_counts_from_start(&d.agent_version));
    d.last_ok_ms.map(|end| rynas_schedule::window::LastOk::new(start, end))
}

/// A scheduled backup of the device was due while the PC was online and none ran: the
/// agent would have started it ([`rynas_schedule::window::agent_due`] from the last good run,
/// at least the plan's delay after the PC came online, then [`start_at`] for the allowed
/// window, which may push it past a closed window) and the PC was still seen
/// [`LATE_MARGIN_MS`] after that, in the same stretch online. Nothing is missed while paused
/// or while the PC is off.
///
/// "Also at every start of the PC" is not judged here: the NAS cannot tell a boot from a
/// resume after sleep or a network that came back (the agent runs it only after a boot), so
/// only the interval or the set times count.
///
/// [`start_at`]: rynas_schedule::window::start_at
pub fn missed_online(d: &Dev, paused_all: bool, tz: &jiff::tz::TimeZone, now: u64) -> bool {
    let Some((s, spec)) = followed(d, paused_all, tz, now) else { return false };
    let (Some(seen), Some(since)) = (d.last_seen_ms, d.online_since_ms) else { return false };
    let due = rynas_schedule::window::agent_due(
        spec.as_ref(),
        u64::from(s.every_h.max(1)) * 3_600_000,
        tz,
        last_ok(d),
        since,
        u64::from(s.delay_min) * 60_000,
        None,
    );
    if due == u64::MAX {
        return false;
    }
    rynas_schedule::window::start_at(s.window.as_deref(), tz, due).is_some_and(|start| seen >= start.saturating_add(LATE_MARGIN_MS))
}

/// When the device's next scheduled backup is due, for the console ([`PcDevice::next_due_ms`]):
/// the due time from the last good run when it has passed (missed while the PC was off: it
/// catches up once, soon after the PC starts), else the next start inside the allowed window.
pub fn next_due(d: &Dev, paused_all: bool, tz: &jiff::tz::TimeZone, now: u64) -> Option<u64> {
    let (s, spec) = followed(d, paused_all, tz, now)?;
    let due = rynas_schedule::window::agent_due(spec.as_ref(), u64::from(s.every_h.max(1)) * 3_600_000, tz, last_ok(d), 0, 0, None);
    if due == u64::MAX {
        return None;
    }
    if due <= now {
        return Some(due);
    }
    rynas_schedule::window::start_at(s.window.as_deref(), tz, due)
}

/// "Bare-metal ready": a version, a recent manifest without problems, a recovery kit of the
/// group (`kit`).
pub fn ready_of(d: &Dev, kit: Option<u64>) -> BareMetalReady {
    let mut reasons = Vec::new();
    if d.last_ok_ms.is_none() {
        reasons.push(ReadyReason::NoVersion);
    }
    match d.manifest_ms {
        None => reasons.push(ReadyReason::NoManifest),
        Some(m) => {
            if d.last_ok_ms.is_some_and(|ok| ok > m.saturating_add(86_400_000)) {
                reasons.push(ReadyReason::ManifestStale);
            }
            reasons.extend(d.manifest_reasons.iter().copied());
        }
    }
    if kit.is_none() {
        reasons.push(ReadyReason::NoRecoveryKit);
    }
    reasons.dedup();
    let details = if reasons.contains(&ReadyReason::ManualLayout) { d.manifest_details.clone() } else { Vec::new() };
    BareMetalReady {
        ready: reasons.is_empty(),
        reasons,
        details,
    }
}

// ------------------------------------------------------------------ helpers of the routes

fn pa(st: &PaState) -> Res<Arc<PcAgent>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "PC backup is not enabled in this rynasd".into(),
        )
    })
}

fn audit(who: &str, action: &str, target: &str) {
    tracing::info!(target: "rynas::audit", "pc backup {action} {target} by {who}");
}

fn json_body<T>(b: Result<Json<T>, JsonRejection>) -> Res<T> {
    Ok(b?.0)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Res<T> {
    tokio::task::spawn_blocking(f).await.map_err(Error::internal)
}

fn check_name(d: &str) -> Res<()> {
    if valid_device(d) {
        Ok(())
    } else {
        Err(Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, "no such PC or group".into()))
    }
}

/// The bearer token of an agent request (`Authorization: Bearer`, or the password of Basic).
pub fn bearer(h: &HeaderMap) -> Option<String> {
    let v = h.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    if let Some(t) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")) {
        return Some(t.trim().to_string());
    }
    let b = v.strip_prefix("Basic ").or_else(|| v.strip_prefix("basic "))?;
    let raw = base64::engine::general_purpose::STANDARD.decode(b.trim()).ok()?;
    let s = String::from_utf8(raw).ok()?;
    let (_, pw) = s.split_once(':')?;
    Some(pw.to_string())
}

/// The device of an agent request's token, or 401.
fn agent_device(svc: &PcAgent, h: &HeaderMap) -> Res<String> {
    let d = bearer(h)
        .and_then(|t| svc.device_of_token(&t))
        .map(|(d, _)| d)
        .ok_or_else(|| {
            Error::new(StatusCode::UNAUTHORIZED, error_codes::UNAUTHENTICATED, "unknown device token".into())
        })?;
    svc.note_contact(&d);
    Ok(d)
}

pub fn router(st: PaState) -> Router {
    let admins = st.as_ref().map(|s| s.ops.admins.clone()).unwrap_or_default();
    let console = Router::new()
        .route("/v1/pcbackup", get(overview))
        .route("/v1/pcbackup/settings", put(settings_put))
        .route("/v1/pcbackup/pairings", post(pairing_post))
        .route("/v1/pcbackup/pairings/{id}", delete(pairing_delete))
        .route("/v1/pcbackup/groups/{group}", put(group_put).delete(group_delete))
        .route("/v1/pcbackup/groups/{group}/maintenance", post(maintenance_post))
        .route("/v1/pcbackup/groups/{group}/recovery-kit", post(kit_post))
        .route(
            "/v1/pcbackup/devices/{device}",
            get(detail_get).put(device_put).delete(device_delete),
        )
        .route("/v1/pcbackup/devices/{device}/backup-now", post(backup_now))
        .route("/v1/pcbackup/devices/{device}/cancel", post(cancel_post))
        .route("/v1/pcbackup/devices/{device}/versions", get(versions_get))
        .route("/v1/pcbackup/devices/{device}/browse", get(browse_get))
        .route("/v1/pcbackup/devices/{device}/download", get(download_get))
        .route("/v1/pcbackup/devices/{device}/zip", get(zip_get))
        .route("/v1/pcbackup/devices/{device}/restore-code", post(restore_code_post))
        .route("/v1/pcbackup/restore-choice", post(restore_choice_post))
        .with_state(st.clone())
        .route_layer(axum::middleware::from_fn_with_state(
            crate::roles::AdminGate {
                policy: admins,
                what: "PC backup",
            },
            crate::roles::admin_only,
        ));
    let agent = Router::new()
        .route("/v1/pcbackup/agent/pair", post(pair_post))
        .route("/v1/pcbackup/agent/plan", get(plan_get))
        .route("/v1/pcbackup/agent/trust", get(trust_get))
        .route("/v1/pcbackup/agent/status", post(status_post))
        .route("/v1/pcbackup/agent/report", post(report_post))
        .route("/v1/pcbackup/agent/facts", post(facts_post))
        .route("/v1/pcbackup/agent/btrfs/snapshots", get(btrfs_snapshots_get))
        .route("/v1/pcbackup/agent/btrfs/receive", post(btrfs_receive_post))
        .route("/v1/pcbackup/agent/restore/login", post(restore_login))
        .route("/v1/pcbackup/agent/restore/report", post(restore_report))
        .route("/v1/pcbackup/agent/restore/btrfs/{device}/snapshots", get(restore_btrfs_snapshots))
        .route("/v1/pcbackup/agent/restore/btrfs/{device}/manifest", get(restore_btrfs_manifest))
        .route("/v1/pcbackup/agent/restore/btrfs/{device}/legacy", get(restore_btrfs_legacy))
        .route("/v1/pcbackup/agent/restore/btrfs/{device}/send", get(restore_btrfs_send))
        .route("/v1/pcbackup/repo/{group}", post(repo::create_refused))
        .route("/v1/pcbackup/repo/{group}/", post(repo::create_refused))
        .route("/v1/pcbackup/repo/{group}/{*rest}", axum::routing::any(repo::handle))
        .with_state(st);
    console.merge(agent)
}

// ------------------------------------------------------------------ console

async fn overview(State(st): State<PaState>) -> Res<Json<PcBackupOverview>> {
    let svc = pa(&st)?;
    let s2 = svc.clone();
    blocking(move || {
        let reg = s2.snapshot();
        let now = now_ms();
        maint::count_used(&s2, &reg);
        PcBackupOverview {
            settings: reg.settings.clone(),
            groups: reg.groups.iter().map(|(n, g)| s2.group_view(n, g, &reg)).collect(),
            devices: reg.devices.iter().map(|(n, d)| s2.view(n, d, &reg)).collect(),
            pairings: reg
                .pairings
                .iter()
                .filter(|p| p.expires_ms > now)
                .map(|p| PairingView {
                    id: p.id.clone(),
                    description: p.description.clone(),
                    group: p.group.clone(),
                    expires_ms: p.expires_ms,
                    btrfs_target: p.btrfs_target.clone(),
                })
                .collect(),
            memory_budget_bytes: maint::memory_budget(reg.settings.maintenance_memory_pct),
            fingerprint: s2.fingerprint(),
            dry: s2.dry,
        }
    })
    .await
    .map(Json)
}

/// The folder of the settings: absolute, normalized, inside a volume or the system folder.
pub fn check_folder(folder: &str, places: &[(String, String)]) -> Result<(), String> {
    if folder.is_empty() {
        return Ok(());
    }
    if !folder.starts_with('/')
        || folder.ends_with('/')
        || folder.contains("//")
        || folder.split('/').any(|p| p == ".." || p == ".")
        || folder.chars().any(|c| c.is_control() || c == '"' || c == '\\')
    {
        return Err("the folder must be an absolute, normalized path".into());
    }
    let inside = places
        .iter()
        .any(|(_, dir)| folder.starts_with(&format!("{}/", dir.trim_end_matches('/'))));
    if !inside {
        return Err("the folder must be inside a volume or the system folder".into());
    }
    Ok(())
}

async fn settings_put(
    State(st): State<PaState>,
    h: HeaderMap,
    body: Result<Json<PcAgentSettings>, JsonRejection>,
) -> Res<StatusCode> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    let mut s = json_body(body)?;
    if s.maintenance_hour > 23 {
        return Err(Error::invalid("maintenance_hour", "0–23"));
    }
    // 0 (what older versions showed) is the default share
    s.maintenance_memory_pct = s.effective_memory_pct();
    if !(10..=80).contains(&s.maintenance_memory_pct) {
        return Err(Error::invalid("maintenance_memory_pct", "10–80 %"));
    }
    let folder = s.folder.trim().to_string();
    if svc.dry {
        if !folder.is_empty() && !folder.starts_with('/') {
            return Err(Error::invalid("folder", "the folder must be an absolute path"));
        }
    } else {
        let places = crate::places::places_in(&crate::places::mountinfo());
        check_folder(&folder, &places).map_err(|m| Error::invalid("folder", m))?;
    }
    let hour = s.maintenance_hour;
    svc.edit(|r| {
        // existing repositories stay where they are; only new groups use a new folder
        r.settings = PcAgentSettings {
            folder: folder.clone(),
            ..s
        };
        Ok(())
    })?;
    // the nightly maintenance is the scheduler task pc-maintenance: its hour goes live too
    let (cfg, w) = (svc.ops.config.clone(), who.clone());
    tokio::task::spawn_blocking(move || crate::system_tasks::apply_pc(&cfg, &w, hour))
        .await
        .map_err(Error::internal)?
        .map_err(Error::internal)?;
    audit(&who, "settings", &folder);
    Ok(StatusCode::NO_CONTENT)
}

async fn pairing_post(
    State(st): State<PaState>,
    h: HeaderMap,
    body: Result<Json<PairingRequest>, JsonRejection>,
) -> Res<Response> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    let req = json_body(body)?;
    if req.description.chars().count() > 120 {
        return Err(Error::invalid("description", "at most 120 characters"));
    }
    if let Some(g) = &req.group
        && req.btrfs_target.is_none()
        && svc.group(g).is_err()
    {
        return Err(Error::invalid("group", "no such group"));
    }
    // a PC for the btrfs method: its folder must exist (made with `agent`), and the name
    // must still be free here
    if let Some(t) = &req.btrfs_target {
        if !svc.btrfs_side()?.has_agent_target(t) {
            return Err(Error::invalid("btrfs_target", "no folder for rynas-agent with this name"));
        }
        // a PC of the btrfs method already there may be paired again (agent reinstalled, new
        // system, lost token): it keeps its folder, history and settings, only the token changes
        if svc.snapshot().devices.get(t).is_some_and(|d| !d.group.is_empty()) {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::ALREADY_EXISTS,
                format!("a PC named {t} is already paired: switch its method instead"),
            ));
        }
    }
    let code = new_code()?;
    let id = new_token()?[..12].to_string();
    let expires_ms = now_ms() + PAIRING_TTL_MS;
    svc.edit(|r| {
        if r.settings.folder.is_empty() && req.btrfs_target.is_none() {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::CONFLICT,
                "choose the folder of the PC backups first".into(),
            ));
        }
        let now = now_ms();
        r.pairings.retain(|p| p.expires_ms > now);
        r.pairings.push(Pairing {
            id: id.clone(),
            code_sha: code_hash(&code),
            description: req.description.trim().to_string(),
            group: if req.btrfs_target.is_some() { None } else { req.group.clone() },
            quota_gib: req.quota_gib,
            expires_ms,
            btrfs_target: req.btrfs_target.clone(),
        });
        Ok(())
    })?;
    audit(&who, "pairing", &id);
    let fingerprint = svc.fingerprint();
    let body = PairingCreated {
        id: id.clone(),
        command: pair_command(&code, fingerprint.as_deref(), req.btrfs_target.is_some()),
        code,
        expires_ms,
        fingerprint,
    };
    Ok(crate::error::created("/api/v1/pcbackup/pairings", &id, body))
}

/// The command the console shows for a pairing (`{url}` is filled in by the browser): with
/// the certificate's fingerprint, so the agent pins the NAS' certificate without asking and
/// nobody in the path can pose as the NAS. A pairing of the btrfs method adds `--btrfs`: the
/// agent then skips its question for a btrfs root, which offers the btrfs method instead of
/// the file backup (an older agent ignores the flag and still asks).
pub fn pair_command(code: &str, fingerprint: Option<&str>, btrfs: bool) -> String {
    let mut cmd = match fingerprint.filter(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_hexdigit() || b == b':')) {
        Some(f) => format!("sudo rynas-agent pair {{url}} {code} --fingerprint {f}"),
        None => format!("sudo rynas-agent pair {{url}} {code}"),
    };
    if btrfs {
        cmd.push_str(" --btrfs");
    }
    cmd
}

async fn pairing_delete(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Res<StatusCode> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    svc.edit(|r| {
        r.pairings.retain(|p| p.id != id);
        Ok(())
    })?;
    audit(&who, "pairing removed", &id);
    Ok(StatusCode::NO_CONTENT)
}

async fn detail_get(
    State(st): State<PaState>,
    UrlPath(device): UrlPath<String>,
) -> Res<Json<PcDeviceDetail>> {
    let svc = pa(&st)?;
    check_name(&device)?;
    let s2 = svc.clone();
    blocking(move || {
        let d = s2.device(&device)?;
        let reg = s2.snapshot();
        maint::count_used(&s2, &reg);
        Ok(PcDeviceDetail {
            device: s2.view(&device, &d, &reg),
            history: d.history.iter().rev().cloned().collect(),
            manifest: s2.manifest(&device),
        })
    })
    .await?
    .map(Json)
}

pub fn check_settings(s: &PcDeviceSettings) -> Res<()> {
    if !(1..=168).contains(&s.plan.interval_hours) {
        return Err(Error::invalid("plan.interval_hours", "1–168 hours"));
    }
    if s.plan.start_delay_min > 1440 {
        return Err(Error::invalid("plan.start_delay_min", "at most 1440 minutes"));
    }
    if !(5..=3600).contains(&s.plan.freeze_max_secs) {
        return Err(Error::invalid("plan.freeze_max_secs", "5–3600 seconds"));
    }
    if s.plan.window.as_deref().is_some_and(|w| !rynas_api_types::scheduler::week_window_valid(w)) {
        return Err(Error::invalid("plan.window", "168 cells of 0 or 1 with at least one 1"));
    }
    // set times: a calendar expression systemd would take, with a time left to run at
    if let Some(c) = &s.plan.calendar {
        let c = c.trim();
        if c.is_empty() || c.len() > 256 {
            return Err(Error::invalid("plan.calendar", "a calendar expression"));
        }
        rynas_scheduler::calendar::plausible_calendar(c).map_err(|e| Error::invalid("plan.calendar", e))?;
        // the agent's own parser (what it can read is what is stored)
        let spec = rynas_schedule::CalendarSpec::parse(c).map_err(|e| Error::invalid("plan.calendar", e.to_string()))?;
        // the times are the NAS's (sent with its zone): a zone of its own would need the
        // zone database on every PC
        if spec.timezone.is_some() {
            return Err(Error::invalid("plan.calendar", "no time zone in the expression: the times are the NAS's"));
        }
        let tz = rynas_power::system_tz();
        let now = now_ms();
        if spec.next_after(&tz, jiff::Timestamp::now()).is_none() {
            return Err(Error::invalid("plan.calendar", "the expression never runs again"));
        }
        // backups, not a busy loop (and room for the repository's maintenance)
        if rynas_schedule::window::calendar_min_gap_ms(&spec, &tz, now).is_some_and(|g| g < 3_600_000) {
            return Err(Error::invalid("plan.calendar", "set times at least an hour apart"));
        }
    }
    if s.plan.excludes.len() > 200
        || s
            .plan
            .excludes
            .iter()
            .any(|e| e.trim().is_empty() || e.len() > 512 || e.chars().any(char::is_control))
    {
        return Err(Error::invalid("plan.excludes", "up to 200 non-empty patterns"));
    }
    if s.retention.is_empty() {
        return Err(Error::invalid("retention", "keep at least one version"));
    }
    if s.min_age_days > 3650 {
        return Err(Error::invalid("min_age_days", "at most 3650 days"));
    }
    if s.critical_hours > 0 && s.alert_hours > s.critical_hours {
        return Err(Error::invalid("critical_hours", "not earlier than the late alert"));
    }
    if s.description.chars().count() > 120 {
        return Err(Error::invalid("description", "at most 120 characters"));
    }
    Ok(())
}

async fn device_put(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(device): UrlPath<String>,
    body: Result<Json<PcDeviceSettings>, JsonRejection>,
) -> Res<Json<PcDevice>> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    check_name(&device)?;
    let mut s = json_body(body)?;
    s.plan.excludes = s.plan.excludes.iter().map(|e| e.trim().to_string()).collect();
    s.description = s.description.trim().to_string();
    check_settings(&s)?;
    // the btrfs method sends into the folder of the PC's name (made with `agent` on the
    // btrfs side first); the files method needs the repository the pairing made
    let folder = if s.method == PcMethod::Btrfs {
        let f = svc.btrfs_side()?.agent_folder_id(&device);
        if f.is_none() {
            return Err(Error::invalid("method", "make the PC's folder for rynas-agent on the btrfs side first"));
        }
        f
    } else {
        None
    };
    let d = svc.edit(|r| {
        let d = r.devices.get_mut(&device).ok_or_else(|| not_found(&device))?;
        if s.method == PcMethod::Files && d.group.is_empty() {
            return Err(Error::invalid("method", "this PC was paired for btrfs snapshots only (no repository)"));
        }
        // choosing the btrfs method binds the PC to the folder there now; saving other
        // settings keeps the binding (a folder made again is bound only by choosing again)
        if s.method == PcMethod::Btrfs && (d.settings.method != PcMethod::Btrfs || d.btrfs_folder.is_none()) {
            d.btrfs_folder = folder;
        }
        if d.settings.plan != s.plan || d.settings.method != s.method {
            d.rev += 1;
        }
        // the btrfs method's retention and alerts are the folder's: what the console sent
        // back for them (the folder's, shown in the record) never replaces the record's own
        if s.method == PcMethod::Btrfs {
            s.retention = d.settings.retention;
            s.min_age_days = d.settings.min_age_days;
            s.alert_hours = d.settings.alert_hours;
            s.critical_hours = d.settings.critical_hours;
        }
        d.settings = s;
        Ok(d.clone())
    })?;
    svc.changed.notify_waiters();
    audit(&who, "settings", &device);
    // the view reads the btrfs folder of such a PC: off the async threads
    let s2 = svc.clone();
    blocking(move || s2.view(&device, &d, &s2.snapshot())).await.map(Json)
}

async fn group_put(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(group): UrlPath<String>,
    body: Result<Json<PcGroupSettings>, JsonRejection>,
) -> Res<Json<PcGroup>> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    check_name(&group)?;
    let mut s = json_body(body)?;
    s.description = s.description.trim().to_string();
    if s.description.chars().count() > 120 {
        return Err(Error::invalid("description", "at most 120 characters"));
    }
    svc.edit(|r| {
        let g = r.groups.get_mut(&group).ok_or_else(|| group_not_found(&group))?;
        g.settings = s;
        Ok(())
    })?;
    audit(&who, "group settings", &group);
    let reg = svc.snapshot();
    let g = reg.groups.get(&group).ok_or_else(|| group_not_found(&group))?;
    Ok(Json(svc.group_view(&group, g, &reg)))
}

/// Deletes a group with its repository: only when every PC in it is revoked.
async fn group_delete(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(group): UrlPath<String>,
) -> Res<Response> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    check_name(&group)?;
    let reg = svc.snapshot();
    let g = reg.groups.get(&group).cloned().ok_or_else(|| group_not_found(&group))?;
    if reg.devices.values().any(|d| d.group == group && !d.revoked) {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            "revoke every PC of the group first".into(),
        ));
    }
    // the group is taken like a maintenance (no upload, no maintenance, no read meanwhile),
    // and the pairings that would join it go now
    let guard = maint::take(&svc, &group).map_err(|e| Error::new(StatusCode::CONFLICT, error_codes::BUSY, e))?;
    svc.edit(|r| {
        r.pairings.retain(|p| p.group.as_deref() != Some(group.as_str()));
        Ok(())
    })?;
    audit(&who, "delete group", &group);
    let s2 = svc.clone();
    let id = svc.ops.tasks.spawn("pcbackup.delete-group", &who, move |ctx| async move {
        let _guard = guard;
        maint::delete_group(&s2, &group, &g, &ctx).await
    })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

/// Revokes a PC: its token stops now, its key file goes; its versions stay readable in the
/// group until the group is deleted.
async fn device_delete(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(device): UrlPath<String>,
) -> Res<Response> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    check_name(&device)?;
    let d = svc.edit(|r| {
        let d = r.devices.get_mut(&device).ok_or_else(|| not_found(&device))?;
        d.token_sha.clear();
        d.revoked = true;
        d.command = None;
        Ok(d.clone())
    })?;
    svc.clear_progress(&device);
    svc.changed.notify_waiters();
    audit(&who, "revoke", &device);
    let s2 = svc.clone();
    let id = svc.ops.tasks.spawn("pcbackup.revoke", &who, move |ctx| async move {
        maint::revoke(&s2, &device, &d, &ctx).await
    })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

fn queue(svc: &PcAgent, device: &str, cancel: bool) -> Res<()> {
    svc.edit(|r| {
        let seq = r.seq + 1;
        r.seq = seq;
        let d = r.devices.get_mut(device).ok_or_else(|| not_found(device))?;
        if d.revoked {
            return Err(Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, "the PC is revoked".into()));
        }
        // a "Back up now" the PC has not taken yet (no contact since: off, asleep): Cancel
        // takes it back instead of queueing a cancel behind it
        let untaken = matches!(d.command, Some(AgentCommand::BackupNow { .. }))
            && d.command_at_ms.is_some_and(|c| d.last_seen_ms.is_none_or(|t| t < c));
        if cancel && untaken {
            d.command = None;
            d.command_at_ms = None;
            d.rev += 1;
            return Ok(());
        }
        d.command = Some(if cancel {
            AgentCommand::Cancel { id: seq }
        } else {
            AgentCommand::BackupNow { id: seq }
        });
        d.command_at_ms = Some(now_ms());
        d.rev += 1;
        Ok(())
    })?;
    svc.changed.notify_waiters();
    Ok(())
}

async fn backup_now(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(device): UrlPath<String>,
) -> Res<StatusCode> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    check_name(&device)?;
    queue(&svc, &device, false)?;
    audit(&who, "back up now", &device);
    Ok(StatusCode::NO_CONTENT)
}

async fn cancel_post(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(device): UrlPath<String>,
) -> Res<StatusCode> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    check_name(&device)?;
    queue(&svc, &device, true)?;
    audit(&who, "cancel", &device);
    Ok(StatusCode::NO_CONTENT)
}

async fn maintenance_post(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(group): UrlPath<String>,
) -> Res<Response> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    check_name(&group)?;
    let id = maint::start(&svc, &group, &who, false)?;
    audit(&who, "maintenance", &group);
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

/// The versions of one PC inside its group.
pub fn scope(device: &str) -> SnapshotScope {
    SnapshotScope {
        tag: device_tag(device),
        ..Default::default()
    }
}

async fn versions_get(
    State(st): State<PaState>,
    UrlPath(device): UrlPath<String>,
) -> Res<Json<Vec<SnapshotEntry>>> {
    let svc = pa(&st)?;
    check_name(&device)?;
    match svc
        .read_op_cached(&device, WorkerOp::Snapshots { scope: Some(scope(&device)) }, &format!("versions:{device}"))
        .await?
    {
        WorkerEvent::Snapshots { list } => {
            let n = list.len() as u32;
            svc.touch(|r| {
                if let Some(d) = r.devices.get_mut(&device) {
                    d.versions = n;
                }
            });
            Ok(Json(list))
        }
        other => Err(Error::internal(format!("unexpected worker answer {other:?}"))),
    }
}

#[derive(Deserialize)]
struct BrowseQuery {
    version: String,
    #[serde(default)]
    path: Option<String>,
}

fn check_version(v: &str) -> Res<()> {
    if v == "latest" || (v.len() >= 8 && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_hexdigit())) {
        Ok(())
    } else {
        Err(Error::invalid("version", "not a version id"))
    }
}

fn check_inner(p: &str) -> Res<()> {
    if !p.starts_with('/')
        || p.split('/').any(|s| s == "..")
        || p.chars().any(char::is_control)
        || p.len() > 4096
    {
        return Err(Error::invalid("path", "not a path inside the version"));
    }
    Ok(())
}

/// `version=` and several `path=` parameters (axum's `Query` keeps one value per key).
fn paths_of(raw: Option<&str>) -> Res<(String, Vec<String>)> {
    let mut version = String::new();
    let mut paths = Vec::new();
    for pair in raw.unwrap_or("").split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = percent_decode(v).ok_or_else(|| Error::bad_request("bad query string"))?;
        match k {
            "version" => version = v,
            "path" => paths.push(v),
            _ => {}
        }
    }
    check_version(&version)?;
    if paths.is_empty() || paths.len() > 1000 {
        return Err(Error::invalid("path", "name 1–1000 paths"));
    }
    for p in &paths {
        check_inner(p)?;
    }
    Ok((version, paths))
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

async fn browse_get(
    State(st): State<PaState>,
    UrlPath(device): UrlPath<String>,
    Query(q): Query<BrowseQuery>,
) -> Res<Json<Vec<NodeEntry>>> {
    let svc = pa(&st)?;
    check_name(&device)?;
    check_version(&q.version)?;
    let path = q.path.unwrap_or_else(|| "/".into());
    check_inner(&path)?;
    let key = format!("ls:{device}:{}:{path}", q.version);
    match svc
        .read_op_cached(
            &device,
            WorkerOp::Ls {
                snapshot: q.version,
                path,
                scope: Some(scope(&device)),
            },
            &key,
        )
        .await?
    {
        WorkerEvent::Entries { mut list } => {
            list.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
            Ok(Json(list))
        }
        other => Err(Error::internal(format!("unexpected worker answer {other:?}"))),
    }
}

async fn download_get(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(device): UrlPath<String>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Res<Response> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    check_name(&device)?;
    let (version, paths) = paths_of(raw.as_deref())?;
    let path = paths[0].clone();
    let name = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("file").to_string();
    audit(&who, "download", &format!("{device}:{path}"));
    maint::stream(&svc, &device, version, vec![path], false, &name).await
}

async fn zip_get(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(device): UrlPath<String>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Res<Response> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    check_name(&device)?;
    let (version, paths) = paths_of(raw.as_deref())?;
    let name = if paths.len() == 1 {
        let base = paths[0].rsplit('/').find(|s| !s.is_empty()).unwrap_or(&device);
        format!("{base}.zip")
    } else {
        format!("{device}-files.zip")
    };
    audit(&who, "zip", &format!("{device}: {} paths", paths.len()));
    maint::stream(&svc, &device, version, paths, true, &name).await
}

async fn kit_post(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(group): UrlPath<String>,
) -> Res<Json<RecoveryKit>> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    check_name(&group)?;
    let kit = maint::recovery_kit(&svc, &group).await?;
    audit(&who, "recovery kit", &group);
    Ok(Json(kit))
}

async fn restore_code_post(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(device): UrlPath<String>,
) -> Res<Response> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    check_name(&device)?;
    let code = new_code()?;
    let expires_ms = now_ms() + RESTORE_CODE_TTL_MS;
    let btrfs = svc.has_btrfs_folder(&device);
    svc.edit(|r| {
        let files = r.devices.get(&device).is_some_and(|d| !d.group.is_empty());
        if !files && !btrfs {
            return Err(match r.devices.get(&device) {
                None => not_found(&device),
                Some(_) => Error::new(
                    StatusCode::CONFLICT,
                    error_codes::CONFLICT,
                    format!("{device} has neither file backups nor a folder of btrfs snapshots to restore from"),
                ),
            });
        }
        let now = now_ms();
        // a new code replaces the PC's earlier ones (one valid code per PC at a time)
        r.restore_codes.retain(|c| c.expires_ms > now && c.device != device);
        r.restore_codes.push(RestoreCode {
            code_sha: code_hash(&code),
            device: device.clone(),
            expires_ms,
            batch: None,
        });
        Ok(())
    })?;
    audit(&who, "restore code", &device);
    Ok((
        StatusCode::CREATED,
        Json(RestoreCodeCreated {
            code,
            expires_ms,
            device,
        }),
    )
        .into_response())
}

async fn restore_choice_post(State(st): State<PaState>, h: HeaderMap) -> Res<Json<RestoreChoice>> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    let batch = new_code()?;
    // PCs with file backups, with a folder of btrfs snapshots (paired agents or btrbk over
    // SSH), or both
    let folders: BTreeMap<String, String> = svc.btrfs.get().map(|b| b.target_names().into_iter().collect()).unwrap_or_default();
    let devices = svc.edit(|r| {
        let now = now_ms();
        r.restore_codes.retain(|c| c.expires_ms > now);
        let mut devices = Vec::new();
        let mut names: BTreeMap<String, (String, String)> = r
            .devices
            .iter()
            .filter(|(n, d)| !d.group.is_empty() || folders.contains_key(*n))
            .map(|(n, d)| (n.clone(), (d.settings.description.clone(), d.hostname.clone())))
            .collect();
        for (n, desc) in &folders {
            names.entry(n.clone()).or_insert_with(|| (desc.clone(), String::new()));
        }
        for (name, (description, hostname)) in names {
            let code = new_code()?;
            r.restore_codes.push(RestoreCode {
                code_sha: code_hash(&code),
                device: name.clone(),
                expires_ms: now + ADMIN_CODE_TTL_MS,
                batch: Some(batch.clone()),
            });
            devices.push(RestoreChoiceDevice {
                device: name.clone(),
                description,
                hostname,
                code,
            });
        }
        Ok(devices)
    })?;
    audit(&who, "restore login from a live system", &format!("{} PCs", devices.len()));
    Ok(Json(RestoreChoice { devices }))
}

// ------------------------------------------------------------------ the agent

async fn pair_post(
    State(st): State<PaState>,
    h: HeaderMap,
    body: Result<Json<PairRequest>, JsonRejection>,
) -> Res<Json<PairResponse>> {
    let svc = pa(&st)?;
    let req = json_body(body)?;
    let source = source_of(&h);
    svc.check_rate(&source).await?;
    if req.hostname.is_empty() || req.hostname.len() > 255 || req.hostname.chars().any(char::is_control) {
        return Err(Error::invalid("hostname", "not a host name"));
    }
    let h = code_hash(&req.code);
    // one-time: the pairing is gone before anything else happens
    let pairing = svc.edit(|r| {
        let now = now_ms();
        r.pairings.retain(|p| p.expires_ms > now);
        let i = r.pairings.iter().position(|p| p.code_sha == h);
        Ok(i.map(|i| (r.pairings.remove(i), r.settings.clone())))
    })?;
    let Some((pairing, settings)) = pairing else {
        return Err(svc.failed(&source));
    };
    let mut out = maint::pair(&svc, &pairing, &settings, &req).await;
    match &out {
        Ok(p) => tracing::info!(target: "rynas::audit", "pc backup: {} paired as {} in group {}", req.hostname, p.device, p.group),
        Err(e) => tracing::warn!("pc backup: pairing of {} failed: {}", req.hostname, e.2),
    }
    if let Ok(p) = &mut out {
        let s = svc.clone();
        p.trust = blocking(move || s.trust()).await?;
    }
    out.map(Json)
}

/// `GET /v1/pcbackup/agent/trust`: the local CA, identity and addresses agents pin (see
/// [`AgentTrust`]); 404 without a local CA (dry systems).
async fn trust_get(State(st): State<PaState>, h: HeaderMap) -> Res<Json<AgentTrust>> {
    let svc = pa(&st)?;
    let device = agent_device(&svc, &h)?;
    if svc.device(&device)?.revoked {
        return Err(Error::new(StatusCode::UNAUTHORIZED, error_codes::UNAUTHENTICATED, "revoked".into()));
    }
    let s = svc.clone();
    blocking(move || s.trust())
        .await?
        .map(Json)
        .ok_or_else(|| Error::not_found("this NAS has no local certificate authority"))
}

#[derive(Deserialize, Default)]
struct PlanQuery {
    #[serde(default)]
    rev: Option<u64>,
    #[serde(default)]
    wait: Option<u64>,
}

/// The plan an agent receives: the PC's settings, the shared trigger (S6; the two old fields
/// stay for older agents, R5: with set times `interval_hours` is the calendar's spacing), the
/// NAS's zone (the window's and the calendar's), and `paused`
/// while every task of the NAS is paused (with its own revision, so every agent takes it).
fn plan_of(d: &Dev, paused_all: bool) -> AgentPlan {
    plan_at(d, paused_all, &rynas_power::system_tz(), now_ms())
}

/// [`plan_of`] in the zone `tz` at `now`.
fn plan_at(d: &Dev, paused_all: bool, tz: &jiff::tz::TimeZone, now: u64) -> AgentPlan {
    let mut plan = d.settings.plan.clone();
    plan.paused |= paused_all;
    let tz = tz.clone();
    let (trigger, every_h) = plan_trigger(&plan, &tz, now, agent_follows_calendar(&d.agent_version));
    plan.interval_hours = every_h;
    AgentPlan {
        rev: if paused_all { d.rev | PAUSED_ALL_REV } else { d.rev },
        trigger: Some(trigger),
        tz: tz.iana_name().filter(|n| !n.is_empty()).map(str::to_string),
        tz_posix: rynas_power::system_posix(),
        plan,
        command: d.command.clone(),
        method: d.settings.method,
        woken_ms: d.woken_ms.filter(|w| now.saturating_sub(*w) < WOKEN_FOR_MS),
    }
}

/// A PC plan as the trigger its agent follows, and the interval of the plan's
/// `interval_hours` field (the calendar's spacing with set times):
///
/// - set times, for an agent that knows them (`follows_calendar`, 0.8.21 and later):
///   `Calendar`;
/// - set times for an older agent: `Interval` of the calendar's spacing, which a 0.8.20 agent
///   follows with the window and its skipped-start reports (it knows only `Interval`);
/// - set times without any future time (an expression with a date that has passed): the
///   plan's interval, so backups go on;
/// - else: the plan's interval.
pub fn plan_trigger(plan: &PcPlan, tz: &jiff::tz::TimeZone, now: u64, follows_calendar: bool) -> (rynas_api_types::scheduler::TaskTrigger, u32) {
    use rynas_api_types::scheduler::TaskTrigger;
    // ("also at every start of the PC" is the plan's `at_boot`, on top of either)
    let interval = |hours: u32| (TaskTrigger::Interval { hours, after_boot_min: plan.start_delay_min }, hours);
    if let Some(c) = plan.calendar.as_deref().map(str::trim).filter(|c| !c.is_empty())
        && let Ok(spec) = rynas_schedule::CalendarSpec::parse(c)
        && i64::try_from(now).ok().and_then(|m| jiff::Timestamp::from_millisecond(m).ok()).and_then(|t| spec.next_after(tz, t)).is_some()
    {
        let every = rynas_schedule::window::calendar_every_h(&spec, tz, now);
        if !follows_calendar {
            return interval(every);
        }
        return (TaskTrigger::Calendar { on_calendar: c.to_string(), cron: None, persistent: true, randomized_delay_s: 0 }, every);
    }
    interval(plan.interval_hours)
}

async fn plan_get(
    State(st): State<PaState>,
    h: HeaderMap,
    Query(q): Query<PlanQuery>,
) -> Res<Json<AgentPlan>> {
    let svc = pa(&st)?;
    let device = agent_device(&svc, &h)?;
    let deadline = Instant::now() + Duration::from_secs(q.wait.unwrap_or(0).min(MAX_WAIT));
    loop {
        // registered before the check: a change between the check and the wait is not lost
        let notified = svc.changed.notified();
        let d = svc.device(&device)?;
        if d.revoked {
            return Err(Error::new(StatusCode::UNAUTHORIZED, error_codes::UNAUTHENTICATED, "revoked".into()));
        }
        let seen = now_ms();
        // at most once a minute, and at once when a command waits for this contact (the
        // console shows it pending while the last contact is older than the command)
        let taking = d.command_at_ms.is_some_and(|c| d.last_seen_ms.is_none_or(|t| t < c));
        if taking || d.last_seen_ms.is_none_or(|t| seen.saturating_sub(t) > 60_000) {
            svc.touch(|r| {
                if let Some(x) = r.devices.get_mut(&device) {
                    // a power command taken at an earlier contact is done (the agent
                    // remembers the ids it handled; no run report follows it)
                    if is_power_command(x.command.as_ref()) && x.command_at_ms.is_some_and(|c| x.last_seen_ms.is_some_and(|t| t >= c)) {
                        x.command = None;
                        x.command_at_ms = None;
                    }
                    x.contact(seen);
                }
            });
        }
        let plan = plan_of(&d, svc.paused_all());
        if q.rev != Some(plan.rev) || Instant::now() >= deadline {
            if matches!(plan.command, Some(AgentCommand::Sleep { .. })) && q.rev != Some(plan.rev) {
                svc.note_slept(&device);
            }
            return Ok(Json(plan));
        }
        // a pause of every task that ends by itself wakes nobody: looked at again within 10 s
        let left = deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(10));
        let _ = tokio::time::timeout(left, notified).await;
    }
}

async fn status_post(
    State(st): State<PaState>,
    h: HeaderMap,
    body: Result<Json<AgentStatus>, JsonRejection>,
) -> Res<StatusCode> {
    let svc = pa(&st)?;
    let device = agent_device(&svc, &h)?;
    let mut s = json_body(body)?;
    // (a cut inside a character would panic)
    maint::trunc(&mut s.run_id, 64);
    if let Some(m) = &mut s.message {
        maint::trunc(m, 500);
    }
    if let Some(b) = &mut s.btrfs {
        maint::bound_step(b);
    }
    svc.progress(&device, s);
    Ok(StatusCode::NO_CONTENT)
}

async fn report_post(
    State(st): State<PaState>,
    h: HeaderMap,
    body: Result<Json<AgentReport>, JsonRejection>,
) -> Res<StatusCode> {
    let svc = pa(&st)?;
    let device = agent_device(&svc, &h)?;
    let r = json_body(body)?;
    let s2 = svc.clone();
    blocking(move || maint::report(&s2, &device, r)).await??;
    Ok(StatusCode::NO_CONTENT)
}

/// Most network cards kept of a PC's facts.
const MAX_NICS: usize = 32;

/// The facts as stored: bounded lists and strings, MACs normalized (cards without a valid
/// MAC dropped).
pub fn bound_facts(mut f: AgentFacts) -> AgentFacts {
    let cut = |s: &mut String, n: usize| {
        if s.len() > n {
            let mut c = n;
            while !s.is_char_boundary(c) {
                c -= 1;
            }
            s.truncate(c);
        }
    };
    cut(&mut f.agent_version, 32);
    f.nics.retain_mut(|n| match rynas_api_types::wol::normalize_mac(&n.mac) {
        Some(m) => {
            n.mac = m;
            cut(&mut n.name, 32);
            n.ipv4.truncate(8);
            n.ipv4.retain(|c| c.len() <= 18);
            true
        }
        None => false,
    });
    f.nics.truncate(MAX_NICS);
    // a mode this NAS does not know is never offered nor sent
    f.sleep.retain(|m| *m != SleepMode::Unknown);
    f.sleep.dedup();
    f.sleep.truncate(3);
    f
}

/// `POST /v1/pcbackup/agent/facts` (device token): the PC's facts (agents from 0.8.26).
async fn facts_post(
    State(st): State<PaState>,
    h: HeaderMap,
    body: Result<Json<AgentFacts>, JsonRejection>,
) -> Res<StatusCode> {
    let svc = pa(&st)?;
    let device = agent_device(&svc, &h)?;
    let f = bound_facts(json_body(body)?);
    let s2 = svc.clone();
    blocking(move || s2.store_facts(&device, f)).await??;
    Ok(StatusCode::NO_CONTENT)
}

impl PcAgent {
    /// Stores an agent's facts (and its version, its root filesystem).
    pub fn store_facts(&self, device: &str, f: AgentFacts) -> Res<()> {
        self.edit(|r| {
            let d = r.devices.get_mut(device).ok_or_else(|| not_found(device))?;
            if !f.agent_version.is_empty() {
                d.agent_version = f.agent_version.clone();
            }
            if f.root_btrfs.is_some() {
                d.root_btrfs = f.root_btrfs;
            }
            d.facts = Some(f);
            Ok(())
        })
    }

    /// The PC reported progress of a run in the last two minutes (a backup runs).
    pub fn running(&self, device: &str) -> bool {
        self.progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(device)
            .is_some_and(|(p, _)| now_ms().saturating_sub(p.updated_ms) < 120_000)
    }

    /// Queues a power command (`make` gets the command's id) for the agent's next poll:
    /// 404 unknown PC, 409 `conflict` when revoked or the agent is older than
    /// [`POWER_AGENT`], 409 `busy` while a backup runs.
    pub fn power_command(&self, device: &str, make: impl FnOnce(u64) -> AgentCommand) -> Res<()> {
        let busy = self.running(device);
        self.edit(|r| {
            // (a refusal drops the whole edit, the counter too)
            let seq = r.seq + 1;
            r.seq = seq;
            let d = r.devices.get_mut(device).ok_or_else(|| not_found(device))?;
            if d.revoked {
                return Err(Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, "the PC is revoked".into()));
            }
            if !takes_power_commands(d) {
                return Err(Error::new(
                    StatusCode::CONFLICT,
                    error_codes::CONFLICT,
                    format!("the agent of {device} is older than 0.8.26: update it on the PC"),
                ));
            }
            let backing_up = busy || matches!(d.command, Some(AgentCommand::BackupNow { .. })) && d.command_at_ms.is_some_and(|c| d.last_seen_ms.is_none_or(|t| t < c));
            if backing_up {
                return Err(Error::new(StatusCode::CONFLICT, error_codes::BUSY, format!("a backup of {device} is running")));
            }
            d.command = Some(make(seq));
            d.command_at_ms = Some(now_ms());
            d.rev += 1;
            Ok(())
        })?;
        self.changed.notify_waiters();
        Ok(())
    }

    /// Marks the PC as woken by the NAS for a due backup ([`AgentPlan::woken_ms`]).
    pub fn set_woken(&self, device: &str, at_ms: u64) -> Res<()> {
        self.edit(|r| {
            let d = r.devices.get_mut(device).ok_or_else(|| not_found(device))?;
            d.woken_ms = Some(at_ms);
            d.rev += 1;
            Ok(())
        })?;
        self.changed.notify_waiters();
        Ok(())
    }
}

/// The scheduled backup due now inside its allowed window, as the device's agent counts it
/// (wake for backups): its due time, or `None` (not due, outside the window, paused,
/// revoked, Pause all).
pub fn wake_due(d: &Dev, paused_all: bool, tz: &jiff::tz::TimeZone, now: u64) -> Option<u64> {
    let (s, spec) = followed(d, paused_all, tz, now)?;
    let due = rynas_schedule::window::agent_due(spec.as_ref(), u64::from(s.every_h.max(1)) * 3_600_000, tz, last_ok(d), 0, 0, None);
    if due == u64::MAX || due > now {
        return None;
    }
    let start = rynas_schedule::window::start_at(s.window.as_deref(), tz, due)?;
    let open = s.window.as_deref().is_none_or(|w| rynas_schedule::window::is_open(w, tz, now));
    (start <= now && open).then_some(due)
}

/// The device of an agent request of the btrfs method (token, then [`PcMethod::Btrfs`]) and
/// the folder it is bound to.
fn btrfs_device(svc: &PcAgent, h: &HeaderMap) -> Res<(String, Option<u64>)> {
    let device = agent_device(svc, h)?;
    let d = svc.device(&device)?;
    if d.settings.method != PcMethod::Btrfs {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            "this PC uses the files method (choose btrfs snapshots on the NAS)".into(),
        ));
    }
    Ok((device, d.btrfs_folder))
}

impl PcAgent {
    /// The agent folder `folder` of `device` was removed: a PC bound to it stops sending into
    /// any folder of that name. A PC with only that method is revoked (its token goes); one
    /// that also has a repository falls back to the files method.
    pub fn folder_removed(&self, device: &str, folder: u64) {
        let changed = self.edit(|r| {
            let Some(d) = r.devices.get_mut(device).filter(|d| d.btrfs_folder == Some(folder)) else {
                return Ok(false);
            };
            d.btrfs_folder = None;
            if d.group.is_empty() {
                d.token_sha.clear();
                d.revoked = true;
                d.command = None;
            } else if d.settings.method == PcMethod::Btrfs {
                d.settings.method = PcMethod::Files;
            }
            d.rev += 1;
            Ok(true)
        });
        match changed {
            Ok(true) => {
                self.clear_progress(device);
                self.changed.notify_waiters();
                tracing::info!(target: "rynas::audit", "pc backup: {device}: its btrfs folder was removed, the PC no longer sends into it");
            }
            Ok(false) => {}
            Err(e) => tracing::warn!("pc backup: {device}: unbinding the removed folder: {}", e.2),
        }
    }
}

async fn btrfs_snapshots_get(State(st): State<PaState>, h: HeaderMap) -> Res<Json<AgentBtrfsListing>> {
    let svc = pa(&st)?;
    let (device, folder) = btrfs_device(&svc, &h)?;
    Ok(Json(svc.btrfs_side()?.agent_listing(&device, folder).await?))
}

#[derive(Debug, Deserialize)]
pub struct ReceiveQuery {
    pub snapshot: String,
    /// `none` (or missing): a full send.
    #[serde(default)]
    pub parent: Option<String>,
}

/// `POST /v1/pcbackup/agent/btrfs/receive?snapshot=&parent=`: one `btrfs send` stream (zstd)
/// into the PC's folder of the btrfs method ([`crate::pc_backup::PcBackup::agent_receive`]).
async fn btrfs_receive_post(
    State(st): State<PaState>,
    h: HeaderMap,
    q: Result<Query<ReceiveQuery>, axum::extract::rejection::QueryRejection>,
    body: axum::body::Body,
) -> Res<Json<AgentBtrfsReceived>> {
    let svc = pa(&st)?;
    let (device, folder) = btrfs_device(&svc, &h)?;
    let Query(q) = q.map_err(|e| Error::bad_request(e.body_text()))?;
    let parent = q.parent.filter(|p| !p.is_empty() && p != "none");
    let side = svc.btrfs_side()?.clone();
    let seen = now_ms();
    svc.touch(|r| {
        if let Some(x) = r.devices.get_mut(&device) {
            x.contact(seen);
        }
    });
    Ok(Json(side.agent_receive(&device, folder, q.snapshot, parent, body).await?))
}

async fn restore_login(
    State(st): State<PaState>,
    h: HeaderMap,
    body: Result<Json<RestoreLogin>, JsonRejection>,
) -> Res<Json<RestoreSession>> {
    let svc = pa(&st)?;
    let req = json_body(body)?;
    let source = source_of(&h);
    svc.check_rate(&source).await?;
    let hash = code_hash(&req.code);
    let code = svc.edit(|r| {
        let now = now_ms();
        r.restore_codes.retain(|c| c.expires_ms > now);
        let i = r.restore_codes.iter().position(|c| c.code_sha == hash);
        let code = i.map(|i| r.restore_codes.remove(i));
        // an administrator login's other codes end with this one
        if let Some(b) = code.as_ref().and_then(|c| c.batch.as_ref()) {
            r.restore_codes.retain(|c| c.batch.as_ref() != Some(b));
        }
        Ok(code)
    })?;
    let Some(code) = code else {
        return Err(svc.failed(&source));
    };
    // a PC with file backups (a paired record with a group), a folder of btrfs snapshots, or
    // both; a btrbk folder has no PC record at all
    let d = svc.device(&code.device).ok();
    let btrfs = svc.has_btrfs_folder(&code.device);
    let group = d.as_ref().map(|d| d.group.clone()).unwrap_or_default();
    let give_back = |code: RestoreCode| {
        // the code was not used up: it works again
        let _ = svc.edit(move |r| {
            r.restore_codes.push(code);
            Ok(())
        });
    };
    if group.is_empty() && !btrfs {
        give_back(code.clone());
        return Err(not_found(&code.device));
    }
    // a key of its own for this session (never the NAS' key): its password stops opening the
    // repository when the session ends and the key file goes
    let (key_id, password) = if group.is_empty() {
        (String::new(), String::new())
    } else {
        let g = match svc.group(&group) {
            Ok(g) => g,
            Err(e) => {
                give_back(code.clone());
                return Err(e);
            }
        };
        match maint::add_session_key(&svc, &group, &g, &code.device).await {
            Ok(k) => k,
            Err(e) => {
                give_back(code.clone());
                return Err(e);
            }
        }
    };
    let token = new_token()?;
    let now = now_ms();
    let session = Session {
        token_sha: sha_hex(token.as_bytes()),
        devices: vec![code.device.clone()],
        created_ms: now,
        expires_ms: now + SESSION_IDLE_MS,
        key_group: group.clone(),
        key_id,
    };
    let expires_ms = session.expires_ms;
    svc.edit(|r| {
        r.sessions.push(session);
        Ok(())
    })?;
    tracing::info!(target: "rynas::audit", "pc backup: restore session for {}", code.device);
    Ok(Json(RestoreSession {
        token,
        expires_ms,
        devices: vec![RestoreDevice {
            device: code.device.clone(),
            group,
            description: d.as_ref().map(|d| d.settings.description.clone()).unwrap_or_default(),
            hostname: d.as_ref().map(|d| d.hostname.clone()).unwrap_or_default(),
            repo_password: password,
            btrfs,
        }],
    }))
}

/// A restore session's request about `device`: 401 without a live session, 403 when the
/// session does not cover that PC. The request keeps the session alive.
fn session_for(svc: &PcAgent, h: &HeaderMap, device: &str) -> Res<String> {
    let token = bearer(h).unwrap_or_default();
    let devices = svc
        .session_devices(&token)
        .ok_or_else(|| Error::new(StatusCode::UNAUTHORIZED, error_codes::UNAUTHENTICATED, "no restore session".into()))?;
    if !devices.iter().any(|d| d == device) {
        return Err(Error::new(StatusCode::FORBIDDEN, error_codes::FORBIDDEN, "not this session's PC".into()));
    }
    Ok(token)
}

#[derive(Debug, Deserialize)]
pub struct RestoreSnapQuery {
    #[serde(default)]
    pub snapshot: String,
}

fn snap_query(q: Result<Query<RestoreSnapQuery>, axum::extract::rejection::QueryRejection>) -> Res<RestoreSnapQuery> {
    let Query(q) = q.map_err(|e| Error::bad_request(e.body_text()))?;
    if q.snapshot.is_empty() {
        return Err(Error::invalid("snapshot", "a snapshot name"));
    }
    Ok(q)
}

/// `GET /v1/pcbackup/agent/restore/btrfs/{device}/snapshots` (restore session).
async fn restore_btrfs_snapshots(State(st): State<PaState>, h: HeaderMap, UrlPath(device): UrlPath<String>) -> Res<Json<RestoreBtrfsListing>> {
    let svc = pa(&st)?;
    session_for(&svc, &h, &device)?;
    Ok(Json(svc.btrfs_side()?.restore_listing(&device).await?))
}

/// `GET …/restore/btrfs/{device}/manifest?snapshot=` (restore session): the manifest as the
/// PC wrote it.
async fn restore_btrfs_manifest(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(device): UrlPath<String>,
    q: Result<Query<RestoreSnapQuery>, axum::extract::rejection::QueryRejection>,
) -> Res<Response> {
    let svc = pa(&st)?;
    session_for(&svc, &h, &device)?;
    let q = snap_query(q)?;
    let b = svc.btrfs_side()?.restore_manifest(&device, q.snapshot).await?;
    Ok(([(header::CONTENT_TYPE, "application/json")], b).into_response())
}

/// `GET …/restore/btrfs/{device}/legacy?snapshot=` (restore session).
async fn restore_btrfs_legacy(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(device): UrlPath<String>,
    q: Result<Query<RestoreSnapQuery>, axum::extract::rejection::QueryRejection>,
) -> Res<Json<RestoreLegacy>> {
    let svc = pa(&st)?;
    session_for(&svc, &h, &device)?;
    let q = snap_query(q)?;
    Ok(Json(svc.btrfs_side()?.restore_legacy(&device, q.snapshot).await?))
}

/// `GET …/restore/btrfs/{device}/send?snapshot=` (restore session): the full stream,
/// zstd-compressed. The session stays alive while it runs; the stream stops when it ends.
async fn restore_btrfs_send(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(device): UrlPath<String>,
    q: Result<Query<RestoreSnapQuery>, axum::extract::rejection::QueryRejection>,
) -> Res<Response> {
    let svc = pa(&st)?;
    let token = session_for(&svc, &h, &device)?;
    let q = snap_query(q)?;
    let weak = Arc::downgrade(&svc);
    let dev = device.clone();
    // keeps the session alive; `false` once it ended (the report closed it, it expired)
    let keepalive = Box::new(move || weak.upgrade().and_then(|s| s.session_devices(&token)).is_some_and(|d| d.contains(&dev)));
    let who = format!("a restore session from {}", source_of(&h));
    let body = svc.btrfs_side()?.restore_send(&device, q.snapshot, who, keepalive).await?;
    Ok(([(header::CONTENT_TYPE, "application/zstd")], body).into_response())
}

async fn restore_report(
    State(st): State<PaState>,
    h: HeaderMap,
    body: Result<Json<RestoreReport>, JsonRejection>,
) -> Res<StatusCode> {
    let svc = pa(&st)?;
    let token = bearer(&h).unwrap_or_default();
    let devices = svc
        .session_devices(&token)
        .ok_or_else(|| Error::new(StatusCode::UNAUTHORIZED, error_codes::UNAUTHENTICATED, "no restore session".into()))?;
    let token_sha = sha_hex(token.as_bytes());
    let mut r = json_body(body)?;
    if !devices.contains(&r.device) {
        return Err(Error::new(StatusCode::FORBIDDEN, error_codes::FORBIDDEN, "not this session's PC".into()));
    }
    if r.log.len() > 64 << 10 {
        let mut cut = r.log.len() - (64 << 10);
        while !r.log.is_char_boundary(cut) {
            cut += 1;
        }
        r.log = r.log[cut..].to_string();
    }
    maint::trunc(&mut r.message, 2000);
    // kept in the PC's history: a version id or a snapshot name, never longer
    maint::trunc(&mut r.version, 160);
    let now = now_ms();
    svc.edit(|reg| {
        // a btrbk folder has no PC record: its restore is in the audit log and the log file
        if let Some(d) = reg.devices.get_mut(&r.device) {
            d.history.push_back(PcRun {
                run_id: format!("restore-{now}"),
                started_ms: now,
                ended_ms: now,
                ok: r.ok,
                snapshot: Some(r.version.clone()),
                error: (!r.ok).then(|| r.message.clone()),
                restore: true,
                ..Default::default()
            });
            while d.history.len() > HISTORY {
                d.history.pop_front();
            }
        }
        // the report is the restore's last call: the session ends, its key goes
        if let Some(s) = reg.sessions.iter_mut().find(|s| s.token_sha == token_sha) {
            s.expires_ms = now;
        }
        Ok(())
    })?;
    let s2 = svc.clone();
    tokio::spawn(async move { maint::end_sessions(&s2).await });
    let dir = PcAgent::dir(&svc.state_dir).join("restore-logs");
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir);
    let _ = crate::tls::write_atomic(
        &dir.join(format!("{}-{now}.log", r.device)),
        r.log.as_bytes(),
        0o600,
        None,
    );
    tracing::info!(
        target: "rynas::audit",
        "pc backup: restore of {} reported ({})",
        r.device,
        if r.ok { "ok" } else { "failed" }
    );
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        let c = new_code().unwrap();
        assert_eq!(c.len(), 14);
        assert_eq!(c.matches('-').count(), 2);
        assert_eq!(code_hash(&c.to_lowercase()), code_hash(&c));
        assert_eq!(normalize_code("o1l-i"), "0111");
        assert_eq!(new_token().unwrap().len(), 64);
    }

    /// What an agent older than 0.8.21 receives for a plan at set times: an interval of the
    /// calendar's spacing (the only trigger 0.8.20 knows, so it keeps the window and its skip
    /// reports), never the calendar; a current agent gets the calendar.
    #[test]
    fn set_times_for_older_agents() {
        let tz = jiff::tz::TimeZone::get("Europe/Rome").unwrap();
        let now = 1_795_000_000_000;
        let window = format!("{}{}", "0".repeat(22), "1".repeat(146));
        let plan = PcPlan { calendar: Some("Mon *-*-* 03:00:00".into()), start_delay_min: 7, window: Some(window.clone()), ..PcPlan::default() };
        let (old, every) = plan_trigger(&plan, &tz, now, false);
        assert_eq!(every, 168);
        let v = serde_json::to_value(&old).unwrap();
        assert_eq!(v, serde_json::json!({"kind": "interval", "hours": 168, "after_boot_min": 7}));
        // as 0.8.20 reads the plan: the shared interval, the window kept
        let sent = AgentPlan { plan: PcPlan { interval_hours: every, ..plan.clone() }, trigger: Some(old), ..AgentPlan::default() };
        let s = sent.schedule();
        assert!(s.shared && s.calendar.is_none() && s.window == Some(window) && s.every_h == 168);
        let (new, _) = plan_trigger(&plan, &tz, now, true);
        assert!(matches!(new, rynas_api_types::scheduler::TaskTrigger::Calendar { .. }));
        // no time left: the plan's own interval for everyone
        let gone = PcPlan { calendar: Some("2020-01-01 00:00:00".into()), interval_hours: 12, ..PcPlan::default() };
        assert_eq!(plan_trigger(&gone, &tz, now, true).1, 12);
    }

    #[test]
    fn bearer_forms() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(bearer(&h).as_deref(), Some("abc"));
        let b = base64::engine::general_purpose::STANDARD.encode("pc:tok");
        h.insert(header::AUTHORIZATION, format!("Basic {b}").parse().unwrap());
        assert_eq!(bearer(&h).as_deref(), Some("tok"));
    }

    #[test]
    fn folders() {
        let places = vec![("data".to_string(), "/volume1".to_string())];
        assert!(check_folder("/volume1/pcbackup", &places).is_ok());
        assert!(check_folder("/volume1", &places).is_err());
        assert!(check_folder("/etc/x", &places).is_err());
        assert!(check_folder("/volume1/../etc", &places).is_err());
        assert!(check_folder("", &places).is_ok());
    }

    #[test]
    fn queries() {
        let (v, p) = paths_of(Some("version=abcdef12&path=%2Fhome%2Fa%20b&path=/etc")).unwrap();
        assert_eq!(v, "abcdef12");
        assert_eq!(p, vec!["/home/a b".to_string(), "/etc".to_string()]);
        assert!(paths_of(Some("version=zz&path=/x")).is_err());
        assert!(paths_of(Some("version=latest&path=/../x")).is_err());
        assert!(percent_decode("%2").is_none());
        assert_eq!(percent_decode("a%41").as_deref(), Some("aA"));
    }

    #[test]
    fn states() {
        let mut d = Dev {
            created_ms: 0,
            settings: PcDeviceSettings::default(),
            ..Default::default()
        };
        let h = 3_600_000;
        // never backed up: waiting while nothing was missed, late (then critical) once one was
        assert_eq!(state_of(&d, false, false, 100 * h), PcState::Waiting);
        assert_eq!(state_of(&d, false, true, 10 * h), PcState::Late);
        assert_eq!(state_of(&d, false, true, 100 * h), PcState::Critical);
        d.last_ok_ms = Some(100 * h);
        assert_eq!(state_of(&d, false, false, 101 * h), PcState::Ok);
        // a PC that is off for days: still as its last backup left it
        assert_eq!(state_of(&d, false, false, 500 * h), PcState::Ok);
        assert_eq!(state_of(&d, false, true, 127 * h), PcState::Late);
        assert_eq!(state_of(&d, false, true, 173 * h), PcState::Critical);
        // no alerts: late, never critical
        d.settings.alert_hours = 0;
        assert_eq!(state_of(&d, false, true, 173 * h), PcState::Late);
        d.settings.alert_hours = 26;
        assert_eq!(state_of(&d, true, true, 173 * h), PcState::Running);
        d.history.push_back(PcRun {
            ok: false,
            ..Default::default()
        });
        // failed is said before late
        assert_eq!(state_of(&d, false, false, 101 * h), PcState::Failed);
        assert_eq!(state_of(&d, false, true, 173 * h), PcState::Failed);
        assert!(!ready_of(&d, None).ready);
    }

    #[test]
    fn power_commands_only_for_new_agents() {
        let facts = Some(AgentFacts { agent_version: "0.8.26".into(), ..Default::default() });
        let d = |v: &str, f: Option<AgentFacts>| Dev { agent_version: v.into(), facts: f, ..Default::default() };
        assert!(takes_power_commands(&d("0.8.26", facts.clone())));
        assert!(takes_power_commands(&d("v0.9.0", facts.clone())));
        assert!(!takes_power_commands(&d("0.8.25", facts.clone())));
        // an unknown version is never trusted with a command it may not parse
        assert!(!takes_power_commands(&d("", facts.clone())));
        assert!(!takes_power_commands(&d("dev", facts)));
        // no facts: an agent that never said what it can do
        assert!(!takes_power_commands(&d("0.8.26", None)));
        assert!(is_power_command(Some(&AgentCommand::Sleep { id: 1, mode: SleepMode::Suspend })));
        assert!(!is_power_command(Some(&AgentCommand::BackupNow { id: 1 })));
    }

    #[test]
    fn facts_are_bounded() {
        let f = bound_facts(AgentFacts {
            agent_version: "x".repeat(100),
            nics: (0..40)
                .map(|i| AgentNic { name: format!("eth{i}"), mac: format!("AA:BB:CC:DD:EE:{i:02X}"), ..Default::default() })
                .chain([AgentNic { name: "bad".into(), mac: "ff:ff:ff:ff:ff:ff".into(), ..Default::default() }])
                .collect(),
            sleep: vec![SleepMode::Suspend, SleepMode::Suspend, SleepMode::Unknown, SleepMode::Poweroff],
            ..Default::default()
        });
        assert_eq!(f.agent_version.len(), 32);
        assert_eq!(f.nics.len(), MAX_NICS);
        assert_eq!(f.nics[1].mac, "aa:bb:cc:dd:ee:01");
        assert!(f.nics.iter().all(|n| n.name != "bad"));
        assert_eq!(f.sleep, [SleepMode::Suspend, SleepMode::Poweroff]);
    }

    #[test]
    fn woken_plan_and_due_wake() {
        let tz = jiff::tz::TimeZone::UTC;
        let t = 1_800_000_000_000u64;
        let h = 3_600_000;
        let mut d = Dev { agent_version: "0.8.26".into(), last_ok_ms: Some(t), last_ok_start_ms: Some(t), ..Default::default() };
        d.settings.plan.interval_hours = 24;
        // not due yet, then due
        assert_eq!(wake_due(&d, false, &tz, t + h), None);
        assert_eq!(wake_due(&d, false, &tz, t + 25 * h), Some(t + 24 * h));
        // paused (also by Pause all) or revoked: never
        assert_eq!(wake_due(&d, true, &tz, t + 25 * h), None);
        d.revoked = true;
        assert_eq!(wake_due(&d, false, &tz, t + 25 * h), None);
        d.revoked = false;
        // a window closed now (open when the backup fell due): not now
        let now_dt = jiff::Timestamp::from_millisecond((t + 25 * h) as i64).unwrap().to_zoned(tz.clone()).datetime();
        let cell = rynas_schedule::window::cell_of(&now_dt);
        d.settings.plan.window = Some((0..168).map(|c| if c == cell { '0' } else { '1' }).collect());
        assert_eq!(wake_due(&d, false, &tz, t + 25 * h), None);
        d.settings.plan.window = Some("1".repeat(168));
        assert_eq!(wake_due(&d, false, &tz, t + 25 * h), Some(t + 24 * h));
        // the plan says the NAS woke the PC, for two hours at most
        d.woken_ms = Some(t + 25 * h);
        assert_eq!(plan_at(&d, false, &tz, t + 26 * h).woken_ms, Some(t + 25 * h));
        assert_eq!(plan_at(&d, false, &tz, t + 28 * h).woken_ms, None);
    }

    const H: u64 = 3_600_000;
    const M: u64 = 60_000;

    /// Monday 2026-11-16 00:00 UTC.
    fn t0() -> u64 {
        jiff::civil::date(2026, 11, 16).at(0, 0, 0, 0).to_zoned(jiff::tz::TimeZone::UTC).unwrap().timestamp().as_millisecond() as u64
    }

    /// A PC of `version` backed up at `last_ok`, online from `since` to `seen`.
    fn pc(version: &str, last_ok: u64, since: u64, seen: u64) -> Dev {
        Dev {
            agent_version: version.into(),
            created_ms: 0,
            last_ok_ms: Some(last_ok),
            online_since_ms: Some(since),
            last_seen_ms: Some(seen),
            settings: PcDeviceSettings::default(),
            ..Default::default()
        }
    }

    fn late(d: &Dev, now: u64) -> bool {
        missed_online(d, false, &jiff::tz::TimeZone::UTC, now)
    }

    #[test]
    fn off_is_not_late() {
        // every 24 h, 10 min after the start: seen two hours after the backup, then off for days
        let t = t0();
        let d = pc("0.8.22", t, t, t + 2 * H);
        let now = t + 72 * H;
        assert!(!late(&d, now));
        assert_eq!(state_of(&d, false, false, now), PcState::Ok);
        // the backup due after a day was missed while off: it catches up at the start
        assert_eq!(next_due(&d, false, &jiff::tz::TimeZone::UTC, now), Some(t + 24 * H));
        assert_eq!(maint::want_alert(&d, false, &jiff::tz::TimeZone::UTC, now), Alert::None);
    }

    #[test]
    fn online_while_due_is_late() {
        let t = t0();
        // online across the due time (24 h): late once the margin has passed
        assert!(!late(&pc("0.8.22", t, t + 20 * H, t + 24 * H + 29 * M), t + 25 * H));
        assert!(late(&pc("0.8.22", t, t + 20 * H, t + 24 * H + 30 * M), t + 25 * H));
        // the notification only once the last good version is older than alert_hours (26)
        let d = pc("0.8.22", t, t + 20 * H, t + 25 * H);
        assert_eq!(maint::want_alert(&d, false, &jiff::tz::TimeZone::UTC, t + 25 * H), Alert::None);
        let d = pc("0.8.22", t, t + 20 * H, t + 27 * H);
        assert_eq!(maint::want_alert(&d, false, &jiff::tz::TimeZone::UTC, t + 27 * H), Alert::Late);
        let d = pc("0.8.22", t, t + 20 * H, t + 73 * H);
        assert_eq!(maint::want_alert(&d, false, &jiff::tz::TimeZone::UTC, t + 73 * H), Alert::Critical);
        // late while online, then off: still late (it was on and nothing ran)
        assert!(late(&d, t + 200 * H));
    }

    #[test]
    fn came_online_after_due_waits_for_the_delay() {
        // off when due (24 h), on at 30 h: the agent catches up 10 min after its start
        let t = t0();
        assert!(!late(&pc("0.8.22", t, t + 30 * H, t + 30 * H + 39 * M), t + 31 * H));
        assert!(late(&pc("0.8.22", t, t + 30 * H, t + 30 * H + 40 * M), t + 31 * H));
    }

    #[test]
    fn closed_window_is_not_late() {
        // allowed 02:00-03:00 only; backed up Monday 01:00, due Tuesday 01:00, starts at 02:00
        let t = t0();
        let mut w = vec![b'0'; 168];
        for day in 0..7 {
            w[day * 24 + 2] = b'1';
        }
        let mut d = pc("0.8.22", t + H, t + H, t + 24 * H + 2 * H + 29 * M);
        d.settings.plan.window = Some(String::from_utf8(w).unwrap());
        assert!(!late(&d, t + 27 * H));
        d.last_seen_ms = Some(t + 24 * H + 2 * H + 30 * M);
        assert!(late(&d, t + 27 * H));
        // not due yet: the next start is when the window opens
        assert_eq!(next_due(&d, false, &jiff::tz::TimeZone::UTC, t + 2 * H), Some(t + 26 * H));
        // an agent older than 0.8.20 does not know the window: due at 01:00
        d.agent_version = "0.8.19".into();
        d.last_seen_ms = Some(t + 25 * H + 30 * M);
        assert!(late(&d, t + 27 * H));
    }

    #[test]
    fn calendar_follows_the_agent_version() {
        // every day at 04:00; backed up Monday 04:05: due Tuesday 04:00
        let t = t0();
        let mut d = pc("0.8.21", t + 4 * H + 5 * M, t + 4 * H, t + 28 * H + 29 * M);
        d.settings.plan.calendar = Some("*-*-* 04:00:00".into());
        assert!(!late(&d, t + 29 * H));
        d.last_seen_ms = Some(t + 28 * H + 30 * M);
        assert!(late(&d, t + 29 * H));
        // an agent older than 0.8.21 runs every 24 h from the last good run (04:05)
        d.agent_version = "0.8.20".into();
        assert!(!late(&d, t + 29 * H));
        d.last_seen_ms = Some(t + 28 * H + 35 * M);
        assert!(late(&d, t + 29 * H));
    }

    #[test]
    fn paused_and_at_boot_are_never_late() {
        let t = t0();
        let mut d = pc("0.8.22", t, t + 20 * H, t + 30 * H);
        assert!(late(&d, t + 30 * H));
        assert!(!missed_online(&d, true, &jiff::tz::TimeZone::UTC, t + 30 * H));
        assert_eq!(next_due(&d, true, &jiff::tz::TimeZone::UTC, t + 30 * H), None);
        d.settings.plan.paused = true;
        assert!(!late(&d, t + 30 * H));
        // "also at every start": a new stretch online is not told from a resume, never late
        let mut d = pc("0.8.22", t, t + 2 * H, t + 5 * H);
        d.settings.plan.at_boot = true;
        assert!(!late(&d, t + 5 * H));
        // revoked
        let mut d = pc("0.8.22", t, t + 20 * H, t + 30 * H);
        d.revoked = true;
        assert!(!late(&d, t + 30 * H));
    }

    #[test]
    fn contacts_make_stretches_online() {
        let mut d = Dev::default();
        d.contact(1000);
        assert_eq!(d.online_since_ms, Some(1000));
        d.contact(1000 + 4 * M);
        assert_eq!(d.online_since_ms, Some(1000));
        d.contact(1000 + 10 * M);
        assert_eq!(d.online_since_ms, Some(1000 + 10 * M));
        // a record from before the field: the first contact starts one
        let mut d = Dev { last_seen_ms: Some(5000), ..Default::default() };
        d.contact(6000);
        assert_eq!(d.online_since_ms, Some(6000));
    }

    #[test]
    fn contacts_before_a_taken_sleep_do_not_count() {
        assert_eq!(contact_after_sleep(Some(1_000), None), Some(1_000));
        // the poll that took the command and the one started right after it: asleep since
        assert_eq!(contact_after_sleep(Some(1_000), Some(1_000)), None);
        assert_eq!(contact_after_sleep(Some(1_000 + SLEPT_GRACE_MS), Some(1_000)), None);
        // a request after it: the PC is back (or never slept)
        assert_eq!(contact_after_sleep(Some(1_001 + SLEPT_GRACE_MS), Some(1_000)), Some(1_001 + SLEPT_GRACE_MS));
        assert_eq!(contact_after_sleep(None, Some(1_000)), None);
    }

    #[test]
    fn a_busy_session_outlives_the_idle_time_up_to_the_cap() {
        let mut s = Session {
            devices: vec!["pc".into()],
            created_ms: 0,
            expires_ms: SESSION_IDLE_MS,
            ..Default::default()
        };
        // a request every 10 minutes for 30 hours: alive all along
        let step = 10 * 60 * 1000;
        let mut now = 0;
        while now < 30 * 3600 * 1000 {
            assert_eq!(s.slide(now), Some(vec!["pc".to_string()]), "at {now}");
            now += step;
        }
        assert!(now > SESSION_IDLE_MS * 20);
        // never past the cap, however busy
        while now < SESSION_MAX_MS - 1000 {
            assert!(s.slide(now).is_some(), "at {now}");
            now += step;
        }
        now = SESSION_MAX_MS - 1000;
        assert!(s.slide(now).is_some());
        assert_eq!(s.expires_ms, SESSION_MAX_MS);
        now = SESSION_MAX_MS;
        assert!(s.slide(now).is_none());
        // idle longer than the idle time: gone
        let mut idle = Session {
            devices: vec!["pc".into()],
            created_ms: 0,
            expires_ms: SESSION_IDLE_MS,
            ..Default::default()
        };
        assert!(idle.slide(SESSION_IDLE_MS + 1).is_none());
        // kept in the registry: it survives a restart
        let mut reg = Registry::default();
        reg.sessions.push(s.clone());
        let back: Registry = serde_json::from_slice(&serde_json::to_vec(&reg).unwrap()).unwrap();
        assert_eq!(back.sessions[0].expires_ms, s.expires_ms);
    }

    #[test]
    fn parallel_uploads_share_the_quota() {
        let gib = 1u64 << 30;
        let mut l = Live {
            used: Some(gib),
            ..Default::default()
        };
        // 2 GiB quota, 1 GiB used: two 400 MiB uploads fit, the third does not
        let q = 2 * gib;
        let mib = 1u64 << 20;
        assert_eq!(l.reserve(q, Some(400 * mib), gib), Ok((400 * mib, 400 * mib)));
        assert_eq!(l.reserve(q, Some(400 * mib), gib), Ok((400 * mib, 400 * mib)));
        assert!(l.reserve(q, Some(400 * mib), gib).is_err());
        // without a length: only what is left
        assert_eq!(l.reserve(q, None, gib), Ok((224 * mib, 224 * mib)));
        assert!(l.reserve(q, None, gib).is_err());
        // no quota: no reservation, the size limit only
        let mut free = Live::default();
        assert_eq!(free.reserve(0, None, gib), Ok((0, gib)));
    }

    #[test]
    fn wrong_codes_count_per_client() {
        let mut f = Failures::default();
        for _ in 0..MAX_FAILURES {
            assert!(f.check("192.0.2.1").is_ok());
            f.record("192.0.2.1");
        }
        assert!(f.check("192.0.2.1").is_err());
        // somebody else still gets in, unslowed
        assert_eq!(f.check("192.0.2.2"), Ok(false));
        // many clients together only slow the checks down
        for i in 0..SLOW_FAILURES {
            f.record(&format!("198.51.100.{}", i % 250));
        }
        assert_eq!(f.check("192.0.2.2"), Ok(true));
        assert!(f.by_source.len() <= MAX_FAIL_SOURCES);
    }

    #[test]
    fn client_sources() {
        let mut h = HeaderMap::new();
        assert_eq!(source_of(&h), "local");
        h.insert(rynas_auth::ipc::CLIENT_ADDR_HEADER, "192.0.2.7".parse().unwrap());
        assert_eq!(source_of(&h), "192.0.2.7");
        h.insert(rynas_auth::ipc::CLIENT_ADDR_HEADER, "2001:db8:1:2:3:4:5:6".parse().unwrap());
        assert_eq!(source_of(&h), "2001:db8:1:2::/64");
        h.insert(rynas_auth::ipc::CLIENT_ADDR_HEADER, "::ffff:192.0.2.9".parse().unwrap());
        assert_eq!(source_of(&h), "192.0.2.9");
    }

    #[test]
    fn pairing_command_pins_the_certificate() {
        assert_eq!(
            pair_command("ABCD-EFGH-JKMN", Some("AA:BB:01"), false),
            "sudo rynas-agent pair {url} ABCD-EFGH-JKMN --fingerprint AA:BB:01"
        );
        assert_eq!(pair_command("C", None, false), "sudo rynas-agent pair {url} C");
        assert_eq!(pair_command("C", Some("x; rm"), false), "sudo rynas-agent pair {url} C");
        assert_eq!(pair_command("C", Some("AA:BB"), true), "sudo rynas-agent pair {url} C --fingerprint AA:BB --btrfs");
    }
}
