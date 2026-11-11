//! Deduplication of shared folders (D-DEDUP, `docs/phase2/dedup.md`).
//!
//! Offline batch deduplication with oans (package `rynas-oans`, binary
//! `/usr/lib/rynas/oans/oans`): identical files and extents are made to share one copy on
//! disk through the kernel's `FIDEDUPERANGE` ioctl, which compares the bytes itself.
//!
//! - The switch is per shared folder ([`ShareDedup`], inside the share); the **job is per
//!   filesystem**: one run covers every enabled share of a volume, so duplicates are found
//!   across shares. Its settings are the `dedup` configuration key ([`DedupConfig`]).
//! - Runs happen only inside the job's weekly window and are stopped with SIGINT at its end
//!   (work is kept, the next window resumes); other heavy work on the same filesystem
//!   (scrub, replication, backup, storage operations) stops a run and goes first.
//! - The hashfile is a cache: rynas passes the whole command line on every run.
//!
//! | Method | Path | Body | Answer |
//! |---|---|---|---|
//! | GET | `/api/v1/storage/dedup` | — | [`DedupOverview`] |
//! | PUT | `/api/v1/storage/dedup/jobs/{key}` | [`VolumeDedup`] | [`DedupJobView`] (staged with the scheduler key) |
//! | DELETE | `/api/v1/storage/dedup/jobs/{key}` | — | staged (the hashfile stays) |
//! | POST | `/api/v1/storage/dedup/jobs/{key}/estimate` | [`DedupRunRequest`] | 202 `TaskStarted` (`storage.dedup-estimate`) |
//! | POST | `/api/v1/storage/dedup/jobs/{key}/run` | [`DedupRunRequest`] | 202 `TaskStarted` (`storage.dedup`) |
//! | POST | `/api/v1/storage/dedup/jobs/{key}/stop` | — | 204 (SIGINT, the run saves its work) |
//! | GET | `/api/v1/storage/dedup/jobs/{key}/history` | — | `Vec<`[`DedupRecord`]`>`, newest first |
//! | POST | `/api/v1/storage/dedup/jobs/{key}/reset` | [`DedupResetRequest`] | 202 `TaskStarted` (`storage.dedup-reset`) |
//! | POST | `/api/v1/storage/dedup/install` | — | 202 `TaskStarted` (`dedup.install`) |
//!
//! Administrators only. `{key}` is a volume label (`vol1`, `system`) or, for a folder on a
//! filesystem that is no rynas volume (XFS with reflink), `uuid.<filesystem uuid>`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::PendingOp;

/// The configuration key of the per-filesystem jobs.
pub const DEDUP_KEY: &str = "dedup";
/// Hours in the weekly window (Sunday first, `window[day * 24 + hour]`, like the other grids).
pub const WINDOW_CELLS: usize = 168;
/// Prefix of the key of a filesystem that is no rynas volume.
pub const UUID_KEY_PREFIX: &str = "uuid.";
/// The package with the engine, installed on demand.
pub const ENGINE_PACKAGE: &str = "rynas-oans";
/// Where the package puts the engine (not in `PATH`: no clash with another oans or with
/// Debian's duperemove).
pub const ENGINE_PATH: &str = "/usr/lib/rynas/oans/oans";

fn yes() -> bool {
    true
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Deduplication of one shared folder (`Share::dedup`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShareDedup {
    /// The share is a root of its filesystem's job.
    #[serde(default)]
    pub enabled: bool,
    /// gitignore patterns relative to the share (`cache/`, `*.iso`); rynas anchors them at
    /// the share's path. No `!`, no `..`, at most 32.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
    /// Also deduplicate the recycle bin (`#recycle`), excluded by default.
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_recycle: bool,
}

/// The `dedup` key: settings of the per-filesystem jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DedupConfig {
    /// Key: a volume label or `uuid.<filesystem uuid>`.
    #[serde(default)]
    pub jobs: BTreeMap<String, VolumeDedup>,
}

/// Settings of one filesystem's job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeDedup {
    /// Off: no scheduled run (the settings and the hashfile stay; run now still works).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// When runs may happen: 168 characters, `1` = may run, `0` = not.
    #[serde(default = "default_window")]
    pub window: String,
    /// Skip a window when the volume's disks are in standby (never wake them up).
    #[serde(default = "yes")]
    pub only_if_awake: bool,
    /// Files below this size are skipped (`-m`), 4 KiB..=1 GiB.
    #[serde(default = "default_min_file_size")]
    pub min_file_size: u64,
    /// Hashing block size (`-b`): a power of two from 4 KiB to 1 MiB. A change makes the next
    /// run hash everything again.
    #[serde(default = "default_block_size")]
    pub block_size: u32,
    /// Also match parts of extents (`--dedupe-options=partial`): more CPU and a much bigger
    /// hashfile.
    #[serde(default)]
    pub partial: bool,
    /// Skip all-zero blocks (`--skip-zeroes`).
    #[serde(default = "yes")]
    pub skip_zeroes: bool,
    /// `--cpu-threads` (1..=8); `None`: oans chooses.
    #[serde(default = "default_cpu_threads")]
    pub cpu_threads: Option<u8>,
    /// `--io-threads` (1..=8); `None`: oans chooses from the disks.
    #[serde(default)]
    pub io_threads: Option<u8>,
    /// `MemoryMax` of the run (MiB, 256..=16384; `MemoryHigh` is three quarters of it).
    #[serde(default = "default_memory_max_mib")]
    pub memory_max_mib: u32,
}

