//! Removable drives (`/api/v1/removable/...`): USB disks and sticks, USB card readers, SD
//! cards in a built-in reader, optical drives, anything the kernel flags as removable.
//!
//! - [`RemovableState`]: what rynasd sees (drives with their volumes, mounts, usage, link
//!   speed, actions in progress), pushed on `/api/v1/events` as `removable.state` whenever
//!   it changes.
//! - Actions are immediate (not staged): mount, unmount, unlock, lock, safe removal, each a
//!   rynasd task `removable.<verb>`.
//! - [`RemovableSettings`]: the configuration key `removable` (staged and applied like every
//!   key): auto-mount, safe-removal notifications, read-only mounts, network sharing.
//!
//! Design and research: `docs/research/removable-drives.md`.

use serde::{Deserialize, Serialize};

/// How a drive is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DriveBus {
    /// USB disk, stick or card reader.
    Usb,
    /// SD card in a built-in reader (`mmcblk` with device type `SD`; eMMC is left out).
    Sd,
    /// Optical drive (`sr*`).
    Optical,
    /// Any other drive the kernel flags as removable.
    #[default]
    Other,
}

/// What "remove safely" does for a drive, after unmounting its volumes and locking its
/// encrypted ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RemoveMethod {
    /// The USB device is powered off (the drive spins down, the port goes dark): it can be
    /// unplugged. USB drives and USB card readers.
    PowerOff,
    /// The medium is ejected (optical drives, other drives with removable media).
    Eject,
    /// Nothing more: the card can be pulled (SD card in a built-in reader).
    #[default]
    Release,
}

/// An action rynasd is carrying out on a drive or a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovableAction {
    Mount,
    Unmount,
    Unlock,
    Lock,
    Remove,
}

impl RemovableAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mount => "mount",
            Self::Unmount => "unmount",
            Self::Unlock => "unlock",
            Self::Lock => "lock",
            Self::Remove => "remove",
        }
    }
}

/// The USB link of a drive, from the USB device in sysfs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UsbLink {
    /// Negotiated speed (`speed`): 1.5, 12, 480, 5000, 10000, 20000 Mbit/s (12 shown for 1.5).
    pub speed_mbps: u32,
    /// USB version the device declares (`version`, e.g. "3.20").
    pub version: String,
}

/// The cleartext side of an unlocked LUKS volume (the dm device).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ClearDevice {
    /// Kernel name (`dm-3`).
    pub name: String,
    pub dev_path: String,
    pub label: String,
    pub fs_type: String,
}

/// A filesystem or a LUKS container on a partition, or on the whole drive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RemovableVolume {
    /// Kernel name (`sdc1`, `mmcblk0p1`, `sr0`): the id used in the volume routes.
    pub name: String,
    pub dev_path: String,
    /// Filesystem label (for LUKS, the label of the cleartext filesystem when unlocked).
    pub label: String,
    /// `vfat`, `exfat`, `ntfs`, `ext4`, `btrfs`, `xfs`, `hfsplus`, `iso9660`, `udf`,
    /// `crypto_LUKS`...
    pub fs_type: String,
    pub uuid: Option<String>,
    pub size_bytes: u64,
    /// A LUKS container.
    pub luks: bool,
    /// A LUKS container that is open ([`Self::clear`]).
    pub unlocked: bool,
    pub clear: Option<ClearDevice>,
    /// rynas can mount this filesystem type (a driver or tool is installed).
    pub supported: bool,
    /// Where the volume (or its cleartext device) is mounted.
    pub mount_point: Option<String>,
    pub read_only: bool,
    /// Usage of the mounted filesystem (`statvfs`).
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub avail_bytes: Option<u64>,
    /// SMB share name when the volume is offered on the network.
    pub share: Option<String>,
    /// Action in progress on this volume.
    pub pending: Option<RemovableAction>,
}

