//! Storage inventory, health, planning and execution (`/api/v1/storage/...`).
//!
//! The read side describes the system as it is, or a plan of commands that *would* change
//! it. The write side executes plans as tasks and stages disk settings.
//!
//! | Method | Path | Body | Response |
//! |---|---|---|---|
//! | GET | `/api/v1/storage/disks` | | `Vec<DiskSummary>` |
//! | GET | `/api/v1/storage/disks/{name}` | | [`DiskDetail`] |
//! | GET | `/api/v1/storage/arrays` | | `Vec<MdArray>` |
//! | GET | `/api/v1/storage/filesystems` | | `Vec<BtrfsFilesystem>` |
//! | GET | `/api/v1/storage/tree` | | [`StorageTree`] |
//! | POST | `/api/v1/storage/plan/volume` | [`VolumePlanRequest`] | [`StoragePlan`] |
//! | POST | `/api/v1/storage/plan/grow` | [`GrowPlanRequest`] | [`StoragePlan`] |
//! | POST | `/api/v1/storage/plan/replace` | [`ReplacePlanRequest`] | [`StoragePlan`] |
//! | POST | `/api/v1/storage/plan/scrub` | [`ScrubPlanRequest`] | [`StoragePlan`] |
//!
//! The write side (volumes, subvolumes, disk settings) is listed above
//! [`VolumeCreateRequest`].

use serde::{Deserialize, Serialize};

// ------------------------------------------------------------------ disks

/// Disk family, which decides the recommended settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DiskKind {
    Hdd,
    Ssd,
    Nvme,
    #[default]
    Unknown,
}

/// Power state read without waking the disk (ATA CHECK POWER MODE, `smartctl -n standby`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PowerState {
    /// Spinning or ready (also "active or idle" when the disk does not tell them apart).
    Active,
    Idle,
    /// Spun down. Reading SMART would wake it, so cached data is shown.
    Standby,
    Sleep,
    #[default]
    Unknown,
}

/// Overall verdict computed by rynas from SMART (not only the drive's own PASSED flag).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum HealthVerdict {
    /// No SMART data (yet): disk asleep since rynasd started, no permission, unsupported.
    #[default]
    Unknown,
    Healthy,
    Warning,
    Failing,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Info,
    Warning,
    Critical,
}

/// One reason behind a [`HealthVerdict`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HealthReason {
    pub severity: Severity,
    /// Stable machine code, e.g. "reallocated_sectors", "nvme_spare_low", "crc_errors".
    pub code: String,
    /// One line for humans, in English; the UI translates by `code` and `args`.
    pub message: String,
    /// The values `message` names, for the translated text.
    #[serde(default, skip_serializing_if = "ReasonArgs::is_empty")]
    pub args: ReasonArgs,
}

/// Values of a [`HealthReason`]; which ones a code uses is documented next to the code in
/// `rynas_storage::smart::verdict`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReasonArgs {
    /// A count, a temperature (°C), years or a percentage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<f64>,
    /// A threshold (`nvme_spare_low`: the spare threshold in %).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// A name as the drive reports it: a SMART attribute with its id, a self-test kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A status as the drive reports it (the result of a self-test).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ReasonArgs {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// `GET /api/v1/storage/disks`: one whole disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DiskSummary {
    /// Kernel name, e.g. "sda", "nvme0n1".
    pub name: String,
    /// Device node, e.g. "/dev/sda".
    pub path: String,
    pub model: String,
    pub serial: String,
    pub wwn: Option<String>,
    pub firmware: Option<String>,
    pub size: u64,
    pub kind: DiskKind,
    pub rotational: bool,
    /// "sata", "nvme", "usb", "sas", ...
    pub transport: String,
    /// Bay number when a hardware provider maps it (filled by `rynas-web`).
    pub bay: Option<u32>,
    /// The M.2 slot of an NVMe disk when a hardware provider maps it (filled by `rynas-web`).
    #[serde(default)]
    pub m2: Option<M2Ref>,
    pub power_state: PowerState,
    pub rotation_rpm: Option<u32>,
    pub temperature_c: Option<f32>,
    pub power_on_hours: Option<u64>,
    pub health: HealthVerdict,
    pub health_reasons: Vec<HealthReason>,
    /// Age of the SMART data behind `health`; None when there is none.
    pub smart_age_s: Option<u64>,
    /// What sits on the disk: "md2", "btrfs root", "crypto_LUKS", "mounted /boot"... Empty
    /// means blank. Leftovers that are not live (old partitions, an unassembled array) do not
    /// make it busy: see `in_use`.
    pub used_by: Vec<String>,
    /// Something on it is live (mounted, assembled array, open LUKS, swap): not a candidate.
    #[serde(default)]
    pub in_use: bool,
    /// Holds the operating system: never a candidate.
    #[serde(default)]
    pub system: bool,
    /// Known or detected shingled (SMR) drive: bad for RAID5/6 rebuilds.
    pub smr: bool,
    pub removable: bool,
}

