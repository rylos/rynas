//! PC backup with `rynas-agent`, file method (D-PCBACKUP, 2026-11-11): restic-format
//! repositories served by rynas with per-PC rules, one per **backup group** (by default one
//! group per PC; PCs in one group deduplicate against each other and can read each other's
//! data), and a layout manifest for bare-metal restore. The other method, btrfs snapshots
//! received with btrbk, is [`crate::pcbackup`].
//! API in `docs/phase1/plan.md` "API v1: PC backup".
//!
//! The same types are the wire format of the console (`/api/v1/pcbackup/…`), of the agent
//! (`/api/v1/pcbackup/agent/…`) and of the manifest the agent writes to
//! `/.rynas/manifest.json` on the PC.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::backup::Retention;

/// Where the manifest lives on the PC (inside the root filesystem, so every version has it).
pub const MANIFEST_PATH: &str = "/.rynas/manifest.json";
/// The manifest format (one schema for both PC methods: the file method fills
/// [`Manifest::filesystems`] and friends, the btrfs method adds [`Manifest::btrbk`]).
pub const MANIFEST_FORMAT: u32 = 1;
/// The `Accept` value of the restic REST protocol v2 listings.
pub const REST_V2: &str = "application/vnd.x.restic.rest.v2";
/// Longest manifest the NAS keeps (display only).
pub const MAX_MANIFEST: usize = 1 << 20;
/// Longest `changed_during` list in a report.
pub const MAX_CHANGED: usize = 200;

// ------------------------------------------------------------------ plans and settings

/// How filesystems without a snapshot (ext2/3/4, XFS without LVM) are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FreezeMode {
    /// Read while in use; files changed during the backup are listed.
    #[default]
    Hot,
    /// Frozen for the whole run, thawed at `freeze_max_secs` at the latest (then hot).
    Freeze,
}

/// What the NAS tells the agent to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcPlan {
    /// Hours between two backups (1–168).
    pub interval_hours: u32,
    /// Minutes after the agent starts before a due backup runs.
    #[serde(default)]
    pub start_delay_min: u32,
    /// Exclude globs, on top of the agent's own (caches, `/tmp`, swap files).
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub freeze: FreezeMode,
    /// Longest freeze in seconds (5–3600).
    #[serde(default = "default_freeze_max")]
    pub freeze_max_secs: u32,
    /// No scheduled backups ("Back up now" still runs).
    #[serde(default)]
    pub paused: bool,
}

fn default_freeze_max() -> u32 {
    60
}

impl Default for PcPlan {
    fn default() -> Self {
        PcPlan {
            interval_hours: 24,
            start_delay_min: 10,
            excludes: Vec::new(),
            freeze: FreezeMode::Hot,
            freeze_max_secs: default_freeze_max(),
            paused: false,
        }
    }
}

/// Settings of one PC (`PUT /api/v1/pcbackup/devices/{device}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcDeviceSettings {
    #[serde(default)]
    pub description: String,
    pub plan: PcPlan,
    pub retention: Retention,
    /// No version younger than this is ever forgotten.
    #[serde(default)]
    pub min_age_days: u32,
    /// Late after this many hours without a good version (0: never).
    #[serde(default = "default_alert")]
    pub alert_hours: u32,
    /// Critical after this many hours.
    #[serde(default = "default_critical")]
    pub critical_hours: u32,
}

fn default_alert() -> u32 {
    26
}

fn default_critical() -> u32 {
    72
}

impl Default for PcDeviceSettings {
    fn default() -> Self {
        PcDeviceSettings {
            description: String::new(),
            plan: PcPlan::default(),
            retention: Retention::SMART,
            min_age_days: 7,
            alert_hours: default_alert(),
            critical_hours: default_critical(),
        }
    }
}

