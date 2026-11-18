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
//! | GET | `/api/v1/storage/places` | | `Vec<VolumePlace>` (rynas volumes + the system volume) |
//! | POST | `/api/v1/storage/plan/volume` | [`VolumePlanRequest`] | [`StoragePlan`] |
//! | POST | `/api/v1/storage/plan/grow` | [`GrowPlanRequest`] | [`StoragePlan`] |
//! | POST | `/api/v1/storage/plan/replace` | [`ReplacePlanRequest`] | [`StoragePlan`] |
//! | POST | `/api/v1/storage/plan/complete` | [`CompletePlanRequest`] | [`StoragePlan`] |
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
    /// A counter that never goes back to zero: an administrator may mark it as seen
    /// (`POST /api/v1/storage/disks/{name}/health-ack`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub acknowledgeable: bool,
    /// Marked as seen at exactly the current count: still listed, but it no longer weighs in
    /// the disk verdict. Gone as soon as the count changes (grows, or drops after a reset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged: Option<ReasonAck>,
}

/// An acknowledgement of a [`HealthReason`]: the count that was seen, when and by whom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReasonAck {
    pub n: u64,
    pub at_ms: u64,
    #[serde(default)]
    pub by: String,
}

/// `POST /api/v1/storage/disks/{name}/health-ack`: the counts the administrator saw. rynasd
/// reads SMART again and records, per code, the lower of this and the current count, so a
/// count that grew meanwhile stays visible; a value can never be raised above the current one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HealthAckRequest {
    pub codes: Vec<HealthAckCode>,
}

/// Answer of `POST /api/v1/storage/disks/{name}/health-ack`: the reason codes recorded, with
/// the counts, and those whose count had grown past what was seen (still warning).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HealthAckResult {
    pub disk: String,
    pub serial: String,
    pub acknowledged: Vec<HealthAckCode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grew: Vec<String>,
}

/// One code of a [`HealthAckResult`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HealthAckCode {
    pub code: String,
    pub n: u64,
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
    /// The device has no SMART (flash modules, USB bridges that do not pass it through):
    /// `health` stays `unknown` but there is nothing to wait for.
    #[serde(default)]
    pub smart_unsupported: bool,
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
        Self {
            code: code.into(),
            n: None,
        }
    }

    pub fn with(code: &str, n: f64) -> Self {
        Self {
            code: code.into(),
            n: Some(n),
        }
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
    /// Where md's data starts on the member (sysfs `offset`), bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_offset: Option<u64>,
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
    /// Bytes md uses of each member (sysfs `component_size`): a new member needs its data
    /// offset plus this much.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_size: Option<u64>,
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
    /// Sector md `reshape_position` names while a reshape is under way or was interrupted
    /// (sysfs `none` otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reshape_position: Option<u64>,
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
    /// A filesystem of several devices that lost one, or that kept chunks of a weaker profile
    /// from a degraded mount: what is missing and what can be done ([`BtrfsRepair`]). Also
    /// set for a configured volume that cannot be mounted because devices are missing (then
    /// `mountpoint` is `None` and `devices` lists the ones present).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<BtrfsRepair>,
}

// ------------------------------------------------------------------ btrfs with missing devices
//
// | Method | Path | Body | Response |
// |---|---|---|---|
// | POST | `/api/v1/storage/btrfs/{uuid}/replace` | [`BtrfsReplaceRequest`] | 202 `TaskStarted`, or 200 [`StoragePlan`] with `dry_run` |
// | POST | `/api/v1/storage/btrfs/{uuid}/remove-missing` | [`BtrfsRepairRequest`] | 202 / 200 (dry run) |
// | POST | `/api/v1/storage/btrfs/{uuid}/convert` | [`BtrfsRepairRequest`] | 202 / 200 (dry run) |
// | POST | `/api/v1/storage/btrfs/{uuid}/mount-degraded` | [`BtrfsRepairRequest`] | 202 / 200 (dry run) |
// | GET/PUT | `/api/v1/storage/btrfs/{uuid}/settings` | [`BtrfsSettings`] | [`BtrfsSettingsState`] |
//
// `uuid` is the filesystem UUID; `confirm` is the filesystem label, or its UUID when it has
// none (what the storage pages name the volume). See `docs/phase2/btrfs-degraded.md`.

/// Why a repair, or a degraded mount, of a btrfs filesystem cannot run now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BtrfsBlocker {
    /// Some chunks ([`BtrfsRepair::lost`]) have no complete copy on the devices present: the
    /// data would be incomplete.
    Incomplete,
    /// The devices left are fewer than a profile in use needs (raid1 two, raid1c3 three...).
    TooFewDevices,
    /// The devices left lack the unallocated space for the missing device's chunks.
    NoSpace,
    /// The devices present are the ones that were missing at the last degraded mount: they
    /// hold older data than the others (a split brain if both sides were written).
    SplitBrain,
    /// The degraded mount after boot is turned off for this filesystem.
    Disabled,
    /// No device of the filesystem is present.
    NoDevices,
    /// A disk of the filesystem sleeps: its superblock was not read.
    Asleep,
    /// A reason this client does not know (newer server).
    #[serde(other)]
    Other,
}

