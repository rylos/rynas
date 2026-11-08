//! PC backup, receive side (D-PCBACKUP phase 2): folders on the NAS where Linux PCs push their
//! btrbk snapshots over SSH (`target ssh://nas/<path>` in the PC's `btrbk.conf`).
//!
//! Every PC (a "device") gets a btrfs subvolume folder and one line in the `authorized_keys`
//! of the dedicated account [`ACCOUNT`]: its own public key, restricted to rynas' filter with
//! that folder only. See `docs/research/pc-backup.md` §10.

use serde::{Deserialize, Serialize};

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