impl RemovableVolume {
    /// The name shown for the volume: its label, the cleartext label, else `None`.
    pub fn display_label(&self) -> Option<&str> {
        if !self.label.is_empty() {
            return Some(&self.label);
        }
        self.clear
            .as_ref()
            .map(|c| c.label.as_str())
            .filter(|l| !l.is_empty())
    }

    /// The filesystem that is (or would be) mounted: the cleartext one of a LUKS volume.
    pub fn mounted_fs(&self) -> &str {
        match &self.clear {
            Some(c) if self.luks && !c.fs_type.is_empty() => &c.fs_type,
            _ => &self.fs_type,
        }
    }
}

/// One removable drive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RemovableDrive {
    /// Kernel name (`sdc`, `mmcblk0`, `sr0`): the id used in the drive routes.
    pub name: String,
    pub dev_path: String,
    pub bus: DriveBus,
    /// Vendor and model, or the device path when the drive has no name.
    pub title: String,
    pub vendor: String,
    pub model: String,
    pub serial: String,
    /// 0 for a reader or optical drive without a medium.
    pub size_bytes: u64,
    /// The kernel flags the medium as removable (card readers, optical drives).
    pub removable_media: bool,
    /// A reader or optical drive without a medium.
    pub no_media: bool,
    /// Write-protected (the switch of an SD card, a read-only medium).
    pub read_only: bool,
    pub usb: Option<UsbLink>,
    pub remove_method: RemoveMethod,
    /// Requests in flight or sectors written since the last reading: being written to.
    pub busy: bool,
    /// Action in progress on the whole drive (safe removal).
    pub pending: Option<RemovableAction>,
    pub volumes: Vec<RemovableVolume>,
}

impl RemovableDrive {
    pub fn mounted(&self) -> usize {
        self.volumes
            .iter()
            .filter(|v| v.mount_point.is_some())
            .count()
    }
}

/// `GET /api/v1/removable` and the `removable.state` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RemovableState {
    pub drives: Vec<RemovableDrive>,
    /// Where rynas mounts removable volumes (`/media`).
    pub mount_root: String,
    pub read_at_ms: u64,
}

impl RemovableState {
    pub fn mounted(&self) -> usize {
        self.drives.iter().map(RemovableDrive::mounted).sum()
    }

    /// Something is being written or an action is in progress.
    pub fn busy(&self) -> bool {
        self.drives
            .iter()
            .any(|d| d.busy || d.pending.is_some() || d.volumes.iter().any(|v| v.pending.is_some()))
    }
}

/// A mounted removable volume as the file manager lists it next to the shared folders
/// (`GET /api/v1/removable/mounts`; also `rynas_removable::mounts`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RemovableMount {
    /// Always `true`: the entry is a removable volume.
    pub removable: bool,
    /// The volume's kernel name (`sdc1`): `POST /api/v1/removable/volumes/{device}/unmount`.
    pub device: String,
    /// The drive's kernel name (`sdc`): `POST /api/v1/removable/drives/{drive}/remove`.
    pub drive: String,
    /// Drive title (vendor and model).
    pub device_name: String,
    /// Volume label, empty when it has none.
    pub label: String,
    pub fs_type: String,
    pub mount_point: String,
    pub read_only: bool,
    pub remove_method: RemoveMethod,
}

/// `POST /api/v1/removable/volumes/{name}/mount`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MountRequest {
    /// Mount read-only even when the settings do not ask for it.
    #[serde(default)]
    pub read_only: bool,
}

/// `POST /api/v1/removable/volumes/{name}/unlock`. The passphrase travels on cryptsetup's
/// standard input only: never in argv, a log or the configuration.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UnlockVolumeRequest {
    pub passphrase: String,
    /// Mount the cleartext filesystem after unlocking it (default true).
    #[serde(default = "yes")]
    pub mount: bool,
}

fn yes() -> bool {
    true
}

impl std::fmt::Debug for UnlockVolumeRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnlockVolumeRequest")
            .field("passphrase", &"<redacted>")
            .field("mount", &self.mount)
            .finish()
    }
}