/// Chunks of one kind to bring back to the volume's profile with a soft balance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BtrfsConvert {
    /// "data", "metadata" or "system".
    pub kind: String,
    /// The weaker profiles found besides `to`, e.g. `["single"]`.
    pub from: Vec<String>,
    /// The profile of the kind (the most redundant one in use), e.g. "raid1".
    pub to: String,
}

/// A device of a btrfs filesystem as its superblock names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BtrfsMember {
    pub devid: u64,
    pub path: String,
    /// Transaction generation of its superblock.
    pub generation: u64,
}

/// What a multi-device btrfs filesystem lacks and what can be done ([`BtrfsFilesystem::repair`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BtrfsRepair {
    pub mounted: bool,
    /// Mounted with `-o degraded`.
    pub degraded_mount: bool,
    /// The root filesystem: the system starts with it only with `rootflags=degraded`.
    pub system: bool,
    /// Devices the filesystem has (superblock `num_devices`); 0 when unknown.
    pub total_devices: u32,
    /// Device ids known missing (a mounted filesystem names them).
    pub missing: Vec<u64>,
    /// Devices missing (`missing` may be empty for a filesystem that is not mounted).
    pub missing_count: u32,
    /// Size of the largest missing device: the smallest replacement.
    pub missing_bytes: u64,
    /// Chunk kinds and profiles without a complete copy, e.g. `"data single"`.
    pub lost: Vec<String>,
    /// Chunks to convert back to the profile of their kind.
    pub convert: Vec<BtrfsConvert>,
    /// Why the missing devices cannot be removed (the others keeping the profile); `None`
    /// when they can.
    pub remove: Option<BtrfsBlocker>,
    /// Not mounted: why it cannot be mounted degraded; `None` when it can.
    pub mount: Option<BtrfsBlocker>,
    /// Not mounted: the devices present, from their superblocks.
    pub present: Vec<BtrfsMember>,
    /// Every device is back after a degraded mount, but no scrub has finished without errors
    /// since: the devices that were missing lack the copies of what was written meanwhile,
    /// so losing the others now costs data. A data scrub brings them up to date.
    #[serde(default)]
    pub scrub_needed: bool,
}

impl BtrfsRepair {
    /// Devices are missing (not only weaker chunks left over).
    pub fn degraded(&self) -> bool {
        self.missing_count > 0
    }
}

/// `POST /api/v1/storage/btrfs/{uuid}/replace`: rebuild missing device `devid` on a new
/// disk (`btrfs replace start <devid> /dev/<new_disk> <mnt>`, then the size grown to the
/// disk and weaker chunks converted back). Destructive for the new disk: `confirm` and its
/// serial in `serials`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BtrfsReplaceRequest {
    pub devid: u64,
    pub new_disk: String,
    #[serde(default)]
    pub confirm: String,
    #[serde(default)]
    pub serials: Vec<String>,
    #[serde(default)]
    pub dry_run: bool,
}

/// `POST /api/v1/storage/btrfs/{uuid}/{remove-missing,convert,mount-degraded}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BtrfsRepairRequest {
    #[serde(default)]
    pub confirm: String,
    #[serde(default)]
    pub dry_run: bool,
}

/// `PUT /api/v1/storage/btrfs/{uuid}/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtrfsSettings {
    /// Mount the filesystem degraded after boot when a device is missing and every chunk
    /// still has a copy (default on).
    pub auto_degraded: bool,
}

impl Default for BtrfsSettings {
    fn default() -> Self {
        Self { auto_degraded: true }
    }
}

/// `GET /api/v1/storage/btrfs/{uuid}/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BtrfsSettingsState {
    pub applied: BtrfsSettings,
    /// What the session user has staged.
    pub pending: Option<BtrfsSettings>,
}

impl BtrfsFilesystem {
    /// Copies of every data byte: `data_ratio` when btrfs gave it, else from the data profile
    /// (raid1/dup/raid10: 2, raid1c3: 3, raid1c4: 4), else 1 (nothing known).
    pub fn data_copies(&self) -> f64 {
        if self.data_ratio.is_finite() && self.data_ratio >= 1.0 {
            return f64::from(self.data_ratio);
        }
        let data = self.profiles.iter().find(|p| p.kind == "data");
        match data.map(|p| p.profile.to_ascii_lowercase()).as_deref() {
            Some("raid1" | "dup" | "raid10") => 2.0,
            Some("raid1c3") => 3.0,
            Some("raid1c4") => 4.0,
            _ => 1.0,
        }
    }

