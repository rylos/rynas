//! Snapshots (`/api/v1/snapshots/...`): btrbk snapshot jobs with GFS retention, listing,
//! browsing, restore, rollback and deletion.
//!
//! Configuration key `snapshots` ([`SnapshotsConfig`]): one btrbk configuration per job
//! (`/etc/rynas/btrbk/<job>.conf`). The schedule of a job is a task of the Task Scheduler
//! (`snapshot-<job>`, action `snapshot`) staged together with the job. Snapshot names are
//! btrbk's: `<job>.YYYYMMDDTHHMM` (local time, `_N` suffix on collision).

use serde::{Deserialize, Serialize};

use crate::PendingOp;
use crate::scheduler::{TaskRun, TaskTrigger};

/// The `snapshots` configuration key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SnapshotsConfig {
    #[serde(default)]
    pub jobs: Vec<SnapshotJob>,
    /// Restore points of the system taken before apt runs dpkg.
    #[serde(default)]
    pub system: SystemSnapshots,
}

/// Snapshots of one btrfs subvolume (usually a share).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotJob {
    /// `[A-Za-z0-9][A-Za-z0-9_-]{0,47}`, unique, and the snapshot name prefix
    /// (`snapshot_name`). Cannot change (a `PUT` may leave it empty).
    #[serde(default)]
    pub name: String,
    /// Share whose path is the subvolume; its SMB "previous versions" point at this job.
    #[serde(default)]
    pub share: Option<String>,
    /// Absolute path of the subvolume (filled from the share when empty).
    #[serde(default)]
    pub subvolume: String,
    /// Absolute directory of the snapshots, on the same filesystem; default
    /// `<parent of subvolume>/.snapshots/<name>`.
    #[serde(default)]
    pub snapshot_dir: String,
    #[serde(default)]
    pub retention: Retention,
    /// Second copies of the snapshots on other volumes or hosts
    /// ([`crate::replication`]); changed through `/api/v1/snapshots/replication`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replicas: Vec<crate::replication::ReplicaTarget>,
}

/// GFS retention, as btrbk's `snapshot_preserve` / `snapshot_preserve_min`. With a
/// `preset`, the counts are the preset's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    #[serde(default)]
    pub preset: Option<RetentionPreset>,
    /// Hourly snapshots kept (the first of each hour).
    #[serde(default)]
    pub hourly: u32,
    /// The first of each day.
    #[serde(default)]
    pub daily: u32,
    /// The first of each week (Sunday).
    #[serde(default)]
    pub weekly: u32,
    #[serde(default)]
    pub monthly: u32,
    #[serde(default)]
    pub yearly: u32,
    /// Everything younger than this is kept whatever the counts: `latest` (only the newest),
    /// `all`, or `<n>h|d|w|m|y`.
    #[serde(default = "default_min")]
    pub min: String,
}

fn default_min() -> String {
    "1d".into()
}