/// The M.2 slot that holds a disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct M2Ref {
    /// None: an M.2 slot, but which one is unknown.
    pub number: Option<u32>,
}

/// `GET /api/v1/storage/disks/{name}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DiskDetail {
    pub summary: DiskSummary,
    pub smart: Option<SmartInfo>,
    /// Recommended (per disk type) vs current settings, read-only.
    pub settings: Vec<SettingCheck>,
    pub partitions: Vec<Partition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Partition {
    pub name: String,
    pub size: u64,
    pub fstype: Option<String>,
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub mountpoints: Vec<String>,
    /// Devices stacked on it, e.g. ["md2"] or ["luks-1234"].
    pub holders: Vec<String>,
}

/// SMART data as parsed from `smartctl --json=c -a`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SmartInfo {
    /// When it was read (ms since the epoch).
    pub read_ms: u64,
    /// e.g. "7.4".
    pub smartctl_version: String,
    /// "ATA", "NVMe", "SCSI".
    pub protocol: String,
    /// smartctl exit status bit mask (see smartctl(8) "RETURN VALUES").
    pub exit_status: u32,
    /// The drive's own overall assessment, when it gave one.
    pub passed: Option<bool>,
    pub smart_enabled: Option<bool>,
    pub temperature_c: Option<f32>,
    pub power_on_hours: Option<u64>,
    pub power_cycles: Option<u64>,
    /// ATA attribute table (empty for NVMe).
    pub attributes: Vec<SmartAttribute>,
    pub nvme: Option<NvmeHealth>,
    /// Self-test log, newest first.
    pub self_tests: Vec<SelfTest>,
    /// A self-test is running, with its remaining percent.
    pub self_test_running: Option<u32>,
    /// Recommended polling time of the short and extended self-tests, minutes.
    pub short_test_minutes: Option<u32>,
    pub extended_test_minutes: Option<u32>,
    /// Number of entries in the ATA/NVMe error log.
    pub error_log_count: Option<u64>,
    /// Remaining life in percent for SSDs (from the wear attribute or NVMe percentage used).
    pub life_remaining_pct: Option<u32>,
    /// Lifetime host writes, bytes, when the drive reports them.
    pub bytes_written: Option<u64>,
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SmartAttribute {
    pub id: u8,
    pub name: String,
    pub value: u32,
    pub worst: u32,
    pub thresh: u32,
    pub raw: u64,
    /// Raw value as printed by smartctl (may carry extra fields, e.g. "35 (Min/Max 18/45)").
    pub raw_string: String,
    pub prefailure: bool,
    /// "" (never), "past" or "now".
    pub when_failed: String,
    /// One of the attributes rynas watches for the verdict.
    pub watched: bool,
}

/// NVMe SMART / Health Information log page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NvmeHealth {
    pub critical_warning: u8,
    pub temperature_c: Option<f32>,
    pub available_spare_pct: u32,
    pub available_spare_threshold_pct: u32,
    pub percentage_used: u32,
    /// Units of 1000 × 512 bytes, as reported.
    pub data_units_read: u64,
    pub data_units_written: u64,
    /// `data_units_written` in terabytes (10^12 bytes).
    pub tb_written: f64,
    pub tb_read: f64,
    pub power_cycles: u64,
    pub power_on_hours: u64,
    pub unsafe_shutdowns: u64,
    pub media_errors: u64,
    pub error_log_entries: u64,
    pub warning_temp_minutes: u64,
    pub critical_temp_minutes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SelfTest {
    /// "Short offline", "Extended offline", "Short", "Extended", ...
    pub kind: String,
    /// smartctl's status text, e.g. "Completed without error".
    pub status: String,
    pub passed: Option<bool>,
    pub lifetime_hours: Option<u64>,
    pub lba_first_error: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Current value matches the recommendation.
    Ok,
    /// Differs: the UI shows the note and, later, a "restore default" action.
    Differs,
    #[default]
    Unknown,
    NotApplicable,
    /// Past a critical limit (the temperature above its critical threshold).
    Critical,
}

/// A per-disk setting: what the disk type wants vs what is set now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SettingCheck {
    /// "scheduler", "read_ahead_kb", "write_cache", "apm", "trim", "alpm", "apst", "temperature".
    pub key: String,
    /// English label (logs, CLI); the UI translates `key`.
    pub label: String,
    /// English text of the current value; the UI translates `current_value` when present.
    pub current: Option<String>,
    /// English text of the recommendation; the UI translates `recommended_value` when present.
    pub recommended: String,
    pub status: CheckStatus,
    /// Impact in one line, in English; the UI translates `note_code`.
    pub note: String,
    /// `current` as a code the UI translates; absent when `current` is a literal value (a
    /// scheduler name, a number of KiB) shown as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_value: Option<CheckValue>,
    /// `recommended` as a code; absent when it is a literal value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_value: Option<CheckValue>,
    /// Stable code of `note` ("sched_hdd", "apm", "trim_none"…); absent without a note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_code: Option<String>,
}