    /// What the files take, counted once: `device_size` and `used` are raw, so a btrfs RAID1
    /// of two 4 TB disks has 8 TB of devices and every byte written twice. With the usage
    /// figures (`profiles`) their logical sum, else the raw use over the copies.
    pub fn logical_used(&self) -> u64 {
        if self.profiles.is_empty() {
            return (self.used as f64 / self.data_copies()) as u64;
        }
        self.profiles.iter().map(|p| p.used).sum()
    }

    /// The space files can use, counted once. With the usage figures (`profiles`): what they
    /// take plus the estimated free space, which already accounts for the profiles (a full
    /// filesystem has `free_estimated` 0 and stays full, never falls back to the devices).
    /// Without them: the devices over the data copies; when nothing tells the copies either,
    /// the raw `device_size` (an over-estimate for a mirror whose profile is unknown).
    pub fn usable_size(&self) -> u64 {
        if !self.profiles.is_empty() {
            return self.logical_used() + self.free_estimated;
        }
        (self.device_size as f64 / self.data_copies()) as u64
    }
}

#[cfg(test)]
mod btrfs_size_tests {
    use super::*;

    /// btrfs RAID1 of two 4 TB disks, as on the QNAP HS-251 after installation.
    #[test]
    fn raid1_counts_once() {
        let p = |kind: &str, used: u64| BtrfsProfile { kind: kind.into(), profile: "raid1".into(), size: 0, used };
        let f = BtrfsFilesystem {
            device_size: 7_995_448_623_104,
            used: 4_970_000_000,
            free_estimated: 3_993_103_138_816,
            data_ratio: 2.0,
            metadata_ratio: 2.0,
            profiles: vec![p("data", 2_426_000_000), p("metadata", 48_350_000), p("system", 16_384)],
            ..Default::default()
        };
        assert_eq!(f.logical_used(), 2_474_366_384);
        assert_eq!(f.usable_size(), 2_474_366_384 + 3_993_103_138_816);
        let bare = BtrfsFilesystem { device_size: 8_000, data_ratio: 2.0, ..Default::default() };
        assert_eq!(bare.usable_size(), 4_000, "no usage figures: devices over the copies");
    }

    /// A really full filesystem: no free space left, it reads 100 %, not the raw devices.
    #[test]
    fn full_filesystem_stays_full() {
        let f = BtrfsFilesystem {
            device_size: 8_000,
            used: 8_000,
            free_estimated: 0,
            data_ratio: 2.0,
            profiles: vec![BtrfsProfile { kind: "data".into(), profile: "raid1".into(), size: 4_000, used: 4_000 }],
            ..Default::default()
        };
        assert_eq!(f.logical_used(), 4_000);
        assert_eq!(f.usable_size(), 4_000);
    }

    /// No usage figures: the ratio if known, else the raw sizes (documented over-estimate).
    #[test]
    fn missing_usage_figures() {
        let unknown = BtrfsFilesystem { device_size: 8_000, used: 2_000, ..Default::default() };
        assert_eq!(unknown.usable_size(), 8_000, "nothing known: raw devices");
        assert_eq!(unknown.logical_used(), 2_000);
        let mirrored = BtrfsFilesystem { device_size: 8_000, used: 2_000, data_ratio: 2.0, ..Default::default() };
        assert_eq!(mirrored.usable_size(), 4_000);
        assert_eq!(mirrored.logical_used(), 1_000);
        let c3 = BtrfsFilesystem {
            profiles: vec![BtrfsProfile { kind: "data".into(), profile: "raid1c3".into(), size: 0, used: 0 }],
            ..Default::default()
        };
        assert_eq!(c3.data_copies(), 3.0, "ratio unknown, profile known");
    }
}

// ------------------------------------------------------------------ flexible RAID

/// One tier of a flexible RAID: the same slice of every member disk (partition number
/// `index`, identical sectors on each disk), assembled as one md array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FlexTier {
    /// 1-based, lowest slice first.
    pub index: u32,
    /// Byte offset of the slice on every member disk (inclusive).
    pub start: u64,
    /// Byte offset where the slice ends (exclusive).
    pub end: u64,
    /// md level of the tier: [`RaidLevel::Raid1`], [`RaidLevel::Raid5`] or
    /// [`RaidLevel::Raid6`]. `None` in a plan: the slice is too narrow (too few disks reach
    /// it) and is left unused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<RaidLevel>,
    /// Whole disks that hold this slice (kernel names, sorted).
    pub members: Vec<String>,
    /// Kernel name of the md array ("md127"); `None` in a plan before it exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub array: Option<String>,
    /// Usable bytes the tier adds to the volume.
    pub usable: u64,
    #[serde(default)]
    pub health: ArrayHealth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncProgress>,
    /// Members md misses (the `degraded` count of the array).
    #[serde(default)]
    pub degraded: u32,
    /// Disks of [`FlexTier::members`] that are not an active member of the array now
    /// (faulty, removed, rebuilding). A disk that vanished has no name left: it only counts
    /// in [`FlexTier::degraded`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
}