/// A process that keeps a mounted volume busy (open file, working directory, mapped file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VolumeHolder {
    pub pid: u32,
    /// `comm` of the process (`smbd`, `bash`).
    pub command: String,
    /// User name (or uid) of the process owner.
    pub user: String,
}

/// What the members of the access group may do on a shared removable volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShareAccess {
    ReadOnly,
    #[default]
    ReadWrite,
}

/// Network sharing of mounted removable volumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovableShareSettings {
    /// Offer every mounted removable volume as an SMB share (`usb-<name>`).
    pub enabled: bool,
    pub access: ShareAccess,
    /// Group whose members may use the shares and the files of non-POSIX filesystems (FAT,
    /// exFAT, NTFS are mounted with this group).
    pub group: String,
}

impl Default for RemovableShareSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            access: ShareAccess::ReadWrite,
            group: DEFAULT_GROUP.into(),
        }
    }
}

/// Administrators (created by the package): removable volumes are theirs unless the
/// settings name another group (`users` = every account rynas creates).
pub const DEFAULT_GROUP: &str = "rynas-admin";

/// The configuration key `removable`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovableSettings {
    /// Mount the filesystems of a drive when it is plugged in (default on). Encrypted volumes
    /// still wait for their passphrase.
    pub auto_mount: bool,
    /// Mount read-only (auto-mount and the Mount button).
    pub read_only: bool,
    /// Notify when a drive can be removed safely. Unplugging while mounted is always
    /// notified.
    pub notify_safe_removal: bool,
    pub share: RemovableShareSettings,
}

impl Default for RemovableSettings {
    fn default() -> Self {
        Self {
            auto_mount: true,
            read_only: false,
            notify_safe_removal: true,
            share: RemovableShareSettings::default(),
        }
    }
}

/// `GET/PUT /api/v1/removable/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RemovableSettingsState {
    pub applied: RemovableSettings,
    pub pending: Option<RemovableSettings>,
    /// Samba is installed (sharing can work).
    pub smb_available: bool,
}

/// Marketing name of a USB link speed: "USB 3 · 5 Gbps".
pub fn usb_speed_name(speed_mbps: u32) -> Option<String> {
    Some(match speed_mbps {
        0 => return None,
        s if s >= 20_000 => "USB 3.2 · 20 Gbps".into(),
        s if s >= 10_000 => "USB 3.2 · 10 Gbps".into(),
        s if s >= 5_000 => "USB 3 · 5 Gbps".into(),
        s if s >= 480 => "USB 2 · 480 Mbps".into(),
        s => format!("USB 1 · {s} Mbps"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speeds() {
        assert_eq!(usb_speed_name(0), None);
        assert_eq!(usb_speed_name(5000).as_deref(), Some("USB 3 · 5 Gbps"));
        assert_eq!(usb_speed_name(480).as_deref(), Some("USB 2 · 480 Mbps"));
        assert_eq!(usb_speed_name(12).as_deref(), Some("USB 1 · 12 Mbps"));
        assert_eq!(usb_speed_name(10000).as_deref(), Some("USB 3.2 · 10 Gbps"));
    }

    #[test]
    fn passphrase_is_not_debug_printed() {
        let r = UnlockVolumeRequest {
            passphrase: "secret".into(),
            mount: true,
        };
        assert!(!format!("{r:?}").contains("secret"));
        let r: UnlockVolumeRequest = serde_json::from_str(r#"{"passphrase":"x"}"#).unwrap();
        assert!(r.mount);
    }

    #[test]
    fn labels() {
        let mut v = RemovableVolume {
            fs_type: "crypto_LUKS".into(),
            luks: true,
            ..Default::default()
        };
        assert_eq!(v.display_label(), None);
        v.clear = Some(ClearDevice {
            label: "Secret".into(),
            fs_type: "ext4".into(),
            ..Default::default()
        });
        assert_eq!(v.display_label(), Some("Secret"));
        assert_eq!(v.mounted_fs(), "ext4");
    }
}