/// A value of a [`SettingCheck`] the UI translates by `code`; `n` is the number its text
/// shows (a temperature in °C, a latency in µs). A client that does not know `code` shows
/// the English text of the check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CheckValue {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<f64>,
}

impl CheckValue {
    pub fn code(code: &str) -> Self {
        Self { code: code.into(), n: None }
    }

    pub fn with(code: &str, n: f64) -> Self {
        Self { code: code.into(), n: Some(n) }
    }
}

// ------------------------------------------------------------------ md arrays

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MemberRole {
    Active,
    Spare,
    Faulty,
    Journal,
    /// Being rebuilt onto (recovery target).
    Rebuilding,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MdMember {
    /// Component device, e.g. "sda3".
    pub device: String,
    /// Whole disk it belongs to, e.g. "sda".
    pub disk: Option<String>,
    pub role: MemberRole,
    /// Slot in the array (0-based), None for spares and faulty members.
    pub slot: Option<u32>,
    /// sysfs state, e.g. "in_sync", "faulty", "spare", "write_mostly".
    pub state: String,
    /// Corrected read errors counted by md.
    pub errors: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SyncProgress {
    /// "resync", "recovery", "check", "repair", "reshape".
    pub action: String,
    /// None while "DELAYED"/"PENDING".
    pub pct: Option<f32>,
    pub speed_kib_s: Option<u64>,
    pub finish_s: Option<u64>,
    /// "delayed" or "pending" when waiting for another array on the same disks.
    pub waiting: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ArrayHealth {
    Healthy,
    /// Running without full redundancy.
    Degraded,
    /// More members missing than the level tolerates, or not running.
    Failed,
    Inactive,
    #[default]
    Unknown,
}

/// `GET /api/v1/storage/arrays`: one md array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MdArray {
    /// Kernel name, e.g. "md2".
    pub name: String,
    pub path: String,
    /// Array name from the superblock, e.g. "nas:2".
    pub md_name: Option<String>,
    pub uuid: Option<String>,
    /// e.g. "raid5".
    pub level: String,
    /// e.g. "1.2".
    pub metadata: Option<String>,
    pub raid_disks: u32,
    pub size: u64,
    pub chunk_bytes: Option<u64>,
    /// md's own state, e.g. "clean", "active", "clean, degraded, recovering".
    pub state: String,
    pub health: ArrayHealth,
    /// Number of missing members.
    pub degraded: u32,
    pub sync: Option<SyncProgress>,
    /// "internal", "none" or a file path.
    pub bitmap: Option<String>,
    pub events: Option<u64>,
    pub mismatch_cnt: Option<u64>,
    pub members: Vec<MdMember>,
    /// Superblock details were not refreshed because a member disk is in standby.
    pub stale: bool,
}

// ------------------------------------------------------------------ btrfs

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QuotaMode {
    #[default]
    None,
    /// Classic qgroups (full accounting, slow with many snapshots).
    Qgroup,
    /// Simple quotas (kernel 6.7+).
    Squota,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BtrfsDeviceErrors {
    pub write_io: u64,
    pub read_io: u64,
    pub flush_io: u64,
    pub corruption: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BtrfsDevice {
    pub devid: u64,
    /// e.g. "/dev/mapper/rynas-vol1", "/dev/md2".
    pub path: String,
    pub size: u64,
    pub used: u64,
    pub missing: bool,
    pub errors: BtrfsDeviceErrors,
}

/// Block group profile usage, e.g. Data single, Metadata DUP.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BtrfsProfile {
    /// "data", "metadata", "system".
    pub kind: String,
    /// "single", "dup", "raid1", ...
    pub profile: String,
    pub size: u64,
    pub used: u64,
}

/// State of the last btrfs scrub of a filesystem (`btrfs scrub status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScrubState {
    Finished,
    Running,
    /// Stopped by a user (`btrfs scrub cancel`).
    Aborted,
    /// Stopped by an unmount or a reboot.
    Interrupted,
    /// No scrub ever ran.
    #[default]
    Never,
    /// A status word this version does not know.
    #[serde(other)]
    Unknown,
}

impl ScrubState {
    /// From btrfs-progs' `Status:` word.
    pub fn from_btrfs(word: &str) -> Self {
        match word.trim() {
            "finished" => Self::Finished,
            "running" => Self::Running,
            "aborted" => Self::Aborted,
            "interrupted" => Self::Interrupted,
            "never" | "" => Self::Never,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ScrubStatus {
    pub status: ScrubState,
    /// As printed by btrfs-progs, e.g. "Thu Oct 22 15:35:01 2026".
    pub started: Option<String>,
    pub duration_s: Option<u64>,
    pub total_bytes: Option<u64>,
    pub scrubbed_bytes: Option<u64>,
    pub pct: Option<f32>,
    pub rate_bps: Option<u64>,
    pub time_left_s: Option<u64>,
    /// "no errors found" or e.g. "csum=3 read=1".
    pub error_summary: String,
    pub errors: u64,
    pub corrected: Option<u64>,
    pub uncorrectable: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Subvolume {
    pub id: u64,
    /// Path from the top level of the filesystem, e.g. "photos", ".snapshots/photos/photos.20261105T1200".
    pub path: String,
    pub parent_id: u64,
    pub generation: u64,
    pub creation_generation: u64,
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub received_uuid: Option<String>,
    pub readonly: bool,
    pub mountpoint: Option<String>,
}

/// `GET /api/v1/storage/filesystems`: one btrfs filesystem.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BtrfsFilesystem {
    pub uuid: String,
    pub label: Option<String>,
    /// Main mountpoint (top level first, else the shortest path); None when not mounted.
    pub mountpoint: Option<String>,
    pub mountpoints: Vec<String>,
    pub devices: Vec<BtrfsDevice>,
    pub device_size: u64,
    pub used: u64,
    pub free_estimated: u64,
    pub free_min: u64,
    pub data_ratio: f32,
    pub metadata_ratio: f32,
    pub profiles: Vec<BtrfsProfile>,
    /// From the mount options, e.g. "zstd:1"; None when not compressed.
    pub compression: Option<String>,
    pub mount_options: Vec<String>,
    pub quota: QuotaMode,
    pub scrub: Option<ScrubStatus>,
    pub subvolumes: Vec<Subvolume>,
    /// Details come from the cache because an underlying disk is in standby.
    pub stale: bool,
    pub read_ms: u64,
}

// ------------------------------------------------------------------ tree

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Disk,
    Partition,
    Md,
    Crypt,
    Lvm,
    Btrfs,
    Filesystem,
    Subvolume,
    #[default]
    Other,
}

/// One layer of a volume. Children are the layers *below* it (towards the disks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TreeNode {
    pub kind: NodeKind,
    pub name: String,
    pub size: u64,
    /// e.g. "clean", "degraded", "unlocked", "locked", "standby".
    pub state: Option<String>,
    /// Short extra line, e.g. "raid5, 4 disks" or "LUKS2".
    pub detail: Option<String>,
    pub health: HealthVerdict,
    pub children: Vec<TreeNode>,
}

/// A volume: a filesystem with the stack of block devices under it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VolumeTree {
    /// Filesystem UUID.
    pub id: String,
    pub label: Option<String>,
    pub fstype: String,
    pub mountpoint: Option<String>,
    pub size: u64,
    pub used: u64,
    /// Worst health of everything below.
    pub health: HealthVerdict,
    pub encrypted: bool,
    /// Top layer = the device the filesystem lives on; children go down to the disks.
    pub layers: Vec<TreeNode>,
    pub subvolumes: Vec<TreeNode>,
}

/// `GET /api/v1/storage/tree`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StorageTree {
    pub volumes: Vec<VolumeTree>,
    /// Locked LUKS containers (no filesystem visible until unlocked).
    pub locked: Vec<TreeNode>,
    /// Whole disks with nothing on them.
    pub unused_disks: Vec<String>,
}

// ------------------------------------------------------------------ planning

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RaidLevel {
    /// One disk (md RAID1 with one member, so a mirror can be added later, like DSM "Basic").
    #[default]
    Single,
    Raid0,
    Raid1,
    Raid5,
    Raid6,
    Raid10,
}

/// `POST /api/v1/storage/plan/volume`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VolumePlanRequest {
    /// Whole disks by kernel name, e.g. ["sdb", "sdc", "sdd"].
    pub disks: Vec<String>,
    pub level: RaidLevel,
    #[serde(default)]
    pub encrypt: bool,
    #[serde(default = "yes")]
    pub compress: bool,
    /// Volume label and mount name: `[a-z][a-z0-9-]{0,15}`, mounted at `/<label>`.
    pub label: String,
}

