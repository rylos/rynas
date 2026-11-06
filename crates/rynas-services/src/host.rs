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
    /// `path` itself is a symbolic link (the other fields describe its target).
    pub symlink: bool,
}

pub trait ShareHost: Send + Sync {
    fn probe(&self, path: &str) -> PathInfo;
    /// Replaces the access and default ACL of `path`.
    fn set_acl(&self, path: &str, plan: &AclPlan) -> Result<(), String>;
    /// Current ACL in the form of [`AclPlan::canonical`]; `None` if it cannot be read.
    fn get_acl(&self, path: &str) -> Result<Option<BTreeSet<String>>, String>;
    /// The systemd unit is installed (its package is).
    fn unit_installed(&self, unit: &str) -> bool;
    /// Entries of a directory (empty when it cannot be read).
    fn list_dir(&self, path: &str) -> Vec<DirEntryInfo>;
    /// Space used by the files below `path` on its own filesystem (nested subvolumes and
    /// mounts are skipped), counting for at most `budget`: `(bytes, partial)`.
    fn tree_size(&self, path: &str, budget: Duration) -> Option<(u64, bool)>;
    /// Bytes referenced by the btrfs subvolume at `path` according to its quota group
    /// (`btrfs qgroup show -f --raw`), when quotas are enabled on its filesystem: read from
    /// the quota tree, no walk of the files. `None` otherwise.
    fn qgroup_size(&self, _path: &str) -> Option<u64> {
        None
    }
    /// Snapshot names `<prefix>.<timestamp>[_N]` in `dir`.
    fn snapshot_names(&self, dir: &str, prefix: &str) -> Vec<String>;
    /// `mkdir -p` with mode 0755.
    fn ensure_dir(&self, path: &str) -> Result<(), String>;
    /// Removes `path` if it is an empty directory (nothing happens otherwise).
    fn remove_empty_dir(&self, path: &str);
}

/// An entry of [`ShareHost::list_dir`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DirEntryInfo {
    pub name: String,
    pub is_dir: bool,
    /// Root of a btrfs subvolume.
    pub subvolume: bool,
}

/// `<prefix>.` followed by a btrbk timestamp (`YYYYMMDD[Thhmm[ss]][±hhmm]`) and an optional
/// `_N`.
pub fn is_snapshot_name(name: &str, prefix: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(prefix)
        .and_then(|r| r.strip_prefix('.'))
    else {
        return false;
    };
    let stamp = rest.split('_').next().unwrap_or("");
    let suffix_ok = rest[stamp.len()..]
        .strip_prefix('_')
        .is_none_or(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let b = stamp.as_bytes();
    b.len() >= 8
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8..]
            .iter()
            .all(|c| c.is_ascii_digit() || matches!(c, b'T' | b'+' | b'-'))
        && suffix_ok
}

