//! PC backup, receive side (D-PCBACKUP phase 2): folders on the NAS where Linux PCs push their
//! btrbk snapshots over SSH (`target ssh://nas/<path>` in the PC's `btrbk.conf`).
//!
//! Every PC (a "device") gets a btrfs subvolume folder and one line in the `authorized_keys`
//! of the dedicated account [`ACCOUNT`]: its own public key, restricted to rynas' filter with
//! that folder only. See `docs/research/pc-backup.md` §10.

use serde::{Deserialize, Serialize};

use crate::snapshots::Retention;

/// The system account PCs log in as. One account, one key per device.
pub const ACCOUNT: &str = "rynas-pcbackup";

/// Where the filter is installed (the forced command of every key).
pub const FILTER: &str = "/usr/libexec/rynas/rynas-btrbk-filter";

/// A device name: `[a-z0-9][a-z0-9-]{0,31}` (also the last folder of the default path).
pub fn valid_device(d: &str) -> bool {
    let b = d.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Freshness of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReceiveState {
    /// Something was received within the alert window (or no alert is set).
    #[default]
    Ok,
    /// Nothing received for longer than `alert_days`.
    Late,
    /// Nothing received yet.
    Waiting,
    /// The folder is gone or is not a btrfs subvolume folder any more.
    Missing,
}

/// The NAS-side safety rules the filter applies to deletions asked by the PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletePolicy {
    /// btrbk on the PC may delete old snapshots here (`target_preserve`). Off: the PC can
    /// only add, the NAS keeps everything.
    pub allowed: bool,
    /// Never delete the newest N complete snapshots of each subvolume (0: no floor).
    pub keep_last: u32,
    /// Never delete a snapshot younger than N days (0: no minimum).
    pub min_age_days: u32,
}

impl Default for DeletePolicy {
    fn default() -> Self {
        Self {
            allowed: true,
            keep_last: 1,
            min_age_days: 0,
        }
    }
}

/// `btrfs filesystem du -s` of a device folder (on demand, a task).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReceiveUsage {
    /// Data referenced by all snapshots.
    pub total_bytes: u64,
    /// Data only these snapshots hold (freed if the folder went).
    pub exclusive_bytes: u64,
    /// Data shared with other subvolumes of the volume (or between snapshots).
    pub shared_bytes: u64,
    pub read_at_ms: u64,
}

/// One subvolume of the PC (snapshots grouped by the name before the timestamp).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReceivedGroup {
    /// btrbk's snapshot name (`home`, `rootfs`, `@`).
    pub name: String,
    pub snapshots: u32,
    /// The time written in the newest name.
    pub newest_ms: Option<u64>,
}

/// One received subvolume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReceivedSnapshot {
    pub name: String,
    pub group: String,
    /// The time in the name (the PC's clock).
    pub created_ms: Option<u64>,
    /// When the NAS created the subvolume (the receive started).
    pub received_at_ms: Option<u64>,
    pub readonly: bool,
    /// The PC's subvolume UUID: what keeps incremental chains working.
    pub received_uuid: Option<String>,
    /// Interrupted receive: writable and without received UUID. btrbk deletes it before it
    /// sends that snapshot again; the filter always lets it.
    pub incomplete: bool,
    /// The NAS-side rules would refuse a deletion of this snapshot now.
    pub protected: bool,
    /// Not a subvolume (a plain folder with a snapshot name).
    pub not_subvolume: bool,
    /// The newest complete snapshot of its subvolume (by the NAS-side creation time): the
    /// parent of the PC's next incremental send. The NAS never deletes it.
    #[serde(default)]
    pub parent: bool,
    /// Its size, when known (quota groups, or measured on demand).
    #[serde(default)]
    pub size: Option<SnapshotSize>,
}

/// The size of one received snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SnapshotSize {
    /// Data the snapshot references: about what a restore transfers.
    pub referenced_bytes: u64,
    /// Data only this snapshot holds: about what deleting it alone frees.
    pub exclusive_bytes: u64,
    /// When it was read (measured sizes stay until measured again; quota sizes are live).
    pub read_at_ms: u64,
    /// From the volume's quota groups (live) rather than measured.
    #[serde(default)]
    pub from_quota: bool,
}