impl Default for Retention {
    fn default() -> Self {
        RetentionPreset::Standard.retention()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionPreset {
    /// 24 hourly, 14 daily, 8 weekly, 12 monthly; everything of the last day.
    Standard,
    /// 24 hourly, 7 daily, 4 weekly; only the latest is always kept.
    Light,
    /// 48 hourly, 31 daily, 12 weekly, 24 monthly, 5 yearly; everything of the last 2 days.
    Long,
}

impl RetentionPreset {
    pub fn retention(self) -> Retention {
        let (hourly, daily, weekly, monthly, yearly, min) = match self {
            RetentionPreset::Standard => (24, 14, 8, 12, 0, "1d"),
            RetentionPreset::Light => (24, 7, 4, 0, 0, "latest"),
            RetentionPreset::Long => (48, 31, 12, 24, 5, "2d"),
        };
        Retention {
            preset: Some(self),
            hourly,
            daily,
            weekly,
            monthly,
            yearly,
            min: min.into(),
        }
    }
}

/// Restore points of the root filesystem, taken by `rynasctl snapshot system` from the apt
/// hook (`DPkg::Pre-Invoke`), skipped when nothing changed since the last one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemSnapshots {
    pub enabled: bool,
    /// Subvolume to snapshot (the root filesystem).
    #[serde(default = "default_root")]
    pub subvolume: String,
    /// Where the snapshots go, on the same filesystem.
    #[serde(default = "default_system_dir")]
    pub snapshot_dir: String,
    /// Name prefix (`<prefix>.YYYYMMDDTHHMM`).
    #[serde(default = "default_system_prefix")]
    pub prefix: String,
    /// Restore points kept (the oldest beyond this are deleted after a new one).
    #[serde(default = "default_keep")]
    pub keep: u32,
}

fn default_root() -> String {
    "/".into()
}
fn default_system_dir() -> String {
    "/.snapshots/rootfs".into()
}
fn default_system_prefix() -> String {
    "rootfs".into()
}
fn default_keep() -> u32 {
    10
}

impl Default for SystemSnapshots {
    fn default() -> Self {
        Self {
            enabled: false,
            subvolume: default_root(),
            snapshot_dir: default_system_dir(),
            prefix: default_system_prefix(),
            keep: default_keep(),
        }
    }
}

/// `POST /api/v1/snapshots/jobs`, `PUT /api/v1/snapshots/jobs/{name}`: the job and its
/// schedule (the scheduler task `snapshot-<name>`); a missing `schedule` keeps the current
/// one (`hourly` for a new job).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotJobRequest {
    #[serde(flatten)]
    pub job: SnapshotJob,
    #[serde(default)]
    pub schedule: Option<TaskTrigger>,
    /// Timer of the scheduler task on or off; missing: unchanged (on for a new job).
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// A job as the session user sees it (staged version if any) with its state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotJobView {
    pub job: SnapshotJob,
    #[serde(default)]
    pub schedule: Option<TaskTrigger>,
    /// Scheduler task running the job.
    pub task_id: String,
    /// The task's timer is on.
    pub enabled: bool,
    #[serde(default)]
    pub pending: Option<PendingOp>,
    /// `/etc/rynas/btrbk/<name>.conf`.
    pub config_path: String,
    /// Snapshots on disk (0 when the directory cannot be read).
    pub snapshots: u32,
    #[serde(default)]
    pub latest_ms: Option<u64>,
    #[serde(default)]
    pub next_run_ms: Option<u64>,
    #[serde(default)]
    pub last_run: Option<TaskRun>,
}

/// `POST /api/v1/snapshots/jobs/preview-retention`: what `btrbk prune` would delete now
/// with this job's retention (the job need not be saved).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPreviewRequest {
    pub job: SnapshotJob,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RetentionPreview {
    /// Snapshot names kept.
    pub keep: Vec<String>,
    /// Snapshot names btrbk would delete.
    pub delete: Vec<String>,
}

/// One snapshot (`GET /api/v1/snapshots?share=` or `?job=`), newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SnapshotInfo {
    /// The snapshot name: `<job>.<timestamp>`; the `{id}` of the other endpoints.
    pub id: String,
    pub job: String,
    #[serde(default)]
    pub share: Option<String>,
    /// Absolute path of the snapshot.
    pub path: String,
    /// The time in the name (local time of the NAS).
    pub created_ms: u64,
    pub readonly: bool,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub parent_uuid: Option<String>,
    #[serde(default)]
    pub received_uuid: Option<String>,
    /// Whether it may be deleted, and why not.
    pub deletion: SnapshotDeletion,
    /// Why a system restore point was taken, as given to `rynasctl snapshot system --reason`
    /// (`apt`, `manual`, `before rollback` or free text); the UI translates `reason_code`.
    #[serde(default)]
    pub reason: Option<String>,
    /// `reason` as a code; `other` for free text, shown as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<RestorePointReason>,
}