impl Default for VolumeDedup {
    fn default() -> Self {
        Self {
            enabled: true,
            window: default_window(),
            only_if_awake: true,
            min_file_size: default_min_file_size(),
            block_size: default_block_size(),
            partial: false,
            skip_zeroes: true,
            cpu_threads: default_cpu_threads(),
            io_threads: None,
            memory_max_mib: default_memory_max_mib(),
        }
    }
}

/// Every night from 01:00 to 06:00.
pub fn default_window() -> String {
    (0..WINDOW_CELLS)
        .map(|i| if (1..6).contains(&(i % 24)) { '1' } else { '0' })
        .collect()
}

pub fn default_min_file_size() -> u64 {
    64 << 10
}

pub fn default_block_size() -> u32 {
    128 << 10
}

pub fn default_cpu_threads() -> Option<u8> {
    Some(2)
}

pub fn default_memory_max_mib() -> u32 {
    1024
}

/// A well-formed window: 168 characters `0`/`1`, at least one `1`.
pub fn window_valid(w: &str) -> bool {
    w.len() == WINDOW_CELLS && w.bytes().all(|b| b == b'0' || b == b'1') && w.contains('1')
}

/// A job key: a volume label (the system volume included) or `uuid.<uuid>`.
pub fn job_key_valid(k: &str) -> bool {
    match k.strip_prefix(UUID_KEY_PREFIX) {
        Some(u) => {
            !u.is_empty()
                && u.len() <= 36
                && u.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase() || b == b'-')
        }
        None => crate::validate::place_label(k),
    }
}

/// Checks one exclude pattern of a share ([`ShareDedup::excludes`]); the error is short
/// English.
pub fn exclude_valid(p: &str) -> Result<(), &'static str> {
    let t = p.trim();
    if t.is_empty() || t.len() > 256 {
        return Err("a pattern has 1 to 256 characters");
    }
    if t.contains('!') {
        return Err("negation (!) is not supported");
    }
    if t.chars().any(char::is_control) {
        return Err("control characters are not allowed");
    }
    if t.split('/').any(|c| c == ".." || c == ".") {
        return Err("patterns stay inside the folder (no . or ..)");
    }
    if t.trim_matches('/').is_empty() || t.trim_matches(|c| c == '/' || c == '*').is_empty() {
        return Err("a pattern cannot exclude the whole folder");
    }
    // the engine refuses to run with a malformed pattern: catch it here
    let (mut esc, mut class, mut first) = (false, false, false);
    for c in t.chars() {
        if esc {
            esc = false;
            first = false;
            continue;
        }
        match c {
            '\\' => esc = true,
            '[' if !class => {
                class = true;
                first = true;
                continue;
            }
            '!' | '^' if class && first => continue,
            ']' if class && !first => class = false,
            _ => {}
        }
        first = false;
    }
    if esc {
        return Err("a pattern cannot end with \\");
    }
    if class {
        return Err("a [ without its ]");
    }
    Ok(())
}

/// At most this many extra excludes per share.
pub const MAX_EXCLUDES: usize = 32;

/// Checks and normalizes a share's settings (trimmed patterns, duplicates dropped); `None`
/// when nothing is set (the share leaves the key out).
pub fn normalize_share(d: Option<ShareDedup>) -> Result<Option<ShareDedup>, String> {
    let Some(mut d) = d else { return Ok(None) };
    let mut ex: Vec<String> = Vec::new();
    for p in d.excludes {
        let p = p.trim().to_string();
        exclude_valid(&p).map_err(|e| format!("deduplication exclude {p:?}: {e}"))?;
        if !ex.contains(&p) {
            ex.push(p);
        }
    }
    if ex.len() > MAX_EXCLUDES {
        return Err(format!("at most {MAX_EXCLUDES} deduplication excludes per folder"));
    }
    d.excludes = ex;
    Ok((d != ShareDedup::default()).then_some(d))
}

/// The id of the scheduler task of a job (`dedup-<label>`, `dedup-u<uuid hex>`).
pub fn task_id_of(key: &str) -> String {
    match key.strip_prefix(UUID_KEY_PREFIX) {
        Some(u) => format!("dedup-u{}", u.replace('-', "").chars().take(32).collect::<String>()),
        None => format!("dedup-{key}"),
    }
}

