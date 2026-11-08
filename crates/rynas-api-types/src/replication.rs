//! Snapshot replication (`/api/v1/snapshots/replication/...`): a second copy of a snapshot
//! job's snapshots on another btrfs volume of this NAS or on another Linux host over SSH,
//! with its own GFS retention.
//!
//! A replica target belongs to its snapshot job ([`crate::snapshots::SnapshotJob::replicas`]):
//! btrbk sees the job's snapshots and its targets in one configuration, so it picks the
//! incremental parent itself and never prunes the latest snapshot/backup pair. A target runs
//! after each snapshot run of its job, or on its own schedule (scheduler task
//! `replica-<job>-<id>`, action `replicate`). Replicas keep btrbk's names
//! (`<job>.YYYYMMDDTHHMM`); btrbk's `target_preserve` only ever deletes those.

use serde::{Deserialize, Serialize};

use crate::PendingOp;
use crate::scheduler::{TaskRun, TaskTrigger};
use crate::snapshots::{EntryKind, Retention, RetentionPreset};

/// One replica target of a snapshot job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaTarget {
    /// `[a-z0-9][a-z0-9-]{0,23}`, unique within the job; btrbk group `replica-<id>`.
    pub id: String,
    /// Off: kept in the configuration, never run (its scheduler task is off too).
    #[serde(default = "yes")]
    pub enabled: bool,
    pub destination: ReplicaDestination,
    /// GFS retention of the replicas (`target_preserve`, `target_preserve_min`),
    /// independent of the source.
    #[serde(default = "default_target_retention")]
    pub retention: Retention,
    /// Compression of the send stream (`stream_compress`), remote targets only.
    #[serde(default)]
    pub compress: StreamCompress,
    /// Bandwidth limit in KiB/s (`rate_limit`); 0 or missing: none.
    #[serde(default)]
    pub rate_limit_kib: u32,
    /// Runs after each snapshot run of the job (default), or on its own scheduler task.
    #[serde(default = "yes")]
    pub after_snapshots: bool,
}

fn yes() -> bool {
    true
}

/// 14 daily, 8 weekly, 12 monthly, 2 yearly; everything of the last day.
pub fn default_target_retention() -> Retention {
    Retention {
        preset: None,
        hourly: 0,
        daily: 14,
        weekly: 8,
        monthly: 12,
        yearly: 2,
        min: "1d".into(),
    }
}

/// The default retention of a new target: [`default_target_retention`]; presets are the
/// snapshot ones.
pub fn target_retention(preset: Option<RetentionPreset>) -> Retention {
    preset.map_or_else(default_target_retention, RetentionPreset::retention)
}

/// Where the replicas go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplicaDestination {
    /// A directory on another btrfs volume of this NAS (e.g. `/srv/vol2/.replicas/docs`);
    /// created at the first run.
    Local { path: String },
    /// A directory on a btrfs filesystem of another host, reached with the replication key
    /// (`ssh://user@host:port/path`).
    Ssh {
        host: String,
        #[serde(default = "default_port")]
        port: u16,
        #[serde(default = "default_ssh_user")]
        user: String,
        path: String,
        /// A non-root user runs btrfs through `sudo -n` (btrbk `backend_remote
        /// btrfs-progs-sudo`, `ssh_filter_btrbk.sh --sudo`).
        #[serde(default)]
        sudo: bool,
        /// The host keys trusted for this host (`ssh-keyscan` lines, confirmed by the
        /// administrator); written to rynas's own known_hosts.
        #[serde(default)]
        host_keys: Vec<String>,
    },
}

fn default_port() -> u16 {
    22
}
fn default_ssh_user() -> String {
    "root".into()
}

impl ReplicaDestination {
    /// btrbk's target: the path, or `ssh://host:port/path`.
    pub fn url(&self) -> String {
        match self {
            ReplicaDestination::Local { path } => path.clone(),
            ReplicaDestination::Ssh {
                host, port, path, ..
            } => {
                let h = if host.contains(':') {
                    format!("[{host}]")
                } else {
                    host.clone()
                };
                format!("ssh://{h}:{port}{path}")
            }
        }
    }

