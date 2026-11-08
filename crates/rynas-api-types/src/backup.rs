//! Backups to another NAS, a local or USB disk and the cloud (D-BACKUP): restic-format
//! repositories written by `rynas-backup-worker` (rustic_core) under `/api/v1/backup/restic/…`.
//!
//! Two groups of types:
//! - the configuration (`backup` key) and the API: [`BackupJob`], [`Destination`],
//!   [`Retention`], views and requests;
//! - the wire protocol between rynasd and the worker: one [`WorkerRequest`] as JSON on the
//!   worker's standard input, then [`WorkerEvent`] lines (JSON) on its standard output.
//!
//! Secrets (repository password, cloud keys, SFTP private key, rclone configuration) never
//! appear in these types once stored: requests carry them in [`DestinationSecrets`], rynasd
//! seals them with its age key into [`BackupJob::sealed`], and views never include them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::PendingOp;
use crate::scheduler::{NotifyPolicy, TaskRun, TaskTrigger};

/// Configuration key of the jobs.
pub const BACKUP_KEY: &str = "backup";

/// Where a job writes its repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Destination {
    /// A folder on this NAS: a USB disk, another volume (`/media/usb1/backup`).
    Local { path: String },
    /// A restic REST server (`rest-server`, also on another NAS): `https://nas2:8000/repo`.
    /// The password (when the server asks for one) is a secret.
    Rest {
        url: String,
        #[serde(default)]
        user: String,
    },
    /// SFTP with a key: rynas generates the key pair, the public key goes into the other
    /// side's `authorized_keys`.
    Sftp {
        host: String,
        #[serde(default = "default_ssh_port")]
        port: u16,
        user: String,
        /// Folder of the repository on the server.
        path: String,
    },
    /// S3 or an S3-compatible service (MinIO, Wasabi, Garage…). The secret key is a secret.
    S3 {
        /// Empty: AWS.
        #[serde(default)]
        endpoint: String,
        #[serde(default)]
        region: String,
        bucket: String,
        /// Folder inside the bucket.
        #[serde(default)]
        prefix: String,
        access_key_id: String,
    },
    /// Backblaze B2 (native API). The application key is a secret.
    B2 {
        bucket: String,
        bucket_id: String,
        #[serde(default)]
        prefix: String,
        key_id: String,
    },
    /// Any rclone remote (Google Drive, OneDrive, Dropbox…): `remote:path`. The remote's
    /// rclone configuration (with its OAuth token) is a secret.
    Rclone { remote: String },
}

fn default_ssh_port() -> u16 {
    22
}

impl Destination {
    pub fn kind(&self) -> &'static str {
        match self {
            Destination::Local { .. } => "local",
            Destination::Rest { .. } => "rest",
            Destination::Sftp { .. } => "sftp",
            Destination::S3 { .. } => "s3",
            Destination::B2 { .. } => "b2",
            Destination::Rclone { .. } => "rclone",
        }
    }

    /// Off this machine (counts as the offsite copy).
    pub fn offsite(&self) -> bool {
        !matches!(self, Destination::Local { .. })
    }

    /// One line for lists: "s3 · bucket/prefix".
    pub fn summary(&self) -> String {
        match self {
            Destination::Local { path } => path.clone(),
            Destination::Rest { url, .. } => url.clone(),
            Destination::Sftp {
                host, user, path, ..
            } => format!("{user}@{host}:{path}"),
            Destination::S3 { bucket, prefix, .. } | Destination::B2 { bucket, prefix, .. } => {
                if prefix.is_empty() {
                    bucket.clone()
                } else {
                    format!("{bucket}/{prefix}")
                }
            }
            Destination::Rclone { remote } => remote.clone(),
        }
    }
}

/// Secret fields given with a request; empty fields keep the stored value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DestinationSecrets {
    /// REST password, S3 secret key or B2 application key.
    #[serde(default)]
    pub password: String,
    /// rclone configuration of the remote (`[name]` section with its token).
    #[serde(default)]
    pub rclone_config: String,
}