/// Settings of the feature (`PUT /api/v1/pcbackup/settings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcAgentSettings {
    /// The folder that holds one repository per backup group; empty: not set up yet.
    #[serde(default)]
    pub folder: String,
    /// Quota of new groups (0: none).
    #[serde(default)]
    pub default_quota_gib: u32,
    /// Hour (0–23, local time) of the daily retention, prune and check.
    #[serde(default = "default_hour")]
    pub maintenance_hour: u8,
    /// Share of the NAS' memory (10–80 %) a maintenance may use: the worker's unit gets it
    /// as `MemoryMax`, and a prune whose estimate does not fit is postponed.
    #[serde(default = "default_memory_pct")]
    pub maintenance_memory_pct: u8,
    /// Average chunk of new groups' repositories; a repository keeps its own for good.
    #[serde(default)]
    pub chunking: PcChunking,
}

/// Average chunk of a new group's repository. Smaller chunks deduplicate a little better;
/// larger ones mean half the blobs, so about half the memory of prune and check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PcChunking {
    /// 1 MiB with 8 GiB of memory or more, 2 MiB below.
    #[default]
    Auto,
    /// 1 MiB (512 KiB–8 MiB), restic's own.
    Small,
    /// 2 MiB (1–16 MiB).
    Large,
}

/// Memory from which [`PcChunking::Auto`] picks 1 MiB chunks.
pub const AUTO_SMALL_CHUNKS_FROM: u64 = 8 << 30;

impl PcChunking {
    /// The average chunk in MiB for a NAS with `mem_total` bytes of memory.
    pub fn mib(self, mem_total: u64) -> u8 {
        match self {
            Self::Small => 1,
            Self::Large => 2,
            Self::Auto if mem_total >= AUTO_SMALL_CHUNKS_FROM => 1,
            Self::Auto => 2,
        }
    }
}

fn default_hour() -> u8 {
    4
}

fn default_memory_pct() -> u8 {
    40
}

// ------------------------------------------------------------------ console views

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PcState {
    /// Paired, no version yet.
    #[default]
    Waiting,
    Ok,
    Running,
    Late,
    Critical,
    /// The last run failed (and the last good one is not late yet).
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    #[default]
    Preparing,
    /// Snapshots, freezes, the manifest.
    Snapshot,
    /// Reading and uploading.
    Uploading,
    Finishing,
}

/// Live progress of a run (from the agent every 2 s).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcProgress {
    pub run_id: String,
    pub phase: AgentPhase,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default)]
    pub bytes_total: u64,
    #[serde(default)]
    pub files_done: u64,
    #[serde(default)]
    pub files_total: u64,
    /// Bytes per second over the last reports.
    #[serde(default)]
    pub speed_bps: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub updated_ms: u64,
}

/// How one filesystem was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMethod {
    /// btrfs read-only snapshot.
    BtrfsSnapshot,
    /// LVM snapshot mounted read-only.
    LvmSnapshot,
    /// Frozen for the whole run.
    Frozen,
    /// Frozen, thawed by the watchdog before the end (then read hot).
    FrozenThawed,
    /// Read while in use.
    Hot,
    /// Copied as files (the ESP, vfat).
    Files,
    /// Not in the backup.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capture {
    pub path: String,
    pub fstype: String,
    pub method: CaptureMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One run of the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcRun {
    pub run_id: String,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(default)]
    pub files: u64,
    #[serde(default)]
    pub bytes_read: u64,
    #[serde(default)]
    pub bytes_added: u64,
    /// Files changed while they were read (up to [`MAX_CHANGED`]).
    #[serde(default)]
    pub changed_during: Vec<String>,
    #[serde(default)]
    pub changed_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub captures: Vec<Capture>,
    /// A bare-metal restore from the live system (not a backup).
    #[serde(default)]
    pub restore: bool,
}

/// Why a PC is (not) ready for a bare-metal restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyReason {
    NoVersion,
    NoManifest,
    /// The newest manifest is older than the newest version by more than a day.
    ManifestStale,
    /// A mounted filesystem was skipped.
    VolumeSkipped,
    LoaderUnknown,
    /// No partition table could be read.
    NoPartitionTable,
    /// The group has no recovery kit yet.
    NoRecoveryKit,
    /// LVM or md: recreated by hand in this version of the restore.
    ManualLayout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BareMetalReady {
    pub ready: bool,
    #[serde(default)]
    pub reasons: Vec<ReadyReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcMaintenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ms: Option<u64>,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub freed_bytes: u64,
    #[serde(default)]
    pub removed_versions: u32,
    /// Memory the next prune is estimated to need (from the index: blobs × bytes per entry).
    #[serde(default)]
    pub estimate_bytes: u64,
    /// Blobs in the index when it was last counted.
    #[serde(default)]
    pub blobs: u64,
    /// Why the last scheduled maintenance did not run (over the memory budget, PC busy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postponed: Option<String>,
}