/// Unfinished work on a flexible RAID, found from the system state (an interrupted growth),
/// so that "complete" can plan only what is left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlexPendingKind {
    /// A reshape stopped halfway (md `reshape_position` set, no reshape running).
    ReshapeInterrupted,
    /// The tier array is larger than its LVM physical volume (`pvresize` missing).
    ResizePv,
    /// The volume group has free space the logical volume does not use (`lvextend`).
    ExtendLv,
    /// The LUKS mapping is smaller than the logical volume (`cryptsetup resize`).
    ResizeLuks,
    /// The filesystem is smaller than its device (`btrfs filesystem resize max`).
    ResizeFilesystem,
    /// A tier array of the volume is not in the volume group.
    TierNotInVg,
    /// A tier array is not in the recorded volume (not assembled by name at boot).
    TierNotRecorded,
    /// A disk has the tier's partition, but the partition is not a member of the array.
    PartitionNotMember,
    /// A growth or replace recorded as under way did not end (`subject`: its new disks):
    /// completing it, or requesting it again, goes on with it.
    ChangeInterrupted,
    /// The tier array has no write-intent bitmap (a growth stopped after turning it off).
    BitmapOff,
    /// A kind this client does not know (newer server).
    #[serde(other)]
    Other,
}

/// One item of [`FlexLayout::pending`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlexPending {
    pub kind: FlexPendingKind,
    /// The array, partition or device it is about (`md127`, `sdb2`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// Why rynas cannot change a flexible RAID, for a translated text ([`FlexLayout::blocker_items`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlexBlockerKind {
    /// The partitions of tier array `subject` differ in offset or size between the disks.
    UnevenSlices,
    /// Tier arrays `subject` and `other` overlap on the disks.
    TiersOverlap,
    /// The disks of tier array `subject` are not all in the tier below, `other`.
    NotInLowerTier,
    /// The partitions on disk `subject` are not in tier order.
    PartitionOrder,
    /// A tier has no redundancy.
    NoRedundancy,
    /// The volume group `subject` is missing a physical volume.
    VgMissingPv,
    /// A kind this client does not know (newer server): show the English text.
    #[serde(other)]
    Other,
}

/// One item of [`FlexLayout::blocker_items`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlexBlocker {
    pub kind: FlexBlockerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
}

/// A flexible RAID as found on the system: md tiers → LVM volume group → one logical volume.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FlexLayout {
    /// Disks that may fail without data loss: the lowest tolerance of the tiers.
    pub tolerance: u32,
    /// Volume group name (`rynas_<label>` when rynas made it).
    pub vg: String,
    /// Logical volume name (`data` when rynas made it).
    pub lv: String,
    /// Kernel name of the logical volume ("dm-1").
    #[serde(default)]
    pub lv_device: String,
    /// Size of the logical volume.
    #[serde(default)]
    pub size: u64,
    /// Lowest slice first.
    pub tiers: Vec<FlexTier>,
    /// Space on each disk no tier uses: (disk, bytes). Only amounts of at least one tier
    /// step are listed (smaller rests come from rounding near-equal disks together).
    #[serde(default)]
    pub unused: Vec<(String, u64)>,
    /// Names follow the rynas pattern and the filesystem is a btrfs with the volume label:
    /// rynas may change it. A layout made elsewhere is shown read-only.
    pub managed: bool,
    #[serde(default)]
    pub pending: Vec<FlexPending>,
    /// Why rynas cannot change this layout (English): a tier rule broken, a partial volume
    /// group...
    #[serde(default)]
    pub blockers: Vec<String>,
    /// [`FlexLayout::blockers`] by code, in the same order (an older server sends none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocker_items: Vec<FlexBlocker>,
    /// The LVM facts were not refreshed because a disk under them is in standby (or LVM
    /// could not be read): the checks that need them are skipped.
    #[serde(default)]
    pub stale: bool,
}

impl FlexLayout {
    /// The flexible level of the layout (`Flex1`/`Flex2`), `None` without tolerance.
    pub fn level(&self) -> Option<RaidLevel> {
        RaidLevel::flexible(self.tolerance.min(2))
    }