    /// The path on the target machine.
    pub fn path(&self) -> &str {
        match self {
            ReplicaDestination::Local { path } | ReplicaDestination::Ssh { path, .. } => path,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, ReplicaDestination::Local { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StreamCompress {
    #[default]
    No,
    Zstd,
    Lz4,
    Gzip,
}

impl StreamCompress {
    /// The `stream_compress` value.
    pub fn btrbk(self) -> &'static str {
        match self {
            StreamCompress::No => "no",
            StreamCompress::Zstd => "zstd",
            StreamCompress::Lz4 => "lz4",
            StreamCompress::Gzip => "gzip",
        }
    }
}

/// `POST /api/v1/snapshots/replication`, `PUT .../{job}/{id}`: a target and, for a target
/// with its own schedule, the trigger of its scheduler task (missing: unchanged, `daily`
/// for a new one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaRequest {
    pub job: String,
    pub target: ReplicaTarget,
    #[serde(default)]
    pub schedule: Option<TaskTrigger>,
}

/// A target as the session user sees it, with what rynas knows of its last runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaView {
    pub job: String,
    #[serde(default)]
    pub share: Option<String>,
    pub target: ReplicaTarget,
    /// btrbk's target (`ssh://host:port/path` or the local path).
    pub url: String,
    /// Scheduler task of a target with its own schedule.
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub schedule: Option<TaskTrigger>,
    #[serde(default)]
    pub pending: Option<PendingOp>,
    #[serde(default)]
    pub next_run_ms: Option<u64>,
    #[serde(default)]
    pub status: ReplicaStatus,
    /// Problems found without connecting (local targets: not btrfs, same filesystem as the
    /// source, volume not mounted).
    #[serde(default)]
    pub warnings: Vec<ReplicaWarning>,
    /// Remote targets: the line to add to the remote user's `authorized_keys` (empty until
    /// the replication key exists).
    #[serde(default)]
    pub authorized_keys_line: String,
}

/// What rynas recorded about the runs of a target (`<state>/replication/<job>/<id>.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReplicaStatus {
    #[serde(default)]
    pub last_ok_ms: Option<u64>,
    #[serde(default)]
    pub last_attempt_ms: Option<u64>,
    /// The last run failed.
    #[serde(default)]
    pub failing: bool,
    /// Duration of the last successful run, seconds.
    #[serde(default)]
    pub duration_s: Option<u64>,
    /// Replicas sent by the last successful run (names).
    #[serde(default)]
    pub transferred: Vec<String>,
    /// The newest replica sent so far.
    #[serde(default)]
    pub latest: Option<String>,
    /// Replicas btrbk deleted on the target (retention) in the last run.
    #[serde(default)]
    pub pruned: u32,
    /// The last error (English, btrbk's or ssh's words).
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub hint: Option<ReplicaHint>,
    #[serde(default)]
    pub last_run: Option<TaskRun>,
}

/// What a failure most likely needs (the UI shows the fix).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaHint {
    /// The remote refuses the key: add the authorized_keys line.
    SshKey,
    /// The host key changed or was never confirmed.
    HostKey,
    /// Host down, name unknown, port closed.
    Unreachable,
    /// `ssh_filter_btrbk.sh` refused a command (path, options or an old filter).
    Filter,
    /// The target directory does not exist.
    TargetMissing,
    /// The target is not on btrfs.
    NotBtrfs,
    NoSpace,
    /// Another run of the job held its lock for too long.
    Busy,
    /// btrbk is not installed (or too old).
    Btrbk,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaWarning {
    /// The target is not on btrfs: send/receive needs btrfs (file backups are restic's).
    NotBtrfs,
    /// The target is on the filesystem of the source: not a second copy.
    SameFilesystem,
    /// No volume is mounted there: the directory would land on the system disk.
    NotMounted,
    /// A remote target without a confirmed host key.
    NoHostKey,
}

/// `GET /api/v1/snapshots/replication/ssh-key`; `POST` creates the key pair when missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReplicationKey {
    /// `ssh-ed25519 AAAA… rynas-replication@<host>`; empty when there is none yet.
    pub public_key: String,
    #[serde(default)]
    pub fingerprint: String,
    /// The private key, root:root 0600.
    pub path: String,
}

/// `POST /api/v1/snapshots/replication/host-keys`: the keys a host presents now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostKeyRequest {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HostKey {
    /// `host keytype base64`, as `ssh-keyscan` prints it.
    pub line: String,
    /// `SHA256:…`, to compare with `ssh-keygen -lf` on the remote host.
    pub fingerprint: String,
}

/// One replica on a local target (`GET .../{job}/{id}/snapshots`), newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReplicaSnapshot {
    pub name: String,
    pub path: String,
    pub created_ms: u64,
    pub readonly: bool,
    #[serde(default)]
    pub received_uuid: Option<String>,
    /// An interrupted receive (no `received_uuid`, or writable): removed by the next run.
    pub incomplete: bool,
}

/// `GET .../{job}/{id}/snapshots/{name}/browse?path=`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReplicaListing {
    pub name: String,
    pub path: String,
    pub entries: Vec<ReplicaEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReplicaEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size_bytes: u64,
    pub mtime_ms: u64,
}

/// `POST .../{job}/{id}/snapshots/{name}/restore` (202, task `replication.restore`): copies a
/// file or folder of the replica next to the original in the job's live subvolume
/// (`<name> (restored YYYY-MM-DD HHMM)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaRestoreRequest {
    pub path: String,
}

/// `POST .../{job}/{id}/snapshots/{name}/promote` (202, task `replication.promote`): a
/// writable snapshot of the replica as a new subvolume `<volume>/<folder>` on the target's
/// volume (disaster recovery). The replica itself stays read-only, so later incremental
/// transfers keep working.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromoteRequest {
    /// New folder name at the top of the target's volume.
    pub folder: String,
    /// Must be the replica name (400 `confirmation_required` otherwise).
    #[serde(default)]
    pub confirm: String,
}