/// Creates the btrfs subvolume of a share that asks for one, during the apply of `shares`
/// (a task). Implemented by rynasd with the storage write side.
pub trait SubvolumeCreator: Send + Sync {
    /// Mount point of a mounted rynas volume (by label).
    fn volume_path(&self, volume: &str) -> Result<String, String>;
    /// Creates `path` (directly below a volume's mount point) as a subvolume; with
    /// `compression` false its `compression` property is set to "none".
    fn create(&self, path: &str, compression: bool, log: &dyn Fn(&str)) -> Result<(), String>;
    /// The mounted rynas volumes: (label, mount point).
    fn volumes(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// A disk under the volume mounted at `mount` is in standby or asleep: nothing may read
    /// its files now (that would spin it up). Checked without touching the disks.
    fn asleep(&self, _mount: &str) -> bool {
        false
    }
}

/// Referenced bytes of the subvolume's own quota group (`0/<id>`) in the output of
/// `btrfs qgroup show -f --raw <path>`.
pub fn parse_qgroup_referenced(out: &str) -> Option<u64> {
    out.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let id = it.next()?;
        let (level, rest) = id.split_once('/')?;
        if level != "0" || rest.parse::<u64>().is_err() {
            return None;
        }
        it.next()?.parse().ok()
    })
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
            symlink: std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()),
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

    fn list_dir(&self, path: &str) -> Vec<DirEntryInfo> {
        let Ok(rd) = std::fs::read_dir(path) else {
            return Vec::new();
        };
        let Ok(parent_dev) = std::fs::metadata(path).map(|m| m.dev()) else {
            return Vec::new();
        };
        let mut v: Vec<DirEntryInfo> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                // no symlinks followed
                let md = std::fs::symlink_metadata(e.path()).ok()?;
                let is_dir = md.is_dir();
                Some(DirEntryInfo {
                    name,
                    is_dir,
                    // a subvolume root: inode 256 with its own anonymous device
                    subvolume: is_dir && md.ino() == 256 && md.dev() != parent_dev,
                })
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    fn tree_size(&self, path: &str, budget: Duration) -> Option<(u64, bool)> {
        tree_size(std::path::Path::new(path), budget)
    }

    fn qgroup_size(&self, path: &str) -> Option<u64> {
        // fails at once ("quotas not enabled") when the filesystem has no quotas
        let o = Cmd::new("btrfs")
            .args(["qgroup", "show", "-f", "--raw", path])
            .env_clear()
            .timeout(CMD_TIMEOUT)
            .run_blocking()
            .ok()?;
        parse_qgroup_referenced(&o.stdout)
    }

    fn snapshot_names(&self, dir: &str, prefix: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| is_snapshot_name(n, prefix))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    fn ensure_dir(&self, path: &str) -> Result<(), String> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(path)
            .map_err(|e| format!("{path}: {e}"))
    }

    fn remove_empty_dir(&self, path: &str) {
        // rmdir refuses a directory that is not empty (or still a mount point)
        let _ = std::fs::remove_dir(path);
    }
}