/// `POST .../estimate` and `.../run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DedupRunRequest {
    /// When the run stops at the latest (ms since the epoch, at most 24 h ahead). `None`: the
    /// end of the current window block, or of the next one when outside the window.
    #[serde(default)]
    pub until_ms: Option<u64>,
    /// Estimate only: the shares to look at. Empty: the enabled shares, or every share that
    /// can be deduplicated when none is enabled yet.
    #[serde(default)]
    pub shares: Vec<String>,
}

/// `POST .../reset`: the hashfile is deleted (the next run hashes everything again).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DedupResetRequest {
    /// The job key typed again.
    pub confirm: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DedupKind {
    #[default]
    Run,
    Estimate,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DedupOutcome {
    /// Everything was looked at (exit 0).
    #[default]
    Done,
    /// The window (or the given end) came: the work is saved, the next window goes on.
    WindowEnded,
    /// Stopped for other work on the filesystem ([`DedupRecord::by`]); resumed later.
    Preempted,
    /// Stopped by an administrator.
    Stopped,
    /// Done on less than asked (exit 2): a share was missing, unmounted or elsewhere.
    Partial,
    /// Failed (exit 1, or the engine could not start).
    Failed,
    /// Not started: disks asleep, no share, other work until the window closed.
    Skipped,
}

/// One run in the history (newest first, at most 52).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DedupRecord {
    pub kind: DedupKind,
    pub start_ms: u64,
    pub end_ms: u64,
    pub outcome: DedupOutcome,
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Space freed by this run (logical, as oans counts it).
    #[serde(default)]
    pub reclaimed_bytes: u64,
    #[serde(default)]
    pub files_hashed: Option<u64>,
    /// Files the scan could not read (permission, unreadable, path too long).
    #[serde(default)]
    pub skipped_files: u64,
    /// Preempted by (a task type such as `btrfs.scrub`) or skipped because of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// The shares of the run.
    #[serde(default)]
    pub shares: Vec<String>,
    /// Roots oans could not cover (exit 2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    /// Short English detail (the error of a failed run, the reason of a skip).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

/// The hashfile's figures (`oans --json`), read after each run and kept by rynas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DedupReport {
    pub files_tracked: u64,
    pub logical_bytes: u64,
    pub dup_groups: u64,
    pub dup_files: u64,
    /// Duplicates still to reclaim (a logical upper bound).
    pub reclaimable_bytes: u64,
    pub runs: u64,
    /// Freed by every run of this hashfile (logical).
    pub reclaimed_total_bytes: u64,
    /// The stored job deduplicates (`false` after an estimate only).
    #[serde(default)]
    pub dedupe_configured: Option<bool>,
    #[serde(default)]
    pub readonly_subvols_skipped: u64,
    /// Files the last scan could not read, by cause.
    #[serde(default)]
    pub skipped_last_run: BTreeMap<String, u64>,
    pub read_ms: u64,
}

/// The last estimate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DedupEstimate {
    pub at_ms: u64,
    pub shares: Vec<String>,
    /// Duplicates found (logical).
    pub reclaimable_bytes: u64,
    pub dup_files: u64,
    pub files: u64,
    /// Data looked at (logical).
    pub logical_bytes: u64,
}

/// Progress of a running job (from `--progress=json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DedupProgress {
    /// `scanning`, `hashing`, `dedupe`.
    pub phase: String,
    /// Files (scanning, hashing) or bytes (dedupe) done...
    pub done: u64,
    /// ...out of (0 = unknown).
    pub total: u64,
    #[serde(default)]
    pub bytes_per_sec: Option<u64>,
    #[serde(default)]
    pub eta_s: Option<u64>,
    #[serde(default)]
    pub reclaimed_bytes: u64,
}

/// A job that is running or waiting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DedupRunning {
    pub task_id: String,
    pub kind: DedupKind,
    pub started_ms: u64,
    pub until_ms: u64,
    #[serde(default)]
    pub progress: Option<DedupProgress>,
    /// Waiting for other work on the filesystem to end (its task type).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    /// A stop was asked: the engine is saving its work (up to 5 minutes).
    #[serde(default, skip_serializing_if = "is_false")]
    pub stopping: bool,
}

/// Why a share cannot be deduplicated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DedupBlock {
    /// An encrypted folder: nothing in its ciphertext matches.
    Encrypted,
    /// No copy-on-write: the kernel cannot share its extents.
    Nocow,
    /// Not btrfs or XFS.
    Filesystem,
    /// XFS without reflink (`xfs_info` shows `reflink=0`).
    XfsNoReflink,
    /// The folder is on another filesystem than its volume (a mount inside it).
    OtherFilesystem,
    /// The folder does not exist or its filesystem is not mounted.
    NotMounted,
}