/// What a job backs up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackupSource {
    /// A shared folder by name.
    Share { name: String },
    /// Any folder (e.g. the Docker apps folder `/volume1/docker`).
    Folder { path: String },
    /// rynas' own configuration: a `.rynasbak` export encrypted with the repository password.
    Config,
}

/// Snapshots to keep (0: that rule is off). At least one rule must be on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    #[serde(default)]
    pub last: u32,
    #[serde(default)]
    pub hourly: u32,
    #[serde(default)]
    pub daily: u32,
    #[serde(default)]
    pub weekly: u32,
    #[serde(default)]
    pub monthly: u32,
    #[serde(default)]
    pub yearly: u32,
}

impl Retention {
    /// Smart recycle: a week of daily copies, a month of weeklies, a year of monthlies.
    pub const SMART: Retention = Retention {
        last: 3,
        hourly: 24,
        daily: 7,
        weekly: 4,
        monthly: 12,
        yearly: 3,
    };

    pub fn is_empty(&self) -> bool {
        self.last == 0
            && self.hourly == 0
            && self.daily == 0
            && self.weekly == 0
            && self.monthly == 0
            && self.yearly == 0
    }
}

impl Default for Retention {
    fn default() -> Self {
        Retention::SMART
    }
}

/// Repository maintenance after the backups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Maintenance {
    /// Prune (free the space of forgotten snapshots) at most every this many days.
    #[serde(default = "default_prune_days")]
    pub prune_every_days: u32,
    /// Integrity check (structure and index) at most every this many days; 0: never.
    #[serde(default = "default_check_days")]
    pub check_every_days: u32,
    /// Share of the data read back and verified by each scheduled check (0–100 %).
    #[serde(default = "default_read_pct")]
    pub check_read_percent: u8,
}

fn default_prune_days() -> u32 {
    7
}
fn default_check_days() -> u32 {
    7
}
fn default_read_pct() -> u8 {
    2
}

impl Default for Maintenance {
    fn default() -> Self {
        Maintenance {
            prune_every_days: default_prune_days(),
            check_every_days: default_check_days(),
            check_read_percent: default_read_pct(),
        }
    }
}

/// A backup job (one repository).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupJob {
    /// `[a-z0-9][a-z0-9-]{0,31}`; also the snapshot tag `rynas-job:<name>`.
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub sources: Vec<BackupSource>,
    pub destination: Destination,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub maintenance: Maintenance,
    /// Glob patterns left out (`*.tmp`, `**/cache`), on top of the built-in ones
    /// (`#recycle`, `.snapshots`, `@eaDir`).
    #[serde(default)]
    pub excludes: Vec<String>,
    /// Read each source from a temporary read-only btrfs snapshot when it is a subvolume.
    #[serde(default = "yes")]
    pub use_snapshot: bool,
    /// Docker stacks stopped while their data is read (or while the snapshot is taken).
    #[serde(default)]
    pub stop_stacks: Vec<String>,
    /// Upload limit in KiB/s (0: none). Applies to S3, B2 and SFTP (OpenDAL) and rclone.
    #[serde(default)]
    pub bandwidth_kib: u32,
    /// The sealed secrets (age, base64): never sent by the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed: Option<String>,
    /// The SFTP public key (OpenSSH line), shown to put in `authorized_keys`.
    #[serde(default)]
    pub ssh_public_key: String,
}

fn yes() -> bool {
    true
}

/// The `backup` configuration document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BackupConfig {
    #[serde(default)]
    pub jobs: Vec<BackupJob>,
}

/// `POST /api/v1/backup/restic/jobs` and `PUT …/jobs/{name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupJobRequest {
    pub job: BackupJob,
    #[serde(default)]
    pub secrets: DestinationSecrets,
    /// The repository password: empty generates one (only when creating). A job that points
    /// at an existing repository must give its password.
    #[serde(default)]
    pub repository_password: String,
    #[serde(default)]
    pub schedule: Option<TaskTrigger>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub notify: Option<NotifyPolicy>,
}

