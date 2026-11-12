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
    #[serde(default)]
    pub history: VecDeque<PcRun>,
    #[serde(default)]
    pub versions: u32,
    #[serde(default)]
    pub command: Option<AgentCommand>,
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
    #[serde(default)]
    pub revoked: bool,
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
    pub hostname: String,
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
}

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
        let reg = std::fs::read(Self::reg_path(&state_dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
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
            hostname,
            reg: Mutex::new(reg),
            live: Mutex::new(HashMap::new()),
            progress: Mutex::new(HashMap::new()),
            failures: Mutex::new(Failures::default()),
            dirty: std::sync::atomic::AtomicBool::new(false),
            mem_used: Mutex::new(0),
            changed: tokio::sync::Notify::new(),
        })
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
        PcDevice {
            device: name.to_string(),
            description: d.settings.description.clone(),
            hostname: d.hostname.clone(),
            os: d.os.clone(),
            arch: d.arch.clone(),
            agent_version: d.agent_version.clone(),
            created_ms: d.created_ms,
            last_seen_ms: d.last_seen_ms,
            state: state_of(d, progress.is_some(), now_ms()),
            last_ok_ms: d.last_ok_ms,
            last_run: d.history.iter().rev().find(|r| !r.restore).cloned(),
            progress,
            group: d.group.clone(),
            uploaded_bytes: d.uploaded_bytes,
            versions: d.versions,
            settings: d.settings.clone(),
            bare_metal: ready_of(d, kit),
            command: d.command.clone(),
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

fn not_found(device: &str) -> Error {
    Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, format!("no PC {device}"))
}

fn group_not_found(group: &str) -> Error {
    Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, format!("no group {group}"))
}

/// The console state of a device.
pub fn state_of(d: &Dev, running: bool, now: u64) -> PcState {
    if running {
        return PcState::Running;
    }
    let hours_since = |ms: u64| now.saturating_sub(ms) / 3_600_000;
    let alerts = d.settings.alert_hours > 0;
    let Some(ok) = d.last_ok_ms else {
        // never a good version: late only counted from the pairing
        if d.history.iter().any(|r| !r.restore && !r.ok) {
            return PcState::Failed;
        }
        if alerts && d.settings.critical_hours > 0 && hours_since(d.created_ms) >= u64::from(d.settings.critical_hours) {
            return PcState::Critical;
        }
        return PcState::Waiting;
    };
    let h = hours_since(ok);
    if alerts && d.settings.critical_hours > 0 && h >= u64::from(d.settings.critical_hours) {
        return PcState::Critical;
    }
    if alerts && h >= u64::from(d.settings.alert_hours) {
        return PcState::Late;
    }
    match d.history.iter().rev().find(|r| !r.restore) {
        Some(r) if !r.ok => PcState::Failed,
        _ => PcState::Ok,
    }
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
            if d.last_ok_ms.is_some_and(|ok| ok > m + 86_400_000) {
                reasons.push(ReadyReason::ManifestStale);
            }
            reasons.extend(d.manifest_reasons.iter().copied());
        }
    }
    if kit.is_none() {
        reasons.push(ReadyReason::NoRecoveryKit);
    }
    reasons.dedup();
    BareMetalReady {
        ready: reasons.is_empty(),
        reasons,
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
    bearer(h)
        .and_then(|t| svc.device_of_token(&t))
        .map(|(d, _)| d)
        .ok_or_else(|| {
            Error::new(StatusCode::UNAUTHORIZED, error_codes::UNAUTHENTICATED, "unknown device token".into())
        })
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
        .route("/v1/pcbackup/agent/status", post(status_post))
        .route("/v1/pcbackup/agent/report", post(report_post))
        .route("/v1/pcbackup/agent/restore/login", post(restore_login))
        .route("/v1/pcbackup/agent/restore/report", post(restore_report))
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
    let s = json_body(body)?;
    if s.maintenance_hour > 23 {
        return Err(Error::invalid("maintenance_hour", "0–23"));
    }
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
    svc.edit(|r| {
        // existing repositories stay where they are; only new groups use a new folder
        r.settings = PcAgentSettings {
            folder: folder.clone(),
            ..s
        };
        Ok(())
    })?;
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
        && svc.group(g).is_err()
    {
        return Err(Error::invalid("group", "no such group"));
    }
    let code = new_code()?;
    let id = new_token()?[..12].to_string();
    let expires_ms = now_ms() + PAIRING_TTL_MS;
    svc.edit(|r| {
        if r.settings.folder.is_empty() {
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
            group: req.group.clone(),
            quota_gib: req.quota_gib,
            expires_ms,
        });
        Ok(())
    })?;
    audit(&who, "pairing", &id);
    let fingerprint = svc.fingerprint();
    let body = PairingCreated {
        id: id.clone(),
        command: pair_command(&code, fingerprint.as_deref()),
        code,
        expires_ms,
        fingerprint,
    };
    Ok(crate::error::created("/api/v1/pcbackup/pairings", &id, body))
}

/// The command the console shows for a pairing (`{url}` is filled in by the browser): with
/// the certificate's fingerprint, so the agent pins the NAS' certificate without asking and
/// nobody in the path can pose as the NAS.
pub fn pair_command(code: &str, fingerprint: Option<&str>) -> String {
    match fingerprint.filter(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_hexdigit() || b == b':')) {
        Some(f) => format!("sudo rynas-agent pair {{url}} {code} --fingerprint {f}"),
        None => format!("sudo rynas-agent pair {{url}} {code}"),
    }
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
    let d = svc.edit(|r| {
        let d = r.devices.get_mut(&device).ok_or_else(|| not_found(&device))?;
        if d.settings.plan != s.plan {
            d.rev += 1;
        }
        d.settings = s;
        Ok(d.clone())
    })?;
    svc.changed.notify_waiters();
    audit(&who, "settings", &device);
    Ok(Json(svc.view(&device, &d, &svc.snapshot())))
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
        d.command = Some(if cancel {
            AgentCommand::Cancel { id: seq }
        } else {
            AgentCommand::BackupNow { id: seq }
        });
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
        host: String::new(),
    }
}