/// A share as the job sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DedupShare {
    pub name: String,
    pub path: String,
    /// The share's switch (applied configuration).
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<DedupBlock>,
    /// A snapshot job covers it: space comes back when older snapshots expire.
    #[serde(default, skip_serializing_if = "is_false")]
    pub snapshots: bool,
    /// Replicated or backed up as a btrfs send: the next transfer may be slower or bigger.
    #[serde(default, skip_serializing_if = "is_false")]
    pub replicated: bool,
    /// The recycle bin is included.
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_recycle: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DedupWarning {
    /// Classic btrfs quotas are on: commits can become slow with many shared extents.
    Qgroups,
    /// An enabled share is replicated: the first transfer after a big run may be slower or
    /// bigger.
    Replicated,
    /// No share of this filesystem is enabled: runs do nothing.
    NoShares,
    /// An enabled share cannot be deduplicated (see its [`DedupShare::block`]).
    SharesBlocked,
    /// The system volume often sits on flash: the hashfile is written there.
    Flash,
}

/// The engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DedupEngineInfo {
    pub installed: bool,
    #[serde(default)]
    pub version: Option<String>,
    /// The installed version is in the range rynas was tested with.
    pub supported: bool,
    /// Tested range, e.g. `1.14.1` and `1.14.x`.
    pub min_version: String,
    pub max_version: String,
    /// The package can be installed from here (an apt source has it, or rynas carries it).
    pub installable: bool,
}

/// One filesystem's job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DedupJobView {
    pub key: String,
    /// The volume label (`None` for a filesystem that is no rynas volume).
    #[serde(default)]
    pub volume: Option<String>,
    /// Where the filesystem is mounted.
    pub mountpoint: String,
    pub fs_type: String,
    /// Settings (staged ones when a change is pending); `None`: the job is off.
    #[serde(default)]
    pub settings: Option<VolumeDedup>,
    #[serde(default)]
    pub pending: Option<PendingOp>,
    /// The scheduler task that starts the runs.
    pub task_id: String,
    pub shares: Vec<DedupShare>,
    #[serde(default)]
    pub running: Option<DedupRunning>,
    #[serde(default)]
    pub last: Option<DedupRecord>,
    #[serde(default)]
    pub estimate: Option<DedupEstimate>,
    #[serde(default)]
    pub report: Option<DedupReport>,
    /// Start of the next window block (ms since the epoch).
    #[serde(default)]
    pub next_window_ms: Option<u64>,
    #[serde(default)]
    pub warnings: Vec<DedupWarning>,
    pub hashfile: String,
}

/// `GET /api/v1/storage/dedup`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DedupOverview {
    pub engine: DedupEngineInfo,
    /// Every place with a filesystem that can be deduplicated, and every other filesystem
    /// with an enabled share or a job.
    pub jobs: Vec<DedupJobView>,
    /// Shares on no job's filesystem (with the reason in [`DedupShare::block`]).
    #[serde(default)]
    pub other_shares: Vec<DedupShare>,
    /// Folders never deduplicated (shown read-only), relative to each share.
    pub always_excluded: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_window_is_every_night_one_to_six() {
        let w = default_window();
        assert!(window_valid(&w));
        assert_eq!(w.matches('1').count(), 7 * 5);
        assert_eq!(&w[0..24], "011111000000000000000000");
    }

    #[test]
    fn keys_and_task_ids() {
        assert!(job_key_valid("vol1"));
        assert!(job_key_valid("system"));
        assert!(job_key_valid("uuid.0f6e2c1a-1b2c-4d5e-8f90-123456789abc"));
        for bad in ["", "Vol1", "uuid.", "uuid.XYZ", "uuid.0F6E", "../x", "uuid.a/b"] {
            assert!(!job_key_valid(bad), "{bad}");
        }
        assert_eq!(task_id_of("vol1"), "dedup-vol1");
        assert_eq!(
            task_id_of("uuid.0f6e2c1a-1b2c-4d5e-8f90-123456789abc"),
            "dedup-u0f6e2c1a1b2c4d5e8f90123456789abc"
        );
    }

    #[test]
    fn malformed_patterns() {
        for ok in ["cache/", "*.iso", "a[bc]d", "[]x]y", "x\\[", "a\\\\"] {
            assert_eq!(exclude_valid(ok), Ok(()), "{ok}");
        }
        for bad in ["cache\\", "a[bc", "[]", "x[\\]"] {
            assert!(exclude_valid(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn old_documents_get_the_defaults() {
        let v: VolumeDedup = serde_json::from_str("{}").unwrap();
        assert_eq!(v, VolumeDedup::default());
        let s: ShareDedup = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(s.enabled && !s.include_recycle && s.excludes.is_empty());
    }
}