/// Answer of a create: the job, and the generated repository password, shown once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupJobCreated {
    pub view: BackupJobView,
    /// Only when rynas generated it: the user must save it now.
    #[serde(default)]
    pub repository_password: Option<String>,
}

/// State of a job kept by rynasd after each run (`<state>/backup/<name>.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BackupStatus {
    #[serde(default)]
    pub last_ok_ms: Option<u64>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_end_ms: Option<u64>,
    /// Snapshots of this job in the repository after the last run.
    #[serde(default)]
    pub snapshots: u32,
    /// Size of the repository (all its files) after the last run.
    #[serde(default)]
    pub repository_bytes: Option<u64>,
    /// Data read from the sources by the last backup.
    #[serde(default)]
    pub source_bytes: Option<u64>,
    /// New data the last backup added (after dedup, before compression).
    #[serde(default)]
    pub added_bytes: Option<u64>,
    #[serde(default)]
    pub last_prune_ms: Option<u64>,
    #[serde(default)]
    pub last_check_ms: Option<u64>,
    /// `true`: the last check found no error.
    #[serde(default)]
    pub last_check_ok: Option<bool>,
    /// Next slice of the rotating read-data check (1..=12).
    #[serde(default)]
    pub check_slice: u32,
}

/// A job as the session user sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupJobView {
    /// Without `sealed`.
    pub job: BackupJob,
    pub task_id: String,
    #[serde(default)]
    pub schedule: Option<TaskTrigger>,
    pub enabled: bool,
    #[serde(default)]
    pub notify: Option<NotifyPolicy>,
    #[serde(default)]
    pub pending: Option<PendingOp>,
    #[serde(default)]
    pub status: BackupStatus,
    #[serde(default)]
    pub next_run_ms: Option<u64>,
    #[serde(default)]
    pub last_run: Option<TaskRun>,
    /// A backup task of this job is running (its UPID).
    #[serde(default)]
    pub running_task: Option<String>,
    /// Secrets stored for the destination (to say "stored, leave empty to keep").
    #[serde(default)]
    pub has_destination_secret: bool,
    #[serde(default)]
    pub has_rclone_config: bool,
}

/// `POST /api/v1/backup/restic/test`: can rynas reach the destination with these settings?
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRequest {
    pub destination: Destination,
    #[serde(default)]
    pub secrets: DestinationSecrets,
    /// An existing job whose stored secrets fill the empty fields (and its SSH key).
    #[serde(default)]
    pub job: Option<String>,
    /// To tell whether an existing repository opens with it.
    #[serde(default)]
    pub repository_password: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    /// Reachable, no repository yet: the first run creates it.
    Empty,
    /// A repository is there and the password opens it.
    Opens,
    /// A repository is there; the password was not given or is wrong.
    Locked,
    /// Not reachable (the message says why).
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestResult {
    pub outcome: TestOutcome,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub snapshots: Option<u32>,
}

/// `POST /api/v1/backup/restic/ssh-key`: a new key pair for an SFTP destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshKeyCreated {
    /// To paste into the server's `~/.ssh/authorized_keys`.
    pub public_key: String,
    /// A handle for the job request (the private key stays in rynasd).
    pub key_id: String,
}

/// A snapshot in a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SnapshotEntry {
    /// Full id (64 hex); lists show the first 8.
    pub id: String,
    pub time_ms: u64,
    pub hostname: String,
    pub paths: Vec<String>,
    pub tags: Vec<String>,
    #[serde(default)]
    pub files: Option<u64>,
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub added_bytes: Option<u64>,
}

/// One entry of a snapshot folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NodeEntry {
    pub name: String,
    /// Path inside the snapshot (`/volume1/docs/a.txt`).
    pub path: String,
    pub dir: bool,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime_ms: Option<u64>,
}

/// Where a restore writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RestoreTarget {
    /// Back where it was: existing files with other contents are replaced.
    Original,
    /// Into this folder (created when missing), keeping the snapshot's path below it.
    Folder { path: String },
}