async fn versions_get(
    State(st): State<PaState>,
    UrlPath(device): UrlPath<String>,
) -> Res<Json<Vec<SnapshotEntry>>> {
    let svc = pa(&st)?;
    check_name(&device)?;
    match svc
        .read_op(&device, WorkerOp::Snapshots { scope: Some(scope(&device)) })
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
    match svc
        .read_op(
            &device,
            WorkerOp::Ls {
                snapshot: q.version,
                path,
                scope: Some(scope(&device)),
            },
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
    svc.edit(|r| {
        if !r.devices.contains_key(&device) {
            return Err(not_found(&device));
        }
        let now = now_ms();
        r.restore_codes.retain(|c| c.expires_ms > now);
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
    let devices = svc.edit(|r| {
        let now = now_ms();
        r.restore_codes.retain(|c| c.expires_ms > now);
        let mut devices = Vec::new();
        for (name, d) in r.devices.iter() {
            let code = new_code()?;
            r.restore_codes.push(RestoreCode {
                code_sha: code_hash(&code),
                device: name.clone(),
                expires_ms: now + ADMIN_CODE_TTL_MS,
                batch: Some(batch.clone()),
            });
            devices.push(RestoreChoiceDevice {
                device: name.clone(),
                description: d.settings.description.clone(),
                hostname: d.hostname.clone(),
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
    let out = maint::pair(&svc, &pairing, &settings, &req).await;
    match &out {
        Ok(p) => tracing::info!(target: "rynas::audit", "pc backup: {} paired as {} in group {}", req.hostname, p.device, p.group),
        Err(e) => tracing::warn!("pc backup: pairing of {} failed: {}", req.hostname, e.2),
    }
    out.map(Json)
}

#[derive(Deserialize, Default)]
struct PlanQuery {
    #[serde(default)]
    rev: Option<u64>,
    #[serde(default)]
    wait: Option<u64>,
}

fn plan_of(d: &Dev) -> AgentPlan {
    AgentPlan {
        rev: d.rev,
        plan: d.settings.plan.clone(),
        command: d.command.clone(),
    }
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
        if d.last_seen_ms.is_none_or(|t| seen.saturating_sub(t) > 60_000) {
            svc.touch(|r| {
                if let Some(x) = r.devices.get_mut(&device) {
                    x.last_seen_ms = Some(seen);
                }
            });
        }
        if q.rev != Some(d.rev) || Instant::now() >= deadline {
            return Ok(Json(plan_of(&d)));
        }
        let left = deadline.saturating_duration_since(Instant::now());
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
    s.run_id.truncate(64);
    if let Some(m) = &mut s.message {
        let mut cut = m.len().min(500);
        while !m.is_char_boundary(cut) {
            cut -= 1;
        }
        m.truncate(cut);
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
    let d = svc.device(&code.device)?;
    let g = svc.group(&d.group)?;
    // a key of its own for this session (never the NAS' key): its password stops opening the
    // repository when the session ends and the key file goes
    let (key_id, password) = match maint::add_session_key(&svc, &d.group, &g, &code.device).await {
        Ok(k) => k,
        Err(e) => {
            // the code was not used up: it works again
            let back = code.clone();
            let _ = svc.edit(move |r| {
                r.restore_codes.push(back);
                Ok(())
            });
            return Err(e);
        }
    };
    let token = new_token()?;
    let now = now_ms();
    let session = Session {
        token_sha: sha_hex(token.as_bytes()),
        devices: vec![code.device.clone()],
        created_ms: now,
        expires_ms: now + SESSION_IDLE_MS,
        key_group: d.group.clone(),
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
            group: d.group.clone(),
            description: d.settings.description.clone(),
            hostname: d.hostname.clone(),
            repo_password: password,
        }],
    }))
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
    let mut cut = r.message.len().min(2000);
    while !r.message.is_char_boundary(cut) {
        cut -= 1;
    }
    r.message.truncate(cut);
    let now = now_ms();
    svc.edit(|reg| {
        let d = reg.devices.get_mut(&r.device).ok_or_else(|| not_found(&r.device))?;
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
        assert_eq!(state_of(&d, false, 10 * h), PcState::Waiting);
        assert_eq!(state_of(&d, false, 100 * h), PcState::Critical);
        d.last_ok_ms = Some(100 * h);
        assert_eq!(state_of(&d, false, 101 * h), PcState::Ok);
        assert_eq!(state_of(&d, false, 127 * h), PcState::Late);
        assert_eq!(state_of(&d, false, 173 * h), PcState::Critical);
        assert_eq!(state_of(&d, true, 173 * h), PcState::Running);
        d.history.push_back(PcRun {
            ok: false,
            ..Default::default()
        });
        assert_eq!(state_of(&d, false, 101 * h), PcState::Failed);
        assert!(!ready_of(&d, None).ready);
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
            pair_command("ABCD-EFGH-JKMN", Some("AA:BB:01")),
            "sudo rynas-agent pair {url} ABCD-EFGH-JKMN --fingerprint AA:BB:01"
        );
        assert_eq!(pair_command("C", None), "sudo rynas-agent pair {url} C");
        assert_eq!(pair_command("C", Some("x; rm")), "sudo rynas-agent pair {url} C");
    }
}