fn yes() -> bool {
    true
}

/// `POST /api/v1/storage/plan/grow`: add disks to an array (more mirrors, more capacity, or a
/// level change such as RAID1 → RAID5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GrowPlanRequest {
    /// e.g. "md2".
    pub array: String,
    pub disks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<RaidLevel>,
}

/// `POST /api/v1/storage/plan/replace`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReplacePlanRequest {
    pub array: String,
    /// Member to replace: component ("sdb3") or whole disk ("sdb").
    pub old: String,
    /// New whole disk.
    pub new_disk: String,
}

/// `POST /api/v1/storage/plan/scrub`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ScrubPlanRequest {
    /// Filesystem UUID or mountpoint.
    pub filesystem: String,
    /// Also run an md "check" on the arrays below (verifies RAID parity / mirrors).
    #[serde(default = "yes")]
    pub md_check: bool,
}

/// What a plan step does when executed (by the task engine, later).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlanAction {
    /// Run a program: argv only, never a shell. `stdin` names a secret to feed on standard
    /// input (e.g. "passphrase"); it is never part of the plan.
    Command {
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdin: Option<String>,
    },
    /// Write a value to a sysfs file, e.g. md `sync_action`.
    WriteSysfs { path: String, value: String },
    /// A configuration change owned by rynas-config (fstab, crypttab, mdadm.conf, ...).
    Config { file: String, entry: String },
    /// Wait for a condition before going on, e.g. the end of an md reshape.
    Wait { condition: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    /// English text of the step (logs, task output); the UI translates `kind` + `args`.
    pub description: String,
    /// What the step does, for a translated text; `None` from older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<PlanStepKind>,
    /// Values the translated text names (disk, mount point, level, number of disks).
    #[serde(default, skip_serializing_if = "PlanArgs::is_empty")]
    pub args: PlanArgs,
    #[serde(flatten)]
    pub action: PlanAction,
    /// The step destroys data on these devices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub destroys: Vec<String>,
}