/// Who deletes old snapshots of a PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RetentionOwner {
    /// btrbk on the PC (`target_preserve`), under the NAS rules of [`DeletePolicy`].
    #[default]
    Pc,
    /// The NAS, by [`PcRetention::gfs`]: the PC's key can only add snapshots (and remove its
    /// own interrupted receives).
    Nas,
}

/// Retention of the received snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcRetention {
    #[serde(default)]
    pub owner: RetentionOwner,
    /// With `owner: nas`: what the NAS keeps, per subvolume, by the time in the snapshot
    /// names (the same counts as snapshot jobs). The newest complete snapshot of each
    /// subvolume and the floor of [`DeletePolicy`] always stay.
    #[serde(default)]
    pub gfs: Retention,
}

/// The last time the NAS applied the retention of a PC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PruneRun {
    pub at_ms: u64,
    pub deleted: u32,
    /// Interrupted receives removed (part of `deleted`).
    #[serde(default)]
    pub interrupted: u32,
    /// Why it did not run or stopped (a receive was running, a delete failed).
    #[serde(default)]
    pub error: Option<String>,
}

/// Something wrong with a PC's backups (the alerts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PcIssueCode {
    /// Nothing new from the PC for longer than the alert time.
    Late,
    /// One subvolume stopped while the others go on (`group`).
    SubvolumeLate,
    /// A receive was interrupted and nothing is receiving now.
    ReceiveFailed,
    /// A receive has been running for many hours.
    ReceiveStuck,
    /// The volume has less free space than one more full set of the PC's snapshots.
    LowSpace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcIssue {
    pub code: PcIssueCode,
    /// The subvolume (btrbk name) or snapshot it is about.
    #[serde(default)]
    pub subject: Option<String>,
    /// Since when (the newest snapshot of the PC or subvolume, or the interrupted receive).
    #[serde(default)]
    pub since_ms: Option<u64>,
    /// Low space: free bytes and the bytes wanted.
    #[serde(default)]
    pub free_bytes: Option<u64>,
    #[serde(default)]
    pub needed_bytes: Option<u64>,
}

/// Whether the received snapshots are enough to rebuild the PC on an empty disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BareMetal {
    pub ready: bool,
    /// The snapshot the layout manifest (`/.rynas/manifest.json`) was read from.
    #[serde(default)]
    pub manifest_snapshot: Option<String>,
    /// Subvolumes (btrbk names) the manifest says a restore needs.
    #[serde(default)]
    pub subvolumes: Vec<String>,
    /// The PC's boot loader as the manifest names it.
    #[serde(default)]
    pub loader: Option<String>,
    /// The PC's layout from the manifest, for display: disks with their partitions, and
    /// btrbk name → subvolume path → mount point (`docs/research/pc-backup.md` §11.2.4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<crate::pcagent::Disk>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layout: Vec<crate::pcagent::BtrbkSubvolume>,
    /// Boot filesystems the PC copies into its root subvolume (ESP, separate `/boot`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boot_copies: Vec<crate::pcagent::BootCopy>,
    /// What stops a restore (empty when ready).
    #[serde(default)]
    pub missing: Vec<BareMetalGap>,
    /// What a restore can live with but should be looked at.
    #[serde(default)]
    pub warnings: Vec<BareMetalGap>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BareMetalCode {
    /// No complete snapshot received yet.
    NoSnapshots,
    /// No newest snapshot carries `/.rynas/manifest.json` (the PC does not write it).
    NoManifest,
    /// The manifest is there but cannot be read (too big, not JSON, wrong shape).
    ManifestInvalid,
    /// A manifest format this NAS does not know (`subject`: the format number).
    ManifestUnsupported,
    /// The manifest names no subvolume mounted at `/`.
    NoRoot,
    /// A subvolume the manifest needs has no complete snapshot here (`subject`).
    SubvolumeMissing,
    /// The manifest does not name the boot loader.
    LoaderUnknown,
    /// The manifest was written long before the snapshot that carries it (`subject`: days).
    ManifestStale,
    /// The newest run lacks a subvolume: a restore mixes runs (`subject`).
    LatestRunIncomplete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BareMetalGap {
    pub code: BareMetalCode,
    #[serde(default)]
    pub subject: Option<String>,
}

/// One backup run: the snapshots with the same timestamp (btrbk names every subvolume of a
/// run with one time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcRun {
    /// The timestamp of the names (`20261110T0300`).
    pub timestamp: String,
    pub created_ms: Option<u64>,
    /// Snapshot names, by subvolume.
    pub snapshots: Vec<String>,
    /// The expected subvolumes this run lacks (empty: a full set).
    pub lacks: Vec<String>,
    /// Some snapshot of the run is an interrupted receive.
    pub incomplete: bool,
    /// Sums over the run's snapshots when every size is known.
    pub referenced_bytes: Option<u64>,
    pub exclusive_bytes: Option<u64>,
}