/// Why a system restore point was taken ([`SnapshotInfo::reason_code`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestorePointReason {
    /// Taken by the apt hook before dpkg changed packages.
    Apt,
    /// Taken by hand (`rynasctl snapshot system` without `--reason`).
    Manual,
    /// The system replaced by a rollback, kept so the rollback can be undone.
    BeforeRollback,
    /// Free text given with `--reason`, or a code this client does not know.
    #[serde(other)]
    Other,
}

/// Whether deleting a snapshot is allowed (chain-safe delete).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotDeletion {
    #[default]
    Allowed,
    /// The newest snapshot of the job: btrbk's base for the next incremental send and for
    /// "previous versions"; it goes when a newer one exists.
    Latest,
    /// Not read-only: not a btrbk snapshot (or an interrupted receive), left alone.
    Writable,
    /// A directory with a snapshot name that is not a subvolume.
    NotSubvolume,
}

/// `GET /api/v1/snapshots/{id}/browse?path=`: a directory inside a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SnapshotListing {
    pub id: String,
    /// Relative path inside the snapshot (`""`: its top).
    pub path: String,
    pub entries: Vec<SnapshotEntry>,
    /// More than 10 000 entries: the first ones by name.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SnapshotEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size_bytes: u64,
    pub mtime_ms: u64,
    /// Permission bits (`0o7777`).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Where a symlink points: shown, never followed.
    #[serde(default)]
    pub link: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Dir,
    #[default]
    File,
    Symlink,
    Other,
}

/// `POST /api/v1/snapshots/{id}/restore` (202, task `snapshot.restore`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreRequest {
    /// File or folder, relative to the snapshot (and to the live subvolume).
    pub path: String,
    #[serde(default)]
    pub mode: RestoreMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RestoreMode {
    /// Next to the original as `<name> (restored YYYY-MM-DD HHMM)`.
    #[default]
    Copy,
    /// In place; the current version is first renamed to
    /// `<name>.rynas-before-restore-YYYYMMDDTHHMMSS`.
    Replace,
}

/// `POST /api/v1/snapshots/{id}/rollback` (202, task `snapshot.rollback`): the whole
/// subvolume goes back to the snapshot. The current one is kept, renamed
/// `<subvolume>.rynas-rollback-YYYYMMDDTHHMMSS`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackRequest {
    /// Must be the snapshot id (400 `confirmation_required` otherwise).
    #[serde(default)]
    pub confirm: String,
}

/// `GET /api/v1/snapshots/{id}/preview-rollback`: what a rollback to this snapshot does,
/// for the confirmation dialog. No side effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RollbackPreview {
    pub id: String,
    /// A system restore point (root rollback, reboot needed).
    pub system: bool,
    /// The rollback can run from rynas (a share snapshot, or a restore point whose kernel
    /// modules are present on a machine with a boot layer).
    pub allowed: bool,
    /// Why not, when `allowed` is false (English, for the log; the UI shows its own text).
    #[serde(default)]
    pub refusal: Option<RollbackRefusal>,
    /// Root rollback only: the kernel the restore point boots (`uname -r`).
    #[serde(default)]
    pub kernel: Option<String>,
    /// Root rollback only: the kernel running now.
    #[serde(default)]
    pub running_kernel: Option<String>,
    /// Root rollback only: `/lib/modules/<kernel>` is in the restore point.
    #[serde(default)]
    pub modules: bool,
    /// Share rollback: the subvolume that goes back.
    #[serde(default)]
    pub subvolume: Option<String>,
}

/// Why a rollback cannot run from rynas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackRefusal {
    /// Root rollback needs a hardware module's boot layer (it points the boot entry at the
    /// restore point's kernel); elsewhere it is a console procedure.
    NoBootLayer,
    /// The restore point has no kernel image.
    NoKernel,
    /// The restore point lacks `/lib/modules` of its kernel: it cannot boot.
    NoModules,
    /// Not a read-only snapshot.
    NotReadonly,
}
