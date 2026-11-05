//! What the shares domain needs from the filesystem besides writing configuration files:
//! path checks (exists, local filesystem, btrfs subvolume and its UUID) and POSIX ACLs.
//!
//! A share points at an existing directory, or asks for its subvolume
//! ([`rynas_api_types::Share::subvolume`]): the [`SubvolumeCreator`] given by rynasd (on top
//! of `rynas-storage`) creates it when the configuration is applied.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;
use std::time::Duration;

use rynas_tasks::Cmd;

use crate::perm::{AclPlan, parse_getfacl};

/// Facts about a share path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PathInfo {
    pub exists: bool,
    pub is_dir: bool,
    /// `btrfs`, `ext4`, `xfs`, ... or `0x<magic>`.
    pub fs_type: String,
    /// Network or FUSE filesystem: not allowed for shares.
    pub remote: bool,
    /// Root of a btrfs subvolume.
    pub subvolume: bool,
    /// UUID of that subvolume (`btrfs subvolume show`).
    pub subvolume_uuid: Option<String>,
}

pub trait ShareHost: Send + Sync {
    fn probe(&self, path: &str) -> PathInfo;
    /// Replaces the access and default ACL of `path`.
    fn set_acl(&self, path: &str, plan: &AclPlan) -> Result<(), String>;
    /// Current ACL in the form of [`AclPlan::canonical`]; `None` if it cannot be read.
    fn get_acl(&self, path: &str) -> Result<Option<BTreeSet<String>>, String>;
    /// The systemd unit is installed (its package is).
    fn unit_installed(&self, unit: &str) -> bool;
}

/// Creates the btrfs subvolume of a share that asks for one, during the apply of `shares`
/// (a task). Implemented by rynasd with the storage write side.
pub trait SubvolumeCreator: Send + Sync {
    /// Mount point of a mounted rynas volume (by label).
    fn volume_path(&self, volume: &str) -> Result<String, String>;
    /// Creates `path` (directly below a volume's mount point) as a subvolume; with
    /// `compression` false its `compression` property is set to "none".
    fn create(&self, path: &str, compression: bool, log: &dyn Fn(&str)) -> Result<(), String>;
}

/// Name of a filesystem magic number (`statfs.f_type`) and whether it is remote/FUSE.
pub fn fs_name(magic: i64) -> (String, bool) {
    let (n, remote) = match magic as u32 {
        0x9123_683E => ("btrfs", false),
        0xEF53 => ("ext4", false),
        0x5846_5342 => ("xfs", false),
        0x2FC1_2FC1 => ("zfs", false),
        0xF2F5_2010 => ("f2fs", false),
        0x0102_1994 => ("tmpfs", false),
        0x794C_7630 => ("overlay", false),
        0x7366_746E => ("ntfs3", false),
        0x4D44 => ("vfat", false),
        0x2011_BAB0 => ("exfat", false),
        0x6969 => ("nfs", true),
        0xFF53_4D42 => ("cifs", true),
        0xFE53_4D42 => ("smb2", true),
        0x517B => ("smb", true),
        0x6573_5546 => ("fuse", true),
        0x0102_1997 => ("9p", true),
        0x00C3_6400 => ("ceph", true),
        0x5346_414F => ("afs", true),
        0x7375_7245 => ("coda", true),
        _ => return (format!("0x{magic:x}"), false),
    };
    (n.to_string(), remote)
}

/// The real filesystem.
pub struct HostShares;

const CMD_TIMEOUT: Duration = Duration::from_secs(60);

/// `struct btrfs_ioctl_timespec`.
#[repr(C)]
struct BtrfsTimespec {
    sec: u64,
    nsec: u32,
}

/// `struct btrfs_ioctl_get_subvol_info_args` (linux/btrfs.h).
#[repr(C)]
struct SubvolInfo {
    treeid: u64,
    name: [u8; 256],
    parent_id: u64,
    dirid: u64,
    generation: u64,
    flags: u64,
    uuid: [u8; 16],
    parent_uuid: [u8; 16],
    received_uuid: [u8; 16],
    ctransid: u64,
    otransid: u64,
    stransid: u64,
    rtransid: u64,
    ctime: BtrfsTimespec,
    otime: BtrfsTimespec,
    stime: BtrfsTimespec,
    rtime: BtrfsTimespec,
    reserved: [u64; 8],
}

// BTRFS_IOC_GET_SUBVOL_INFO: unprivileged (unlike the tree search `btrfs subvolume show`
// falls back to), so it works inside the hardened rynasd unit
nix::ioctl_read!(btrfs_get_subvol_info, 0x94, 60, SubvolInfo);

/// UUID of the subvolume whose root directory is `path`.
fn subvolume_uuid(path: &str) -> Option<String> {
    use std::os::fd::AsRawFd;
    let dir = std::fs::File::open(path).ok()?;
    // SAFETY: all-zero is a valid value of this plain-data struct.
    let mut info: SubvolInfo = unsafe { std::mem::zeroed() };
    // SAFETY: the fd is open for the duration of the call and `info` has the layout the
    // kernel writes (checked by a test against the size in linux/btrfs.h).
    unsafe { btrfs_get_subvol_info(dir.as_raw_fd(), &mut info) }.ok()?;
    if info.uuid == [0; 16] {
        return None;
    }
    let h: String = info.uuid.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    ))
}