/// [`ShareHost::tree_size`] on the real filesystem: `st_blocks` of every entry on the
/// device of `root`, without following symlinks, until `budget` runs out.
pub fn tree_size(root: &std::path::Path, budget: Duration) -> Option<(u64, bool)> {
    let start = std::time::Instant::now();
    let md = std::fs::symlink_metadata(root).ok()?;
    if !md.is_dir() {
        return Some((md.blocks() * 512, false));
    }
    let dev = md.dev();
    let mut total = md.blocks() * 512;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            if start.elapsed() > budget {
                return Some((total, true));
            }
            let Ok(m) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if m.dev() != dev {
                continue; // nested subvolume or mount (the snapshot folder)
            }
            total += m.blocks() * 512;
            if m.is_dir() {
                stack.push(e.path());
            }
        }
    }
    Some((total, false))
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
    /// Snapshot names per directory.
    pub snapshots: BTreeMap<String, Vec<String>>,
    /// Sizes reported by `tree_size`.
    pub sizes: BTreeMap<String, u64>,
    /// Sizes reported by `qgroup_size` (quotas enabled).
    pub qgroups: BTreeMap<String, u64>,
    /// Every `tree_size` and `qgroup_size` call: `walk <path>`, `qgroup <path>`.
    pub size_log: Vec<String>,
    /// `ensure_dir` and `remove_empty_dir` calls: `mkdir <path>`, `rmdir <path>`.
    pub dir_log: Vec<String>,
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
                symlink: false,
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

    fn list_dir(&self, path: &str) -> Vec<DirEntryInfo> {
        let base = format!("{}/", path.trim_end_matches('/'));
        let v: Vec<DirEntryInfo> = self
            .state()
            .paths
            .iter()
            .filter_map(|(p, i)| {
                let name = p.strip_prefix(&base)?;
                (!name.is_empty() && !name.contains('/') && i.exists).then(|| DirEntryInfo {
                    name: name.to_string(),
                    is_dir: i.is_dir,
                    subvolume: i.subvolume,
                })
            })
            .collect();
        if v.is_empty() && self.probe_host {
            return HostShares.list_dir(path);
        }
        v
    }

    fn tree_size(&self, path: &str, budget: Duration) -> Option<(u64, bool)> {
        self.state().size_log.push(format!("walk {path}"));
        let known = self.state().sizes.get(path).copied();
        if let Some(n) = known {
            return Some((n, false));
        }
        if self.probe_host {
            HostShares.tree_size(path, budget)
        } else {
            self.state().paths.contains_key(path).then_some((0, false))
        }
    }

    fn qgroup_size(&self, path: &str) -> Option<u64> {
        let mut s = self.state();
        s.size_log.push(format!("qgroup {path}"));
        s.qgroups.get(path).copied()
    }

    fn snapshot_names(&self, dir: &str, prefix: &str) -> Vec<String> {
        let known = self.state().snapshots.get(dir.trim_end_matches('/')).cloned();
        match known {
            Some(v) => v
                .into_iter()
                .filter(|n| is_snapshot_name(n, prefix))
                .collect(),
            None if self.probe_host => HostShares.snapshot_names(dir, prefix),
            None => Vec::new(),
        }
    }

    fn ensure_dir(&self, path: &str) -> Result<(), String> {
        tracing::info!("dry system: mkdir -p {path}");
        self.state().dir_log.push(format!("mkdir {path}"));
        Ok(())
    }

    fn remove_empty_dir(&self, path: &str) {
        tracing::info!("dry system: rmdir {path}");
        self.state().dir_log.push(format!("rmdir {path}"));
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
    fn qgroup_referenced_of_the_subvolume() {
        let out = "Qgroupid    Referenced    Exclusive   Path \n\
                   --------    ----------    ---------   ---- \n\
                   0/257       1234567890        16384   photos\n\
                   1/100       9999999999         4096   <under 1/100>\n";
        assert_eq!(parse_qgroup_referenced(out), Some(1_234_567_890));
        // btrfs-progs without the Path column
        assert_eq!(parse_qgroup_referenced("qgroupid rfer excl\n-------- ---- ----\n0/5 42 42\n"), Some(42));
        assert_eq!(parse_qgroup_referenced("ERROR: can't list qgroups: quotas not enabled\n"), None);
    }

    #[test]
    fn subvol_info_layout() {
        // sizeof(struct btrfs_ioctl_get_subvol_info_args) on x86-64 and aarch64
        assert_eq!(std::mem::size_of::<SubvolInfo>(), 504);
    }

    #[test]
    fn snapshot_names_of_a_prefix() {
        for ok in [
            "photos.20261105",
            "photos.20261105T1200",
            "photos.20261105T120000+0100",
            "photos.20261105T1200_2",
        ] {
            assert!(is_snapshot_name(ok, "photos"), "{ok}");
        }
        for bad in [
            "photos",
            "photos.",
            "photos2.20261105T1200",
            "photos.2026",
            "photos.20261105T1200_",
            "photos.20261105T1200_x",
            "other.20261105T1200",
        ] {
            assert!(!is_snapshot_name(bad, "photos"), "{bad}");
        }
    }

    #[test]
    fn tree_size_stays_on_one_filesystem_without_following_symlinks() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("a/b/f"), vec![1u8; 64 * 1024]).unwrap();
        std::os::unix::fs::symlink("/usr", d.path().join("link")).unwrap();
        let (n, partial) = tree_size(d.path(), Duration::from_secs(5)).unwrap();
        assert!(!partial);
        assert!(n >= 64 * 1024, "{n}");
        assert!(n < 1024 * 1024, "the symlink is not followed: {n}");
        assert!(tree_size(&d.path().join("missing"), Duration::from_secs(1)).is_none());
        let h = HostShares;
        let names: Vec<String> = h
            .list_dir(d.path().to_str().unwrap())
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["a", "link"]);
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