/// A device folder, as listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcTarget {
    pub device: String,
    /// Free text (owner, model).
    pub description: String,
    pub path: String,
    /// `ssh-ed25519 AAAA… comment`, as pasted (empty: no key yet, the PC cannot log in).
    pub public_key: String,
    /// "type fingerprint" of the key, for the eye.
    pub key_fingerprint: Option<String>,
    /// Alert when nothing arrived for this many days (0: no alert).
    pub alert_days: u32,
    pub delete: DeletePolicy,
    /// Made from a folder that already had received snapshots.
    pub adopted: bool,
    pub created_at_ms: u64,
    pub state: ReceiveState,
    pub snapshots: u32,
    pub incomplete: u32,
    /// The newest receive (subvolume creation time on the NAS) of a complete snapshot.
    pub last_received_ms: Option<u64>,
    pub groups: Vec<ReceivedGroup>,
    pub usage: Option<ReceiveUsage>,
    /// Alert after this many hours without a new snapshot (0: `alert_days` counts).
    #[serde(default)]
    pub alert_hours: u32,
    #[serde(default)]
    pub retention: PcRetention,
    /// Subvolumes the PC no longer sends: no alert, not expected in a run.
    #[serde(default)]
    pub ignored_groups: Vec<String>,
    /// What is wrong now (the open alerts).
    #[serde(default)]
    pub issues: Vec<PcIssue>,
    #[serde(default)]
    pub bare_metal: BareMetal,
    /// Free and total bytes of the volume holding the folder.
    #[serde(default)]
    pub free_bytes: Option<u64>,
    #[serde(default)]
    pub volume_bytes: Option<u64>,
    /// A receive is running now.
    #[serde(default)]
    pub receiving: bool,
    #[serde(default)]
    pub last_prune: Option<PruneRun>,
}

impl PcTarget {
    /// The alert time in hours (0: off).
    pub fn alert_after_hours(&self) -> u32 {
        alert_hours(self.alert_hours, self.alert_days)
    }
}

/// `alert_hours` when set, else `alert_days` in hours (0: off).
pub fn alert_hours(hours: u32, days: u32) -> u32 {
    if hours > 0 { hours } else { days.saturating_mul(24) }
}

/// `GET /api/v1/pc-backups/targets/{device}`: the target and every snapshot, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcTargetDetail {
    pub target: PcTarget,
    pub list: Vec<ReceivedSnapshot>,
    /// The line rynas wrote in the account's `authorized_keys` (none without a key).
    pub key_line: Option<String>,
    /// For the PC: commands that make its key, its `btrbk.conf` lines, the first
    /// connection and a dry run (with `?host=`, the name the PC reaches the NAS by).
    pub key_commands: String,
    pub client_config: String,
    pub test_commands: String,
    /// The one command that does all of the above on a PC with `rynas-agent`
    /// (`rynas-agent btrbk-setup --nas ssh://… --device …`).
    #[serde(default)]
    pub agent_setup: String,
    /// The snapshots grouped by run, newest first.
    #[serde(default)]
    pub runs: Vec<PcRun>,
    /// Sizes come live from the volume's quota groups (otherwise from Measure).
    #[serde(default)]
    pub sizes_from_quota: bool,
}

/// `POST /api/v1/pc-backups/plan/retention`: what the NAS would delete now with these
/// settings (read only; the settings need not be saved).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcRetentionPlanRequest {
    pub device: String,
    pub retention: PcRetention,
    #[serde(default)]
    pub delete: DeletePolicy,
}