    /// Worst health of the tiers.
    pub fn health(&self) -> ArrayHealth {
        fn rank(h: ArrayHealth) -> u8 {
            match h {
                ArrayHealth::Healthy => 0,
                ArrayHealth::Unknown => 1,
                ArrayHealth::Degraded => 2,
                ArrayHealth::Inactive => 3,
                ArrayHealth::Failed => 4,
            }
        }
        self.tiers
            .iter()
            .map(|t| t.health)
            .max_by_key(|h| rank(*h))
            .unwrap_or_default()
    }
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
    /// The volume is a flexible RAID: its tiers (the LVM node of `layers` has them as
    /// children too).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flexible: Option<FlexLayout>,
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

// ------------------------------------------------------------------ places (D-ROOTVOL)

/// Label of the system volume: the root filesystem offered as a volume wherever a volume can
/// be chosen (D-ROOTVOL). A reserved label ([`crate::validate::RESERVED_LABELS`]), so no
/// rynas volume can take it.
pub const SYSTEM_VOLUME: &str = "system";
/// The folder of the system volume: what rynas puts on it (shares, Docker, apps, backups)
/// lives here. On a btrfs root it is the mount of the top-level subvolume `@rynas-system`
/// (a sibling of the root subvolume), so system restore points and rollbacks neither copy
/// nor replace the data in it.
pub const SYSTEM_VOLUME_DIR: &str = "/srv/rynas/system";
/// A place with less free space than this, or than [`LOW_SPACE_PCT`] of its size, is
/// [`VolumePlace::low_space`].
pub const LOW_SPACE_BYTES: u64 = 10 << 30;
/// See [`LOW_SPACE_BYTES`].
pub const LOW_SPACE_PCT: u64 = 10;

/// The folder of a volume label: `/<label>` for a rynas volume (its mount point),
/// [`SYSTEM_VOLUME_DIR`] for the system volume.
pub fn volume_dir(label: &str) -> String {
    if label == SYSTEM_VOLUME {
        SYSTEM_VOLUME_DIR.to_string()
    } else {
        format!("/{label}")
    }
}

/// The volume label of a path inside a volume folder, and the rest (no leading `/`, may be
/// empty): `/vol1/a/b` → `("vol1", "a/b")`, `/srv/rynas/system/a` → `("system", "a")`. The
/// label is only well formed ([`crate::validate::place_label`]), not checked as mounted.
pub fn split_volume_path(p: &str) -> Option<(String, &str)> {
    if let Some(rest) = p.strip_prefix(SYSTEM_VOLUME_DIR)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return Some((SYSTEM_VOLUME.into(), rest.trim_start_matches('/')));
    }
    let rest = p.strip_prefix('/')?;
    let (label, tail) = rest.split_once('/').unwrap_or((rest, ""));
    crate::validate::volume_label(label).then(|| (label.to_string(), tail))
}

/// Little free space left: below [`LOW_SPACE_BYTES`] or [`LOW_SPACE_PCT`] % of the size.
pub fn low_space(size: u64, avail: u64) -> bool {
    size > 0 && (avail < LOW_SPACE_BYTES || avail.saturating_mul(100) < size.saturating_mul(LOW_SPACE_PCT))
}

/// A volume something can be put on (`GET /api/v1/storage/places`, and the volume lists of
/// the Docker settings): every mounted rynas volume, then the system volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VolumePlace {
    /// Volume label ([`SYSTEM_VOLUME`] for the root filesystem).
    pub label: String,
    /// Its folder ([`volume_dir`]).
    pub path: String,
    pub avail_bytes: Option<u64>,
    /// The root filesystem, the disk of the operating system.
    #[serde(default)]
    pub system: bool,
    /// `btrfs`, `ext4`, `xfs`...: subvolumes, snapshots, replication and NOCOW need btrfs.
    #[serde(default)]
    pub fs_type: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// See [`low_space`].
    #[serde(default)]
    pub low_space: bool,
    /// Why nothing can be put here now (the system volume's folder is not what rynas
    /// expects: a plain folder with files, a symbolic link, a foreign mount...). Shown,
    /// not offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

impl VolumePlace {
    pub fn btrfs(&self) -> bool {
        self.fs_type == "btrfs"
    }
}

// ------------------------------------------------------------------ planning

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RaidLevel {
    /// One disk (md RAID1 with one member, so a mirror can be added later).
    #[default]
    Single,
    Raid0,
    Raid1,
    Raid5,
    Raid6,
    Raid10,
    /// Flexible RAID, 1-disk tolerance: disks of different sizes, each size step ("tier") its
    /// own md array (RAID5, or RAID1 with two members), all tiers joined by LVM.
    Flex1,
    /// Flexible RAID, 2-disk tolerance: tiers are RAID6, or three-way RAID1 with three
    /// members. Needs at least 4 disks.
    Flex2,
}

impl RaidLevel {
    /// A flexible RAID (tiers of md arrays joined by LVM), not one md array.
    pub fn is_flexible(self) -> bool {
        matches!(self, RaidLevel::Flex1 | RaidLevel::Flex2)
    }

    /// Disks that may fail without data loss, at the smallest disk count the level allows
    /// (a RAID1 of more than two disks tolerates more: see the plan's `fault_tolerance`).
    pub fn tolerance(self) -> u32 {
        match self {
            RaidLevel::Single | RaidLevel::Raid0 => 0,
            RaidLevel::Raid1 | RaidLevel::Raid5 | RaidLevel::Raid10 | RaidLevel::Flex1 => 1,
            RaidLevel::Raid6 | RaidLevel::Flex2 => 2,
        }
    }