/// Answer of every `POST /api/v1/storage/plan/*`: no side effects, only what would be done.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StoragePlan {
    /// "create_volume", "grow_array", "replace_disk", "scrub".
    pub operation: String,
    /// False when `errors` is not empty: `steps` is then empty.
    pub valid: bool,
    /// English text of each error (logs, tooltips); the UI translates `issues` instead.
    pub errors: Vec<String>,
    /// English text of each warning (logs, CLI); the UI translates `warning_items` instead.
    pub warnings: Vec<String>,
    /// The same warnings as `warnings`, in the same order, with a stable code and the values
    /// their text names.
    #[serde(default)]
    pub warning_items: Vec<PlanWarning>,
    /// The same errors as `errors`, in the same order, with a stable code and the request
    /// field they are about, so a form can mark the right input.
    #[serde(default)]
    pub issues: Vec<PlanIssue>,
    /// Sum of the selected disks.
    pub raw_bytes: u64,
    /// Estimated usable space of the result (before filesystem overhead).
    pub usable_bytes: u64,
    /// Disks that may fail without data loss.
    pub fault_tolerance: u32,
    /// Bytes of the selected disks that the layout cannot use (mixed sizes).
    pub wasted_bytes: u64,
    /// Named results, e.g. {"md": "/dev/md/vol1", "mapper": "rynas-vol1", "mountpoint": "/vol1"}.
    pub summary: Vec<(String, String)>,
    pub steps: Vec<PlanStep>,
}

/// Why a storage plan cannot run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanIssueCode {
    /// The label breaks the naming rule ([`crate::validate::volume_label`]).
    InvalidLabel,
    /// A volume, array or mount already has that name.
    LabelTaken,
    /// The same disk is listed twice.
    DiskSelectedTwice,
    /// Wrong number of disks for the RAID level.
    DiskCount,
    /// No disks were selected.
    NoDisks,
    NoSuchDisk,
    /// The disk holds the operating system.
    SystemDisk,
    /// The disk is part of an array, a mounted filesystem or another volume.
    DiskInUse,
    /// SMART says the disk is failing.
    DiskFailing,
    /// The disk is too small (for any volume, or than the members it joins or replaces).
    DiskTooSmall,
    NoSuchArray,
    /// The array is degraded or failed: repair it first.
    ArrayNotHealthy,
    /// A resync, recovery, check or reshape is running.
    ArrayBusy,
    /// The array's level cannot be grown or changed by rynas.
    LevelUnsupported,
    /// The level change asked for is not supported.
    LevelChangeUnsupported,
    /// The disk to replace is not a member of the array.
    NotAMember,
    /// RAID0/linear: a member cannot be replaced.
    NoRedundancy,
    /// No healthy member left to rebuild from.
    NoHealthyMember,
    NoSuchFilesystem,
    /// The filesystem is not mounted.
    NotMounted,
    /// A scrub is already running.
    ScrubRunning,
}

/// One error of a [`StoragePlan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanIssue {
    pub code: PlanIssueCode,
    /// Request field it is about, as in the request body: `label`, `level`, `disks`,
    /// `array`, `old`, `new_disk`, `filesystem`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// The disk, array or filesystem it names, when there is one (`sdb`, `md2`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// Values a plan warning or step names, so the UI can build the sentence in its language.
/// Every field is optional; each code documents the ones it sets.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlanArgs {
    /// The disk, partition, array or mount point the text is about (`sdb`, `md2`, `/vol1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// A second name: the source disk of a copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
    /// A list: disks, what uses a disk (`md2 (raid5)`), SMART reason codes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<RaidLevel>,
}