/// A command waiting for the agent's next poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentCommand {
    BackupNow { id: u64 },
    Cancel { id: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcDevice {
    pub device: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub agent_version: String,
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<u64>,
    pub state: PcState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<PcRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<PcProgress>,
    /// Its backup group (the repository).
    pub group: String,
    /// Bytes this PC uploaded into the group's repository (new data only, before prunes).
    #[serde(default)]
    pub uploaded_bytes: u64,
    #[serde(default)]
    pub versions: u32,
    pub settings: PcDeviceSettings,
    #[serde(default)]
    pub bare_metal: BareMetalReady,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<AgentCommand>,
    /// Revoked: no token, no key; the versions stay readable until the group is deleted.
    #[serde(default)]
    pub revoked: bool,
}

/// Settings of a backup group (`PUT /api/v1/pcbackup/groups/{group}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcGroupSettings {
    #[serde(default)]
    pub description: String,
    /// Size limit of the repository (0: none).
    #[serde(default)]
    pub quota_gib: u32,
}

/// A backup group: one repository, one or more PCs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcGroup {
    pub group: String,
    pub settings: PcGroupSettings,
    /// The repository folder on the NAS.
    pub repository: String,
    pub created_ms: u64,
    /// PCs that write here (revoked ones included).
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default)]
    pub used_bytes: u64,
    #[serde(default)]
    pub maintenance: PcMaintenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_kit_ms: Option<u64>,
    /// Average chunk of its repository in MiB, fixed when it was created.
    #[serde(default)]
    pub chunk_mib: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcDeviceDetail {
    pub device: PcDevice,
    #[serde(default)]
    pub history: Vec<PcRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<Manifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairingView {
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub expires_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcBackupOverview {
    pub settings: PcAgentSettings,
    pub groups: Vec<PcGroup>,
    pub devices: Vec<PcDevice>,
    #[serde(default)]
    pub pairings: Vec<PairingView>,
    /// Memory a maintenance may use on this NAS (`maintenance_memory_pct` of the RAM).
    #[serde(default)]
    pub memory_budget_bytes: u64,
    /// SHA-256 of the certificate the web interface serves (hex, `:` separated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub dry: bool,
}

/// `POST /api/v1/pcbackup/pairings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairingRequest {
    #[serde(default)]
    pub description: String,
    /// Join this existing group; `None`: a new group of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Quota of a new group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_gib: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairingCreated {
    pub id: String,
    pub code: String,
    pub expires_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// The command to run on the PC, with this NAS' address filled in by the browser.
    pub command: String,
}

/// The recovery kit of a group: enough to open its repository with stock restic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RecoveryKit {
    pub group: String,
    #[serde(default)]
    pub description: String,
    /// The PCs whose versions it opens.
    #[serde(default)]
    pub devices: Vec<String>,
    pub repository: String,
    pub password: String,
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreCodeCreated {
    pub code: String,
    pub expires_ms: u64,
    pub device: String,
}

// ------------------------------------------------------------------ the agent's calls

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairRequest {
    pub code: String,
    pub hostname: String,
    #[serde(default)]
    pub machine_id: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub agent_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentPlan {
    /// Changes with every new plan or command.
    pub rev: u64,
    pub plan: PcPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<AgentCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairResponse {
    pub device: String,
    /// The repository path is `/api/v1/pcbackup/repo/<group>/`.
    pub group: String,
    pub token: String,
    pub repo_password: String,
    #[serde(default)]
    pub nas_name: String,
    pub plan: AgentPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentStatus {
    pub run_id: String,
    pub phase: AgentPhase,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default)]
    pub bytes_total: u64,
    #[serde(default)]
    pub files_done: u64,
    #[serde(default)]
    pub files_total: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentReport {
    #[serde(flatten)]
    pub run: PcRun,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<Manifest>,
    #[serde(default)]
    pub agent_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreLogin {
    pub code: String,
}

/// The restore TUI's other login: an administrator with TOTP turned on (an account without
/// TOTP is refused). Checked by rynas-web, which then asks rynasd for a [`RestoreChoice`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreAdminLogin {
    pub username: String,
    pub password: String,
    pub totp: String,
}

/// The PCs an administrator may restore, each with a one-time restore code of its own (5
/// minutes; using one ends the others). The TUI logs in with the chosen PC's code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreChoice {
    pub devices: Vec<RestoreChoiceDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreChoiceDevice {
    pub device: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hostname: String,
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreDevice {
    pub device: String,
    pub group: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hostname: String,
    pub repo_password: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreSession {
    pub token: String,
    pub expires_ms: u64,
    pub devices: Vec<RestoreDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreReport {
    pub device: String,
    pub version: String,
    pub ok: bool,
    #[serde(default)]
    pub message: String,
    /// The end of the restore log (up to 64 KiB).
    #[serde(default)]
    pub log: String,
}

/// One entry of a restic REST v2 listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestEntry {
    pub name: String,
    pub size: u64,
}

// ------------------------------------------------------------------ the layout manifest

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Loader {
    Grub,
    SystemdBoot,
    Refind,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InitramfsTool {
    UpdateInitramfs,
    Mkinitcpio,
    Dracut,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BootInfo {
    pub uefi: bool,
    pub loader: Loader,
    #[serde(default)]
    pub shim: bool,
    #[serde(default)]
    pub uki: bool,
    /// Mount point of the ESP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub esp: Option<String>,
    /// The loader's folder in `EFI/` (`debian`, `arch`, `fedora`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efi_id: Option<String>,
    pub initramfs: InitramfsTool,
    /// `/proc/cmdline` at capture.
    #[serde(default)]
    pub cmdline: String,
    /// `/etc/kernel/cmdline`, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_cmdline: Option<String>,
    /// `GRUB_CMDLINE_LINUX` of `/etc/default/grub`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grub_cmdline: Option<String>,
    /// Installed kernel versions (`/usr/lib/modules`).
    #[serde(default)]
    pub kernels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Partition {
    pub number: u32,
    pub start_lba: u64,
    pub size_lba: u64,
    /// GPT type GUID (or `0x83`-style MBR type).
    pub type_guid: String,
    #[serde(default)]
    pub part_guid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub attrs: u64,
    /// Kernel name at capture (`nvme0n1p2`).
    #[serde(default)]
    pub dev: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PartTable {
    /// `gpt` or `dos`.
    pub kind: String,
    #[serde(default)]
    pub disk_guid: String,
    #[serde(default)]
    pub first_lba: u64,
    #[serde(default)]
    pub last_lba: u64,
    #[serde(default)]
    pub partitions: Vec<Partition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Disk {
    /// Kernel name at capture (`nvme0n1`); never used to find the disk again.
    pub name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub serial: String,
    #[serde(default)]
    pub wwn: String,
    pub size_bytes: u64,
    #[serde(default = "default_sector")]
    pub logical_sector: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<PartTable>,
}

fn default_sector() -> u32 {
    512
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MountEntry {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subvol: Option<String>,
    #[serde(default)]
    pub options: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Subvolume {
    /// Path from the top level (`@home`, `@/var/lib/machines`).
    pub path: String,
    #[serde(default)]
    pub id: u64,
    /// Included in the backup (otherwise recreated empty).
    #[serde(default)]
    pub captured: bool,
    #[serde(default)]
    pub nocow: bool,
}

/// Where a filesystem sits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FsSource {
    /// A partition of a disk of the manifest (`disk` = its index in `disks`).
    Partition { disk: usize, number: u32 },
    /// An opened LUKS container (index in `luks`).
    Luks { luks: usize },
    /// An LVM logical volume.
    Lvm { vg: String, lv: String },
    /// An md array.
    Md { name: String },
    /// A whole disk (index in `disks`).
    Disk { disk: usize },
    #[default]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Filesystem {
    /// Device at capture (`/dev/nvme0n1p2`, `/dev/mapper/root`).
    pub device: String,
    pub source: FsSource,
    pub fstype: String,
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub mounts: Vec<MountEntry>,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub used_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureMethod>,
    /// Superblock features (ext: `metadata_csum`, `64bit`…), for information.
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub subvolumes: Vec<Subvolume>,
    /// Default subvolume (btrfs), as a path from the top level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_subvol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Luks {
    /// The container's device (`/dev/nvme0n1p3`).
    pub device: String,
    pub source: FsSource,
    pub uuid: String,
    #[serde(default)]
    pub label: String,
    /// 1 or 2.
    pub version: u32,
    #[serde(default)]
    pub cipher: String,
    #[serde(default)]
    pub key_bits: u32,
    #[serde(default)]
    pub pbkdf: String,
    /// The mapper name it was open as.
    #[serde(default)]
    pub mapper: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Swap {
    /// `/dev/…` or a file path.
    pub path: String,
    #[serde(default)]
    pub file: bool,
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub source: FsSource,
}

/// Everything a bare-metal restore needs besides the files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Manifest {
    /// [`MANIFEST_FORMAT`]; a reader refuses a format it does not know.
    pub format: u32,
    pub written_at_ms: u64,
    #[serde(default)]
    pub agent_version: String,
    pub hostname: String,
    #[serde(default)]
    pub machine_id: String,
    #[serde(default)]
    pub arch: String,
    /// `ID`, `VERSION_ID`, `PRETTY_NAME`, `ID_LIKE` of os-release.
    #[serde(default)]
    pub os_release: BTreeMap<String, String>,
    pub boot: BootInfo,
    pub disks: Vec<Disk>,
    #[serde(default)]
    pub filesystems: Vec<Filesystem>,
    #[serde(default)]
    pub luks: Vec<Luks>,
    #[serde(default)]
    pub swap: Vec<Swap>,
    /// `vgs`/`lvs --reportformat json`, when LVM is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lvm_report: Option<String>,
    /// `mdadm --detail --export` of each array.
    #[serde(default)]
    pub md: Vec<String>,
    #[serde(default)]
    pub fstab: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crypttab: Option<String>,
    /// The btrfs method: btrbk's names of the subvolumes (`docs/research/pc-backup.md`
    /// §11.3). Absent on PCs of the file method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btrbk: Option<BtrbkLayout>,
}

/// The btrfs method's block of the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BtrbkLayout {
    /// btrbk's `snapshot_dir` on the PC.
    #[serde(default)]
    pub snapshot_dir: String,
    pub subvolumes: Vec<BtrbkSubvolume>,
    /// Boot filesystems outside every sent subvolume (the ESP, a separate non-btrfs
    /// `/boot`), copied as files into the root subvolume so its snapshots carry them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boot_copies: Vec<BootCopy>,
}

/// One boot filesystem copied into `/.rynas/boot/<name>/` (one copy kept, replaced whole).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BootCopy {
    /// The mount point copied (`/boot/efi`).
    pub mount: String,
    /// Where the copy is on the PC (`/.rynas/boot/boot-efi`).
    pub dir: String,
    pub bytes: u64,
    pub files: u64,
    /// SHA-256 of the copied names, sizes and times: an unchanged tree is not copied again.
    #[serde(default)]
    pub signature: String,
    /// Files left out because they look like key material (`*.key`, `*keyfile*`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

/// One subvolume the btrfs method sends: btrbk name → subvolume path → mount point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BtrbkSubvolume {
    /// btrbk's name (the prefix of its snapshots, `@home`).
    pub name: String,
    /// Path from the top level.
    #[serde(default)]
    pub path: String,
    /// Where it is mounted (`/home`); empty when not mounted.
    #[serde(default)]
    pub mount: String,
}

impl Manifest {
    /// The filesystem mounted at `/`.
    pub fn root(&self) -> Option<&Filesystem> {
        self.filesystems
            .iter()
            .find(|f| f.mounts.iter().any(|m| m.path == "/"))
    }

    /// The bare-metal readiness of a version with this manifest (without the NAS-side
    /// reasons: versions, recovery kit, age).
    pub fn ready_reasons(&self) -> Vec<ReadyReason> {
        let mut r = Vec::new();
        if self.disks.iter().all(|d| d.table.is_none()) {
            r.push(ReadyReason::NoPartitionTable);
        }
        if self.boot.loader == Loader::Unknown {
            r.push(ReadyReason::LoaderUnknown);
        }
        if self
            .filesystems
            .iter()
            .any(|f| !f.mounts.is_empty() && f.capture == Some(CaptureMethod::Skipped))
        {
            r.push(ReadyReason::VolumeSkipped);
        }
        if self.lvm_report.is_some()
            || !self.md.is_empty()
            || self
                .filesystems
                .iter()
                .any(|f| matches!(f.source, FsSource::Lvm { .. } | FsSource::Md { .. }))
        {
            r.push(ReadyReason::ManualLayout);
        }
        r
    }
}

/// The tag of every version of `device` (versions and retention are per PC inside a group).
pub fn device_tag(device: &str) -> String {
    format!("pc-{device}")
}

/// The tag every agent version carries.
pub const AGENT_TAG: &str = "rynas-pc";

/// A device name the NAS accepts: 1–32 of `[a-z0-9-]`, starting with a letter or digit.
pub fn valid_device(d: &str) -> bool {
    !d.is_empty()
        && d.len() <= 32
        && d.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !d.starts_with('-')
}

/// A device name from a host name: lower case, other characters as `-`, at most 32.
pub fn device_from_hostname(h: &str) -> String {
    let mut s: String = h
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() { c } else { '-' }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s: String = s.trim_matches('-').chars().take(32).collect();
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { "pc".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names() {
        assert_eq!(device_from_hostname("Marco's Laptop.lan"), "marco-s-laptop-lan");
        assert_eq!(device_from_hostname("..."), "pc");
        assert!(valid_device("pc-1"));
        assert!(!valid_device("-pc"));
        assert!(!valid_device("PC"));
        assert!(!valid_device(&"a".repeat(33)));
    }

    #[test]
    fn report_flattens_the_run() {
        let r = AgentReport {
            run: PcRun {
                run_id: "r1".into(),
                ok: true,
                ..Default::default()
            },
            manifest: None,
            agent_version: "0.1.0".into(),
        };
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["run_id"], "r1");
        let back: AgentReport = serde_json::from_value(j).unwrap();
        assert_eq!(back, r);
    }

    /// The fields the btrfs method's NAS side reads (`rynasd::pc_backup`): `format`,
    /// `written_at_ms`, `boot.loader`, `btrbk.subvolumes[].name` and `.mount`.
    #[test]
    fn manifest_shape_shared_by_both_methods() {
        let m = Manifest {
            format: MANIFEST_FORMAT,
            written_at_ms: 5,
            hostname: "pc".into(),
            boot: BootInfo { loader: Loader::SystemdBoot, ..Default::default() },
            btrbk: Some(BtrbkLayout {
                snapshot_dir: "/.snapshots".into(),
                subvolumes: vec![BtrbkSubvolume { name: "@".into(), path: "@".into(), mount: "/".into() }],
                boot_copies: Vec::new(),
            }),
            ..Default::default()
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["format"], 1);
        assert_eq!(v["written_at_ms"], 5);
        assert_eq!(v["boot"]["loader"], "systemd_boot");
        assert_eq!(v["btrbk"]["subvolumes"][0]["name"], "@");
        assert_eq!(v["btrbk"]["subvolumes"][0]["mount"], "/");
        let plain = serde_json::to_value(Manifest::default()).unwrap();
        assert!(plain.get("btrbk").is_none());
    }

    #[test]
    fn readiness() {
        let mut m = Manifest {
            disks: vec![Disk {
                name: "vda".into(),
                size_bytes: 1,
                logical_sector: 512,
                table: Some(PartTable::default()),
                ..Default::default()
            }],
            ..Default::default()
        };
        m.boot.loader = Loader::Grub;
        assert!(m.ready_reasons().is_empty());
        m.filesystems.push(Filesystem {
            fstype: "xfs".into(),
            mounts: vec![MountEntry { path: "/srv".into(), ..Default::default() }],
            capture: Some(CaptureMethod::Skipped),
            ..Default::default()
        });
        assert_eq!(m.ready_reasons(), vec![ReadyReason::VolumeSkipped]);
    }
}