    /// The flexible level of a tolerance (1 or 2), `None` for anything else.
    pub fn flexible(tolerance: u32) -> Option<RaidLevel> {
        match tolerance {
            1 => Some(RaidLevel::Flex1),
            2 => Some(RaidLevel::Flex2),
            _ => None,
        }
    }
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
    /// e.g. "md2"; empty for a flexible volume (named by `volume`).
    #[serde(default)]
    pub array: String,
    pub disks: Vec<String>,
    /// Classic arrays only: a flexible volume keeps its tolerance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<RaidLevel>,
    /// A flexible volume's label: the disks join its tiers (and may make a new top tier).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
}

/// `POST /api/v1/storage/plan/replace`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReplacePlanRequest {
    /// e.g. "md2"; empty for a flexible volume (named by `volume`).
    #[serde(default)]
    pub array: String,
    /// Member to replace: component ("sdb3") or whole disk ("sdb"); for a flexible volume a
    /// whole disk, or "missing" for the disk gone from its degraded tiers.
    pub old: String,
    /// New whole disk.
    pub new_disk: String,
    /// A flexible volume's label: the new disk takes the old one's place in every tier
    /// (and, when bigger, joins the tiers it now reaches).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
}

/// `POST /api/v1/storage/plan/complete` (and `POST /storage/volumes/{name}/complete` to run
/// it): what is left of an interrupted growth of a flexible volume (`FlexLayout.pending`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CompletePlanRequest {
    /// The flexible volume's label.
    pub volume: String,
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
    /// A failure is a warning in the task log and the plan goes on (clearing a replaced disk
    /// that the volume no longer uses: it may be unreadable, or another disk by now).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub best_effort: bool,
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
    /// Flexible RAID: the tiers before the change (empty for a new volume)...
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers_before: Vec<FlexTier>,
    /// ...and after it, for the preview.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers_after: Vec<FlexTier>,
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
    /// The flexible RAID breaks the tier rule (made elsewhere), or flexible RAID cannot be
    /// planned for this operation yet.
    FlexLayoutUnsupported,
    /// A growth of the flexible RAID was interrupted: complete it first.
    FlexGrowthPending,
    /// The fault tolerance of a flexible RAID cannot be changed.
    ToleranceChangeUnsupported,
    /// The last scrub of the filesystem found errors it could not correct: a reshape would
    /// spread them; restore the damaged files first.
    ScrubErrors,
    /// A flexible RAID needs LVM, and `lvm` (package lvm2) is not installed.
    LvmMissing,
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
    /// Flexible RAID: `items` are disks with space no tier can use yet (`size_bytes` in all),
    /// until another disk reaches the same size.
    FlexUnusedSpace,
    /// Flexible RAID: the reshapes move `size_bytes` and take about `count` hours in all
    /// (estimated at 50 MB/s per tier, one tier at a time); the volume stays online.
    FlexLongReshape,
    /// Flexible RAID: tier `subject` tolerates only one disk while it is reshaped to RAID6.
    FlexReducedToleranceWindow,
    /// Flexible RAID: the new disk `subject` misses `size_bytes` to hold tier `count` (and
    /// the tiers above): it joins only the tiers below.
    DiskShortOfTier,
    /// Flexible RAID: no scrub of the filesystem on `subject` (mount point) finished without
    /// errors in the last `count` days: a read error during the reshapes is the main risk.
    FlexNoRecentScrub,
    /// Flexible RAID: `subject` holds old slices of the volume in no tier and is not erased
    /// by this plan (an old superblock on it can confuse assembling the tiers at boot):
    /// add it to a volume or erase it.
    FlexLeftoverDisk,
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
    /// Flexible RAID: make tier array `subject` an LVM physical volume.
    CreatePv,
    /// Flexible RAID: create the volume group `subject` from the tier arrays.
    CreateVg,
    /// Flexible RAID: create the logical volume `subject` over all the free space.
    CreateLv,
    /// Flexible RAID: add tier array `other` to the volume group `subject`.
    ExtendVg,
    /// Flexible RAID: grow the physical volume on tier array `subject` to the array size.
    ResizePv,
    /// Flexible RAID: grow the logical volume `subject` over the free space.
    ExtendLv,
    /// Turn off the write-intent bitmap of `subject` (a level change or reshape needs it).
    BitmapOff,
    /// Turn the write-intent bitmap of `subject` back on.
    BitmapOn,
    /// Flexible RAID: tell the kernel about partition `subject` on a disk in use.
    AddPartitionLive,
    /// Flexible RAID: delete the volume's own partitions `items` of disk `subject` (the disk
    /// has other partitions, which stay).
    DeletePartitions,
    /// Flexible RAID: tell the kernel that partition `subject`, deleted on a disk in use, is
    /// gone.
    RemovePartitionLive,
    /// btrfs: mount `subject` (a mount point) with `-o degraded`, devices missing.
    BtrfsMountDegraded,
    /// btrfs: rebuild missing device `other` (a devid) on disk `subject`.
    BtrfsReplace,
    /// btrfs: wait for the rebuild on `subject` to end.
    BtrfsWaitReplace,
    /// btrfs: grow device `other` (a devid) of `subject` to the whole disk.
    BtrfsResize,
    /// btrfs: remove the missing devices of `subject`, the others keeping its profile.
    BtrfsRemoveMissing,
    /// btrfs: convert the weaker chunks of `subject` back to the profiles `items` (`data
    /// raid1`...), a soft balance.
    BtrfsConvert,
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
// | POST | `/api/v1/storage/volumes/{name}/complete` | [`VolumeCompleteRequest`] | 202 / 200 (dry run) |
// | POST | `/api/v1/storage/volumes/{name}/scrub` | [`VolumeScrubRequest`] | 202 / 200 (dry run) |
// | POST | `/api/v1/storage/volumes/{name}/unlock` | [`UnlockRequest`] | 202 |
// | POST | `/api/v1/storage/volumes/{name}/lock` | — | 202 |
// | POST | `/api/v1/storage/volumes/{name}/reveal-recovery-key` | — | [`RecoveryKey`], once |
// | GET/DELETE | `/api/v1/storage/volumes/{name}/luks-header` | — | `application/octet-stream` / 204 |
// | GET/PUT | `/api/v1/storage/volumes/{name}/settings` | [`VolumeSettings`] | [`VolumeSettingsState`] |
// | GET/POST | `/api/v1/storage/volumes/{name}/subvolumes` | [`SubvolumeCreateRequest`] | `Vec<SubvolumeInfo>` / 202 |
// | PUT/DELETE | `/api/v1/storage/volumes/{name}/subvolumes/{sub}?confirm=` | [`SubvolumeSettings`] | 202 |
// | GET/PUT | `/api/v1/storage/disks/{name}/settings` | [`DiskSettings`] | [`DiskSettingsState`] |
// | POST/DELETE | `/api/v1/storage/disks/{name}/health-ack` | [`HealthAckRequest`] / — | [`HealthAckResult`] / 204 |

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
    /// Serials of every disk the plan erases: the new disks, and any other disk its steps
    /// destroy (a leftover disk of a flexible volume).
    #[serde(default)]
    pub serials: Vec<String>,
    /// Needed when the volume is encrypted and the LUKS mapping is grown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