impl PlanArgs {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Why a storage plan deserves attention (it can still run).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanWarningCode {
    /// `subject` holds data that will be erased; `items` names what uses it.
    DataErased,
    /// `subject` has SMART warnings; `items` are the reason codes.
    SmartWarnings,
    /// No SMART data for `subject` yet.
    NoSmartData,
    /// `subject` is a USB or removable disk.
    RemovableDisk,
    /// Disks of different sizes: `size_bytes` stay unused.
    MixedSizes,
    /// HDD, SSD and NVMe mixed in one array.
    MixedKinds,
    /// SMR disks (`items`) in a redundant array: very slow rebuilds.
    SmrRaid,
    /// SMR disks (`items`) without redundancy: slow sustained writes.
    SmrSlowWrites,
    /// One disk, no redundancy (a mirror can be added later).
    SingleNoRedundancy,
    /// RAID0: any disk failure loses the volume.
    Raid0NoRedundancy,
    /// RAID5 with large or many HDDs: consider RAID6.
    Raid5Rebuild,
    /// The volume is locked after every boot; keep the recovery key and header backup.
    Encrypted,
    CompressionOff,
    /// SSD under LUKS: TRIM is off.
    SsdTrimOff,
    /// `subject` is larger than the members: the extra space stays unused.
    DiskLargerThanMembers,
    /// No mounted btrfs on the array: grow the filesystem by hand.
    GrowFilesystemByHand,
    /// `subject` is a spare: it can simply be removed.
    SpareMember,
    /// `subject` is an SMR disk: the rebuild may be very slow.
    SmrSlowRebuild,
    /// `subject` has `count` partitions: only the md members are rebuilt.
    OtherPartitions,
    /// Array `subject` is not healthy: a scrub can find but not repair errors.
    ArrayNotHealthy,
    /// A scrub reads every block, takes hours and wakes sleeping disks.
    ScrubCost,
    /// A code this client does not know (newer server): show the English text.
    #[serde(other)]
    Other,
}

/// One warning of a [`StoragePlan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanWarning {
    pub code: PlanWarningCode,
    #[serde(default, skip_serializing_if = "PlanArgs::is_empty")]
    pub args: PlanArgs,
}

/// What a [`PlanStep`] does, for a translated description. `args.subject` names the disk,
/// partition, array or mount point where the step has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepKind {
    WipeSignatures,
    ClearPartitionTable,
    CreatePartition,
    WaitUdev,
    /// `level`, `count` disks.
    CreateArray,
    RecordArray,
    LuksFormat,
    AddRecoveryKey,
    BackupLuksHeader,
    UnlockVolume,
    DeclareCrypttab,
    CreateFilesystem,
    MountAtBoot,
    CreateMountPoint,
    MountVolume,
    CreateSnapshotSubvolume,
    CreateCacheSubvolume,
    DisableCowCache,
    MirrorToRaid5,
    AddSpares,
    Reshape,
    SparesToMirrors,
    WaitReshape,
    WaitResync,
    GrowLuks,
    GrowFilesystem,
    /// Copy the partition table of `other` to `subject`.
    CopyPartitionTable,
    RandomizeGuids,
    AddSpare,
    ReplaceCopy,
    WaitCopy,
    RemoveReplaced,
    MarkFailed,
    RemoveMember,
    AddForRebuild,
    WaitRebuild,
    Scrub,
    CheckArray,
    WaitCheck,
    /// A kind this client does not know (newer server): show the English text.
    #[serde(other)]
    Other,
}

// ------------------------------------------------------------------ execution (write side)
//
// | Method | Path | Body | Response |
// |---|---|---|---|
// | POST | `/api/v1/storage/volumes` | [`VolumeCreateRequest`] | 202 `TaskStarted`, or 200 [`StoragePlan`] with `dry_run` |
// | POST | `/api/v1/storage/volumes/{name}/grow` | [`VolumeGrowRequest`] | 202 / 200 (dry run) |
// | POST | `/api/v1/storage/volumes/{name}/replace` | [`VolumeReplaceRequest`] | 202 / 200 (dry run) |
// | POST | `/api/v1/storage/volumes/{name}/scrub` | [`VolumeScrubRequest`] | 202 / 200 (dry run) |
// | POST | `/api/v1/storage/volumes/{name}/unlock` | [`UnlockRequest`] | 202 |
// | POST | `/api/v1/storage/volumes/{name}/lock` | — | 202 |
// | POST | `/api/v1/storage/volumes/{name}/reveal-recovery-key` | — | [`RecoveryKey`], once |
// | GET/DELETE | `/api/v1/storage/volumes/{name}/luks-header` | — | `application/octet-stream` / 204 |
// | GET/PUT | `/api/v1/storage/volumes/{name}/settings` | [`VolumeSettings`] | [`VolumeSettingsState`] |
// | GET/POST | `/api/v1/storage/volumes/{name}/subvolumes` | [`SubvolumeCreateRequest`] | `Vec<SubvolumeInfo>` / 202 |
// | PUT/DELETE | `/api/v1/storage/volumes/{name}/subvolumes/{sub}?confirm=` | [`SubvolumeSettings`] | 202 |
// | GET/PUT | `/api/v1/storage/disks/{name}/settings` | [`DiskSettings`] | [`DiskSettingsState`] |

/// `POST /api/v1/storage/volumes`: execute the plan of [`VolumePlanRequest`].
///
/// Destructive: `confirm` must be the volume label and `serials` the serial numbers of
/// exactly the selected disks (a disk without a serial, such as a virtual disk, is named by
/// its kernel name). `passphrase` is required with `encrypt` and travels on standard input
/// only, never in a log.
#[derive(Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VolumeCreateRequest {
    #[serde(flatten)]
    pub plan: VolumePlanRequest,
    #[serde(default)]
    pub confirm: String,
    #[serde(default)]
    pub serials: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// Validate everything (plan, confirmation, disks free) and answer the plan without
    /// running it.
    #[serde(default)]
    pub dry_run: bool,
}