/// Why a snapshot stays (or goes) in a retention plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionReason {
    /// The parent of the next incremental send.
    Parent,
    /// The NAS floor: one of the newest N, or younger than the minimum age.
    Floor,
    /// Younger than the retention's minimum.
    Recent,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
    /// Named in the future: never deleted by the NAS on its own.
    Future,
    /// An interrupted receive that nothing is receiving any more.
    Interrupted,
    /// Beyond every count.
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionItem {
    pub name: String,
    pub group: String,
    pub keep: bool,
    pub reason: RetentionReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcRetentionPreview {
    /// Every snapshot, newest first.
    pub items: Vec<RetentionItem>,
    pub delete: u32,
    /// Sum of the exclusive sizes of what goes, when every one is known (deleting several
    /// snapshots frees at least this: what only they share goes too).
    pub frees_at_least_bytes: Option<u64>,
}

/// What the PCs connect to and whether this NAS can receive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReceiveSetup {
    pub account: String,
    pub filter: String,
    /// The NAS name for the PC's configuration.
    pub host: String,
    /// sshd's port when it is not 22.
    pub ssh_port: Option<u16>,
    /// btrfs mount points a target folder can go on (`/<volume>/backup/<device>`).
    pub volumes: Vec<String>,
    /// Problems that stop PCs from sending (codes, with a subject).
    pub problems: Vec<SetupProblem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupProblemCode {
    /// No sshd installed or running.
    SshdMissing,
    /// `AllowUsers` / `AllowGroups` in sshd_config leave the account out.
    SshdAllowList,
    /// sudo is not installed (the account runs btrfs through it).
    SudoMissing,
    /// btrfs-progs is not installed.
    BtrfsMissing,
    /// The filter binary is not where the keys point.
    FilterMissing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupProblem {
    pub code: SetupProblemCode,
    #[serde(default)]
    pub subject: Option<String>,
}

/// `POST /api/v1/pc-backups/targets` (202, task `pcbackup.create`): a new folder, or
/// (`adopt`) an existing folder of received snapshots, left as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcTargetCreate {
    pub device: String,
    #[serde(default)]
    pub description: String,
    pub path: String,
    #[serde(default)]
    pub adopt: bool,
    #[serde(default)]
    pub public_key: String,
    #[serde(default = "default_alert")]
    pub alert_days: u32,
    #[serde(default)]
    pub delete: DeletePolicy,
    /// Wins over `alert_days` when not 0.
    #[serde(default)]
    pub alert_hours: u32,
    #[serde(default)]
    pub retention: PcRetention,
}

pub fn default_alert() -> u32 {
    3
}

/// `PUT /api/v1/pc-backups/targets/{device}` (202, task `pcbackup.update`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcTargetUpdate {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub public_key: String,
    pub alert_days: u32,
    pub delete: DeletePolicy,
    #[serde(default)]
    pub alert_hours: u32,
    /// Missing: unchanged.
    #[serde(default)]
    pub retention: Option<PcRetention>,
    /// Missing: unchanged.
    #[serde(default)]
    pub ignored_groups: Option<Vec<String>>,
}

/// `POST /api/v1/pc-backups/plan/adopt`: what a folder holds, read only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdoptRequest {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdoptPreview {
    pub path: String,
    /// The folder is a btrfs subvolume (a plain folder works too, but cannot be snapshotted
    /// or measured on its own).
    pub subvolume: bool,
    pub list: Vec<ReceivedSnapshot>,
    pub groups: Vec<ReceivedGroup>,
    /// Entries that are not btrbk snapshots (left alone).
    pub other_entries: u32,
    /// A device name taken from the folder's name, when it fits.
    pub suggested_device: Option<String>,
    /// What adopting changes on the folder itself (owner, group, mode), when it is not
    /// already `root:rynas-pcbackup 0750`. The create call then needs
    /// `?confirm_permission_change=true`.
    #[serde(default)]
    pub permission_change: Option<PermissionChange>,
}

/// The folder's owner, group and mode now and after adopting (only the folder itself; its
/// content is never touched).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PermissionChange {
    pub uid: u32,
    pub gid: u32,
    /// Permission bits (`0o7777`).
    pub mode: u32,
    /// User and group names when known.
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    /// After: always `root`, [`ACCOUNT`], `0o750`.
    pub new_owner: String,
    pub new_group: String,
    pub new_mode: u32,
}

/// `POST /api/v1/pc-backups/targets/{device}/copy-out` (202, task `pcbackup.copy_out`): a
/// file or folder of a snapshot copied into a folder on the NAS (nothing is overwritten).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CopyOutRequest {
    pub snapshot: String,
    /// Relative path inside the snapshot.
    pub path: String,
    /// An existing folder on the NAS.
    pub destination: String,
}