/// `POST /api/v1/storage/volumes/{name}/replace`: replace a member (failed or not).
#[derive(Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VolumeReplaceRequest {
    /// Member to replace: component ("sdb1") or whole disk ("sdb").
    pub old: String,
    pub new_disk: String,
    #[serde(default)]
    pub confirm: String,
    /// Serials of every disk the plan erases: the new disk, and the old one when it is still
    /// attached and the plan clears it.
    #[serde(default)]
    pub serials: Vec<String>,
    /// Flexible RAID, encrypted: needed when a bigger disk grows the LUKS mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

/// `POST /api/v1/storage/volumes/{name}/complete`: finish an interrupted growth of a
/// flexible volume (the plan of [`CompletePlanRequest`]).
#[derive(Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VolumeCompleteRequest {
    /// Needed when the volume is encrypted and the LUKS mapping is grown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
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
redacted_debug!(VolumeReplaceRequest, old, new_disk, confirm, serials, dry_run);
redacted_debug!(VolumeCompleteRequest, dry_run);
redacted_debug!(UnlockRequest);
redacted_debug!(RecoveryKey);

/// A rynas volume as recorded in the configuration (key `storage.volumes`): the source of
/// the `/etc/fstab`, `/etc/crypttab` and `/etc/mdadm/mdadm.conf` entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VolumeRecord {
    pub label: String,
    /// md array UUID as mdadm prints it (`xxxxxxxx:xxxxxxxx:xxxxxxxx:xxxxxxxx`). Empty for a
    /// flexible RAID, whose arrays are in `tiers`.
    pub md_uuid: String,
    pub level: RaidLevel,
    /// Flexible RAID: one md array per tier (`/dev/md/<label>_t<index>`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<TierRecord>,
    /// Flexible RAID: the LVM volume group (`rynas_<label>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vg: Option<String>,
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
    /// Flexible RAID: the growth or replace under way (recorded before its first step,
    /// cleared when it ends).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<FlexChange>,
}

/// A growth or replace of a flexible RAID under way ([`VolumeRecord::change`]): planning
/// again, with the same request or with complete, goes on with it, whatever step it stopped
/// at (power loss, rynasd restarted during a long wait, a failed or cancelled step).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FlexChange {
    /// The disk a replace takes out (`name` is `missing` for a disk gone); `None` for a
    /// growth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<DiskRef>,
    /// The disks joining (the one new disk of a replace).
    pub new: Vec<DiskRef>,
}

/// A disk as a record names it: its kernel name then, and its identity (serial, or the
/// kernel name for a disk without one), which finds it again under another name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiskRef {
    pub name: String,
    #[serde(default)]
    pub id: String,
}