/// `POST /api/v1/storage/volumes/{name}/grow`: add disks to the array of a volume.
#[derive(Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VolumeGrowRequest {
    pub disks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<RaidLevel>,
    /// The volume label.
    #[serde(default)]
    pub confirm: String,
    /// Serials of the new disks.
    #[serde(default)]
    pub serials: Vec<String>,
    /// Needed when the volume is encrypted and the LUKS mapping is grown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

/// `POST /api/v1/storage/volumes/{name}/replace`: replace a member (failed or not).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VolumeReplaceRequest {
    /// Member to replace: component ("sdb1") or whole disk ("sdb").
    pub old: String,
    pub new_disk: String,
    #[serde(default)]
    pub confirm: String,
    /// Serial of the new disk (one entry).
    #[serde(default)]
    pub serials: Vec<String>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScrubAction {
    #[default]
    Start,
    Stop,
}

/// `POST /api/v1/storage/volumes/{name}/scrub`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VolumeScrubRequest {
    #[serde(default)]
    pub action: ScrubAction,
    /// Also run an md "check" on the arrays below.
    #[serde(default = "yes")]
    pub md_check: bool,
    #[serde(default)]
    pub dry_run: bool,
}

/// `POST /api/v1/storage/volumes/{name}/unlock`: the passphrase or the recovery key.
#[derive(Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UnlockRequest {
    pub passphrase: String,
}

/// Answer of `POST /api/v1/storage/volumes/{name}/reveal-recovery-key`: the recovery key of a
/// volume created encrypted, kept in memory for one hour and given out once.
#[derive(Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RecoveryKey {
    pub key: String,
}

/// `Debug` without the secret fields (passphrases, recovery keys never reach a log).
macro_rules! redacted_debug {
    ($t:ident $(, $f:ident)*) => {
        impl std::fmt::Debug for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let mut d = f.debug_struct(stringify!($t));
                $( d.field(stringify!($f), &self.$f); )*
                d.field("secret", &"<redacted>").finish()
            }
        }
    };
}
redacted_debug!(VolumeCreateRequest, plan, confirm, serials, dry_run);
redacted_debug!(VolumeGrowRequest, disks, level, confirm, serials, dry_run);
redacted_debug!(UnlockRequest);
redacted_debug!(RecoveryKey);

/// A rynas volume as recorded in the configuration (key `storage.volumes`): the source of
/// the `/etc/fstab`, `/etc/crypttab` and `/etc/mdadm/mdadm.conf` entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VolumeRecord {
    pub label: String,
    /// md array UUID as mdadm prints it (`xxxxxxxx:xxxxxxxx:xxxxxxxx:xxxxxxxx`).
    pub md_uuid: String,
    pub level: RaidLevel,
    /// LUKS2 container UUID when encrypted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub luks_uuid: Option<String>,
    /// btrfs filesystem UUID.
    pub fs_uuid: String,
    /// `/<label>`.
    pub mountpoint: String,
    /// Mount options without `nofail`/`noauto` (added by rynas), e.g. `noatime,compress=zstd:1`.
    pub options: String,
    #[serde(default)]
    pub created_ms: u64,
}

/// Configuration key `storage.volumes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StorageVolumes {
    #[serde(default)]
    pub volumes: Vec<VolumeRecord>,
}

/// I/O scheduler choices of a disk queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IoScheduler {
    MqDeadline,
    Bfq,
    Kyber,
    None,
}

impl IoScheduler {
    pub fn as_str(self) -> &'static str {
        match self {
            IoScheduler::MqDeadline => "mq-deadline",
            IoScheduler::Bfq => "bfq",
            IoScheduler::Kyber => "kyber",
            IoScheduler::None => "none",
        }
    }
}

/// Per-disk settings (D-SYSTUNE). Every field is optional: `None` keeps the default of the
/// disk type (scheduler, read-ahead) or leaves the drive alone (APM, standby).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiskSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler: Option<IoScheduler>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_ahead_kb: Option<u32>,
    /// `hdparm -B`: 1-254 (up to 127 allows spin-down), 255 turns APM off. HDDs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apm: Option<u8>,
    /// Spin down after this many idle seconds (`hdparm -S`, rounded up to what the drive can
    /// express); 0 disables the timer. HDDs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standby_s: Option<u32>,
}

impl DiskSettings {
    pub fn is_empty(&self) -> bool {
        *self == DiskSettings::default()
    }
}

/// One per-disk override, matched by serial (kernel names change between boots).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiskOverride {
    pub serial: String,
    #[serde(flatten)]
    pub settings: DiskSettings,
}

/// md tuning chosen for an array (RAID5/6 only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ArrayTuning {
    /// As in [`VolumeRecord::md_uuid`].
    pub md_uuid: String,
    pub stripe_cache_size: u32,
}