/// `POST /api/v1/backup/restic/jobs/{name}/restore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreRequest {
    pub snapshot: String,
    /// Paths inside the snapshot (files or folders); empty: everything.
    #[serde(default)]
    pub paths: Vec<String>,
    pub target: RestoreTarget,
}

/// `POST /api/v1/backup/restic/jobs/{name}/check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CheckRequest {
    /// Share of the data read back (0: structure only, 100: everything).
    #[serde(default)]
    pub read_percent: u8,
}

/// `POST /api/v1/backup/restic/jobs/{name}/preview-retention`: what forget would remove now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RetentionPreview {
    pub keep: Vec<SnapshotEntry>,
    pub remove: Vec<SnapshotEntry>,
}

// ---- the worker protocol ----

/// How the worker reaches a repository: a rustic backend string and its options, plus the
/// password and environment. Built by rynasd from a [`Destination`] and the unsealed secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RepoSpec {
    /// `/media/usb/backup`, `rest:https://…`, `opendal:s3`, `rclone:remote:path`.
    pub repository: String,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    pub password: String,
    /// Files the worker writes (0600, in its private folder) before opening the repository,
    /// by option or environment name: `key` (SFTP private key), `RCLONE_CONFIG`.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Environment of the worker's children (rclone).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A source as the worker reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSource {
    /// Where the data is read (the temporary snapshot, or the folder itself).
    pub read: String,
    /// The path recorded in the snapshot (the folder's real path), so that the parent is
    /// found and restores go back to it.
    pub as_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WorkerOp {
    /// Opens the repository, or says it is empty or locked ([`WorkerEvent::Test`]).
    Test,
    /// Creates the repository when there is none.
    Init,
    /// One snapshot per source, tagged.
    Backup {
        sources: Vec<WorkerSource>,
        tags: Vec<String>,
        host: String,
        excludes: Vec<String>,
    },
    /// Removes the snapshots with `tag` that the retention does not keep (per path).
    Forget {
        tag: String,
        retention: Retention,
        dry_run: bool,
    },
    Prune,
    /// `read_percent` 0: structure only; `slice` `(n, 12)` reads the n-th twelfth.
    Check {
        read_percent: u8,
        #[serde(default)]
        slice: Option<(u32, u32)>,
    },
    Snapshots {
        #[serde(default)]
        tag: Option<String>,
    },
    Ls {
        snapshot: String,
        path: String,
    },
    Restore {
        snapshot: String,
        paths: Vec<String>,
        /// `None`: back to the original place.
        target: Option<String>,
    },
    /// Writes one file of a snapshot to `out`.
    Dump {
        snapshot: String,
        path: String,
        out: String,
    },
    /// Size of the repository.
    Stats,
}

/// Standard input of the worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRequest {
    pub repo: RepoSpec,
    pub op: WorkerOp,
}

/// One line of the worker's standard output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WorkerEvent {
    /// A step started ("reading index", "backing up /volume1/docs").
    Step { title: String },
    /// Progress of the current step (`total` 0: unknown).
    Progress {
        done: u64,
        total: u64,
        #[serde(default)]
        bytes: bool,
    },
    /// A line for the task log.
    Log { text: String },
    Test {
        outcome: TestOutcome,
        #[serde(default)]
        snapshots: Option<u32>,
    },
    Initialized { created: bool },
    BackedUp {
        snapshot: SnapshotEntry,
    },
    Forgot {
        keep: Vec<SnapshotEntry>,
        remove: Vec<SnapshotEntry>,
    },
    Pruned {
        removed_packs: u64,
        freed_bytes: u64,
    },
    Checked {
        errors: Vec<String>,
        warnings: Vec<String>,
    },
    Snapshots { list: Vec<SnapshotEntry> },
    Entries { list: Vec<NodeEntry> },
    Restored { files: u64, bytes: u64 },
    Dumped { bytes: u64 },
    Stats { bytes: u64, files: u64 },
    /// The operation failed (the last line).
    Error { message: String },
}