impl ShareHost for HostShares {
    fn probe(&self, path: &str) -> PathInfo {
        let Ok(md) = std::fs::metadata(path) else {
            return PathInfo::default();
        };
        let mut info = PathInfo {
            exists: true,
            is_dir: md.is_dir(),
            ..Default::default()
        };
        if let Ok(st) = nix::sys::statfs::statfs(path) {
            // f_type is a C long whose width depends on the architecture
            #[allow(clippy::useless_conversion)]
            let (name, remote) = fs_name(i64::from(st.filesystem_type().0));
            info.fs_type = name;
            info.remote = remote;
        }
        // BTRFS_FIRST_FREE_OBJECTID: the root directory of every subvolume
        if info.fs_type == "btrfs" && md.ino() == 256 {
            info.subvolume = true;
            info.subvolume_uuid = subvolume_uuid(path).filter(|u| crate::validate::uuid(u));
        }
        info
    }

    fn set_acl(&self, path: &str, plan: &AclPlan) -> Result<(), String> {
        let spec = plan.spec();
        for args in [vec!["--set", &spec, path], vec!["-d", "--set", &spec, path]] {
            Cmd::new("setfacl")
                .args(args)
                .timeout(CMD_TIMEOUT)
                .run_blocking()
                .map_err(|e| format!("setfacl {path}: {e}"))?;
        }
        Ok(())
    }

    fn get_acl(&self, path: &str) -> Result<Option<BTreeSet<String>>, String> {
        match Cmd::new("getfacl")
            .args(["-c", "-p", "-E", path])
            .timeout(CMD_TIMEOUT)
            .run_blocking()
        {
            Ok(o) => Ok(Some(parse_getfacl(&o.stdout))),
            Err(rynas_tasks::CmdError::Spawn(_)) => Ok(None),
            Err(e) => Err(format!("getfacl {path}: {e}")),
        }
    }

    fn unit_installed(&self, unit: &str) -> bool {
        [
            "/usr/lib/systemd/system",
            "/lib/systemd/system",
            "/etc/systemd/system",
        ]
        .iter()
        .any(|d| std::path::Path::new(d).join(unit).exists())
    }
}

/// Paths and ACLs in memory (tests); with `probe_host`, paths are probed on the real
/// filesystem (read-only) and only the ACL writes stay in memory (`--dry-system`).
#[derive(Default)]
pub struct MemoryShares {
    pub probe_host: bool,
    state: Mutex<MemorySharesState>,
}

#[derive(Debug, Default, Clone)]
pub struct MemorySharesState {
    pub paths: BTreeMap<String, PathInfo>,
    /// ACL per path, as [`AclPlan::canonical`].
    pub acls: BTreeMap<String, BTreeSet<String>>,
    /// Every `set_acl` call: `path spec`.
    pub log: Vec<String>,
    /// `set_acl` fails for these paths.
    pub fail_acl: Vec<String>,
    /// Units reported as not installed (every other one is).
    pub missing_units: Vec<String>,
}

impl MemoryShares {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn probing_host() -> Self {
        Self {
            probe_host: true,
            ..Default::default()
        }
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, MemorySharesState> {
        self.state.lock().unwrap()
    }

    /// Declares a directory (`btrfs` subvolume when `uuid` is given, else `ext4`).
    pub fn add_dir(&self, path: &str, uuid: Option<&str>) {
        self.state().paths.insert(
            path.into(),
            PathInfo {
                exists: true,
                is_dir: true,
                fs_type: if uuid.is_some() { "btrfs" } else { "ext4" }.into(),
                remote: false,
                subvolume: uuid.is_some(),
                subvolume_uuid: uuid.map(String::from),
            },
        );
    }
}

impl ShareHost for MemoryShares {
    fn probe(&self, path: &str) -> PathInfo {
        if let Some(i) = self.state().paths.get(path) {
            return i.clone();
        }
        if self.probe_host {
            HostShares.probe(path)
        } else {
            PathInfo::default()
        }
    }

    fn set_acl(&self, path: &str, plan: &AclPlan) -> Result<(), String> {
        let mut s = self.state();
        s.log.push(format!("{path} {}", plan.spec()));
        tracing::info!("dry system: setfacl {path} {}", plan.spec());
        if s.fail_acl.iter().any(|p| p == path) {
            return Err(format!("setfacl {path}: Operation not supported"));
        }
        s.acls.insert(path.into(), plan.canonical());
        Ok(())
    }

    fn get_acl(&self, path: &str) -> Result<Option<BTreeSet<String>>, String> {
        Ok(self.state().acls.get(path).cloned())
    }

    fn unit_installed(&self, unit: &str) -> bool {
        !self.state().missing_units.iter().any(|u| u == unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subvolume_uuid_when_the_root_is_a_btrfs_subvolume() {
        let i = HostShares.probe("/");
        if i.fs_type != "btrfs" || !i.subvolume {
            return; // nothing to check on this machine
        }
        let u = subvolume_uuid("/");
        assert!(
            u.as_deref().is_some_and(crate::validate::uuid),
            "{u:?} {i:?}"
        );
    }

    #[test]
    fn subvol_info_layout() {
        // sizeof(struct btrfs_ioctl_get_subvol_info_args) on x86-64 and aarch64
        assert_eq!(std::mem::size_of::<SubvolInfo>(), 504);
    }

    #[test]
    fn filesystem_names() {
        assert_eq!(fs_name(0x9123683E), ("btrfs".into(), false));
        assert_eq!(fs_name(0x6969), ("nfs".into(), true));
        assert_eq!(fs_name(0x1234).0, "0x1234");
    }

    #[test]
    fn probing_a_real_directory() {
        let d = tempfile::tempdir().unwrap();
        let i = HostShares.probe(d.path().to_str().unwrap());
        assert!(i.exists && i.is_dir && !i.remote, "{i:?}");
        assert!(!i.fs_type.is_empty());
        assert!(!HostShares.probe("/nonexistent/rynas").exists);
    }
}