/// Configuration key `storage.disks`: rendered to `/etc/udev/rules.d/69-rynas-storage.rules`
/// together with the per-type defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StorageTuning {
    #[serde(default)]
    pub disks: Vec<DiskOverride>,
    #[serde(default)]
    pub arrays: Vec<ArrayTuning>,
}

/// `GET/PUT /api/v1/storage/disks/{name}/settings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DiskSettingsState {
    pub disk: String,
    pub serial: String,
    pub kind: DiskKind,
    /// What the per-type rule sets.
    pub defaults: DiskSettings,
    /// The applied override of this disk.
    pub applied: Option<DiskSettings>,
    /// The override the session user has staged (`Some` of an empty value removes it).
    pub pending: Option<DiskSettings>,
    /// Defaults with the applied override on top.
    pub effective: DiskSettings,
    pub current_scheduler: Option<String>,
    pub current_read_ahead_kb: Option<u32>,
}

/// `PUT /api/v1/storage/volumes/{name}/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VolumeSettings {
    /// md `stripe_cache_size` (entries of one page per member); `None` keeps the kernel
    /// default (256). RAID5/6 only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stripe_cache_size: Option<u32>,
}

/// `GET /api/v1/storage/volumes/{name}/settings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VolumeSettingsState {
    pub volume: String,
    pub applied: VolumeSettings,
    pub pending: Option<VolumeSettings>,
    /// rynas' proposal for this array and this amount of RAM; applied only when chosen.
    pub proposed_stripe_cache_size: Option<u32>,
    /// Memory the proposal costs (entries × 4 KiB × members).
    pub proposed_memory_bytes: Option<u64>,
    /// What the kernel uses now.
    pub current_stripe_cache_size: Option<u32>,
}

/// An entry of `GET /api/v1/storage/volumes/{name}/subvolumes`: the subvolumes directly
/// below the top level of the volume.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SubvolumeInfo {
    pub name: String,
    /// Absolute path, e.g. "/vol1/photos".
    pub path: String,
    pub id: u64,
    pub readonly: bool,
    /// The `compression` property: None inherits the mount option, "none" disables it.
    pub compression: Option<String>,
    /// Created and used by rynas (`.snapshots`, `@rynas-cache`): not changed here.
    pub system: bool,
}

/// `POST /api/v1/storage/volumes/{name}/subvolumes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SubvolumeCreateRequest {
    /// `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`.
    pub name: String,
    #[serde(flatten)]
    pub settings: SubvolumeSettings,
}

/// `PUT /api/v1/storage/volumes/{name}/subvolumes/{sub}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubvolumeSettings {
    /// false sets the `compression` property to "none" (video, photos, backups, VM images).
    #[serde(default = "yes")]
    pub compression: bool,
    #[serde(default)]
    pub readonly: bool,
}

impl Default for SubvolumeSettings {
    fn default() -> Self {
        Self {
            compression: true,
            readonly: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_warnings_and_step_kinds_round_trip() {
        let p = StoragePlan {
            operation: "create_volume".into(),
            valid: true,
            warnings: vec!["sdb: no SMART data yet".into()],
            warning_items: vec![PlanWarning {
                code: PlanWarningCode::NoSmartData,
                args: PlanArgs {
                    subject: Some("sdb".into()),
                    ..Default::default()
                },
            }],
            steps: vec![PlanStep {
                description: "Create the md array (raid1, 2 disks)".into(),
                kind: Some(PlanStepKind::CreateArray),
                args: PlanArgs {
                    level: Some(RaidLevel::Raid1),
                    count: Some(2),
                    ..Default::default()
                },
                action: PlanAction::Command {
                    argv: vec!["mdadm".into()],
                    stdin: None,
                },
                destroys: vec![],
            }],
            ..Default::default()
        };
        let j = serde_json::to_string(&p).unwrap();
        assert!(
            j.contains(r#""warning_items":[{"code":"no_smart_data","args":{"subject":"sdb"}}]"#),
            "{j}"
        );
        assert!(
            j.contains(r#""kind":"create_array","args":{"count":2,"level":"raid1"}"#),
            "{j}"
        );
        assert_eq!(serde_json::from_str::<StoragePlan>(&j).unwrap(), p);
    }

    #[test]
    fn older_and_newer_servers_still_parse() {
        // an older server: no codes at all
        let old = r#"{"operation":"scrub","valid":true,"errors":[],"warnings":["w"],"raw_bytes":0,
            "usable_bytes":0,"fault_tolerance":0,"wasted_bytes":0,"summary":[],
            "steps":[{"description":"d","type":"wait","condition":"c"}]}"#;
        let p: StoragePlan = serde_json::from_str(old).unwrap();
        assert!(p.warning_items.is_empty());
        assert_eq!(p.steps[0].kind, None);
        // a newer server: codes this build does not know
        let w: PlanWarning =
            serde_json::from_str(r#"{"code":"from_the_future","args":{"count":3}}"#).unwrap();
        assert_eq!(w.code, PlanWarningCode::Other);
        let k: PlanStepKind = serde_json::from_str(r#""teleport""#).unwrap();
        assert_eq!(k, PlanStepKind::Other);
    }
}