/// One tier array of a flexible RAID [`VolumeRecord`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TierRecord {
    /// 1-based tier number (`<label>_t<index>`).
    pub index: u32,
    /// md array UUID as mdadm prints it.
    pub md_uuid: String,
}

impl VolumeRecord {
    /// Every md array of the volume: the one array of a classic volume, or the tiers.
    pub fn md_uuids(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.tiers.iter().map(|t| t.md_uuid.as_str()).collect();
        if !self.md_uuid.is_empty() {
            v.insert(0, self.md_uuid.as_str());
        }
        v
    }
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
    /// btrfs filesystems (UUIDs) not mounted degraded after boot when a device is missing:
    /// an administrator mounts them ([`BtrfsSettings::auto_degraded`] off).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub btrfs_manual: Vec<String>,
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
    /// Only on creation: no copy-on-write (`chattr +C` on the empty subvolume), see
    /// [`crate::ShareSubvolume::nocow`]. Ignored by `PUT`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nocow: bool,
}

impl Default for SubvolumeSettings {
    fn default() -> Self {
        Self {
            compression: true,
            readonly: false,
            nocow: false,
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
                best_effort: false,
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

    #[test]
    fn flexible_levels() {
        assert_eq!(serde_json::to_string(&RaidLevel::Flex1).unwrap(), r#""flex1""#);
        assert_eq!(
            serde_json::from_str::<RaidLevel>(r#""flex2""#).unwrap(),
            RaidLevel::Flex2
        );
        assert!(RaidLevel::Flex1.is_flexible() && !RaidLevel::Raid5.is_flexible());
        assert_eq!(RaidLevel::Flex1.tolerance(), 1);
        assert_eq!(RaidLevel::Flex2.tolerance(), 2);
        assert_eq!(RaidLevel::Raid6.tolerance(), 2);
        assert_eq!(RaidLevel::Single.tolerance(), 0);
        assert_eq!(RaidLevel::flexible(2), Some(RaidLevel::Flex2));
        assert_eq!(RaidLevel::flexible(0), None);
    }

    /// Records, trees and plans written before flexible RAID still parse, and a classic
    /// record is written exactly as before.
    #[test]
    fn flexible_fields_are_optional() {
        let old = r#"{"label":"vol1","md_uuid":"0a1b2c3d:4e5f6071:8293a4b5:c6d7e8f9","level":"raid5",
            "fs_uuid":"3e6c1f0a-9b2d-4c8e-a7f1-5d0b2c4e6a81","mountpoint":"/vol1","options":"noatime"}"#;
        let r: VolumeRecord = serde_json::from_str(old).unwrap();
        assert!(r.tiers.is_empty() && r.vg.is_none());
        assert_eq!(r.md_uuids(), ["0a1b2c3d:4e5f6071:8293a4b5:c6d7e8f9"]);
        let j = serde_json::to_string(&r).unwrap();
        assert!(!j.contains("tiers") && !j.contains("vg"), "{j}");

        let flex = VolumeRecord {
            label: "vol2".into(),
            level: RaidLevel::Flex1,
            tiers: vec![
                TierRecord { index: 1, md_uuid: "11111111:22222222:33333333:44444444".into() },
                TierRecord { index: 2, md_uuid: "55555555:66666666:77777777:88888888".into() },
            ],
            vg: Some("rynas_vol2".into()),
            ..Default::default()
        };
        assert_eq!(flex.md_uuids().len(), 2);
        let back: VolumeRecord =
            serde_json::from_str(&serde_json::to_string(&flex).unwrap()).unwrap();
        assert_eq!(back, flex);

        let tree: VolumeTree = serde_json::from_str(
            r#"{"id":"u","label":null,"fstype":"btrfs","mountpoint":null,"size":0,"used":0,
            "health":"healthy","encrypted":false,"layers":[],"subvolumes":[]}"#,
        )
        .unwrap();
        assert!(tree.flexible.is_none());
        let p: StoragePlan = serde_json::from_str(
            r#"{"operation":"scrub","valid":true,"errors":[],"warnings":[],"raw_bytes":0,
            "usable_bytes":0,"fault_tolerance":0,"wasted_bytes":0,"summary":[],"steps":[]}"#,
        )
        .unwrap();
        assert!(p.tiers_before.is_empty() && p.tiers_after.is_empty());
        let k: FlexPendingKind = serde_json::from_str(r#""from_the_future""#).unwrap();
        assert_eq!(k, FlexPendingKind::Other);
    }

    #[test]
    fn flex_layout_health_is_the_worst_tier() {
        let tier = |h| FlexTier { health: h, ..Default::default() };
        let l = FlexLayout {
            tolerance: 1,
            tiers: vec![tier(ArrayHealth::Healthy), tier(ArrayHealth::Degraded)],
            ..Default::default()
        };
        assert_eq!(l.health(), ArrayHealth::Degraded);
        assert_eq!(l.level(), Some(RaidLevel::Flex1));
        assert_eq!(FlexLayout::default().health(), ArrayHealth::Unknown);
    }
}
