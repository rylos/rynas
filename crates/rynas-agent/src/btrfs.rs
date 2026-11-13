//! btrfs: read-only snapshots of every mounted subvolume and of the subvolumes nested in
//! them (abb-agent's `btrfs-image` rules: snapper/Timeshift snapshot folders and container
//! engines' subvolumes are not PC data), the default subvolume, cleanup of leftovers. Uses
//! the system's btrfs-progs.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The folder of the agent's own snapshots, inside each snapshotted subvolume.
pub const SNAP_DIR: &str = ".rynas-snap";

fn btrfs(args: &[&std::ffi::OsStr]) -> Result<String, String> {
    let o = Command::new("btrfs")
        .args(args)
        .output()
        .map_err(|e| format!("btrfs: {e} (install btrfs-progs)"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    } else {
        Err(format!("btrfs {}: {}", args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "), String::from_utf8_lossy(&o.stderr).trim()))
    }
}

/// Nested subvolumes never backed up (recreated empty at restore).
pub fn excluded_subvolume(rel: &str) -> bool {
    let last = rel.rsplit('/').next().unwrap_or(rel);
    last == ".snapshots"
        || last == SNAP_DIR
        // btrbk's local snapshots (the usual folder names and the agent's default)
        || rel.split('/').any(|c| c == "btrbk_snapshots" || c == ".btrbk_snapshots")
        || rel.contains("timeshift-btrfs")
        || rel.contains("var/lib/docker/btrfs")
        || rel.contains("var/lib/containers/storage")
        || rel.starts_with(SNAP_DIR)
        || rel.contains(&format!("/{SNAP_DIR}/"))
}

/// Subvolumes nested below the subvolume mounted at `mount`, as paths relative to it (with
/// their ids), from `btrfs subvolume list -o`.
pub fn nested(mount: &Path, max_depth: usize) -> Vec<(String, u64)> {
    let Ok(out) = btrfs(&["subvolume".as_ref(), "list".as_ref(), "-o".as_ref(), mount.as_os_str()]) else {
        return Vec::new();
    };
    let base = btrfs(&["subvolume".as_ref(), "show".as_ref(), mount.as_os_str()])
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .unwrap_or_default();
    let base = if base == "/" || base.starts_with('<') { String::new() } else { base.trim_start_matches('/').to_string() };
    let mut v = Vec::new();
    for l in out.lines() {
        // ID 258 gen 10 top level 256 path @/var/lib/machines
        let id = l.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let Some(path) = l.split(" path ").nth(1) else { continue };
        let path = path.trim().trim_start_matches("<FS_TREE>/");
        let rel = if base.is_empty() {
            path.to_string()
        } else {
            match path.strip_prefix(&format!("{base}/")) {
                Some(r) => r.to_string(),
                None => continue,
            }
        };
        if rel.split('/').count() <= max_depth {
            v.push((rel, id));
        }
    }
    v.sort();
    v
}

/// The default subvolume as a path from the top level (`None`: the top level itself).
pub fn default_subvol_path(mount: &Path) -> Option<String> {
    let out = btrfs(&["subvolume".as_ref(), "get-default".as_ref(), mount.as_os_str()]).ok()?;
    // ID 256 gen 10 top level 5 path @
    let p = out.split(" path ").nth(1)?.trim().to_string();
    (!p.is_empty() && out.split_whitespace().nth(1) != Some("5")).then_some(p)
}

/// A read-only snapshot of the subvolume at `src` into `<parent>/.rynas-snap/<name>`.
pub fn snapshot(src: &Path, snap_parent: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = snap_parent.join(SNAP_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let dst = dir.join(name);
    btrfs(&["subvolume".as_ref(), "snapshot".as_ref(), "-r".as_ref(), src.as_os_str(), dst.as_os_str()])?;
    Ok(dst)
}

pub fn delete(p: &Path) -> Result<(), String> {
    btrfs(&["subvolume".as_ref(), "delete".as_ref(), p.as_os_str()]).map(|_| ())
}

/// Removes snapshots an interrupted run left in `<mount>/.rynas-snap/` (only the agent's
/// own names: `run-…`).
pub fn cleanup_leftovers(mount: &Path) {
    let dir = mount.join(SNAP_DIR);
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        if e.file_name().to_string_lossy().starts_with("run-") {
            match delete(&e.path()) {
                Ok(()) => eprintln!("{}: removed (left by an interrupted backup)", e.path().display()),
                Err(err) => eprintln!("{err}"),
            }
        }
    }
    let _ = std::fs::remove_dir(&dir);
}

/// A read-only snapshot of the subvolume at `src` at exactly `dst`.
pub fn snapshot_ro(src: &Path, dst: &Path) -> Result<(), String> {
    btrfs(&["subvolume".as_ref(), "snapshot".as_ref(), "-r".as_ref(), src.as_os_str(), dst.as_os_str()]).map(|_| ())
}

/// What `BTRFS_IOC_GET_SUBVOL_INFO` says about a subvolume (unprivileged, kernel ≥ 4.18).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubvolInfo {
    pub uuid: Option<String>,
    pub received_uuid: Option<String>,
    pub readonly: bool,
    /// Creation time (seconds): the real order of snapshots, whatever their names say.
    pub otime: i64,
    /// Last transaction that changed it (0: nothing was ever written, e.g. a receive that
    /// stopped at once).
    pub generation: u64,
    pub parent_uuid: Option<String>,
}

#[repr(C)]
struct Timespec {
    sec: u64,
    nsec: u32,
}

/// `struct btrfs_ioctl_get_subvol_info_args`.
#[repr(C)]
struct SubvolInfoArgs {
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
    ctime: Timespec,
    otime: Timespec,
    stime: Timespec,
    rtime: Timespec,
    reserved: [u64; 8],
}

/// `_IOR(0x94, 60, struct btrfs_ioctl_get_subvol_info_args)`.
const IOC_GET_SUBVOL_INFO: u64 = (2 << 30) | ((std::mem::size_of::<SubvolInfoArgs>() as u64) << 16) | (0x94 << 8) | 60;

/// `/` is on btrfs (the btrfs method can serve this PC).
pub fn root_is_btrfs() -> bool {
    const BTRFS_MAGIC: i64 = 0x9123_683E;
    // SAFETY: statfs fills the struct it is given; the path is a static C string
    unsafe {
        let mut s: libc::statfs = std::mem::zeroed();
        libc::statfs(c"/".as_ptr(), &mut s) == 0 && s.f_type as i64 == BTRFS_MAGIC
    }
}

pub fn uuid_text(b: &[u8; 16]) -> Option<String> {
    if b.iter().all(|&x| x == 0) {
        return None;
    }
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Some(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

/// The facts of the subvolume whose top folder is `p` (not following a final symlink);
/// `None` when it is not a subvolume or the ioctl fails.
pub fn subvol_info(p: &Path) -> Option<SubvolInfo> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(p)
        .ok()?;
    // the top folder of every subvolume is inode 256
    if f.metadata().ok()?.ino() != 256 {
        return None;
    }
    // SAFETY: plain old data, zeroes are valid
    let mut a: Box<SubvolInfoArgs> = Box::new(unsafe { std::mem::zeroed() });
    // SAFETY: the kernel writes at most size_of::<SubvolInfoArgs>() bytes
    if unsafe { libc::ioctl(f.as_raw_fd(), IOC_GET_SUBVOL_INFO as _, &mut *a) } < 0 {
        return None;
    }
    Some(SubvolInfo {
        uuid: uuid_text(&a.uuid),
        received_uuid: uuid_text(&a.received_uuid),
        readonly: a.flags & 1 != 0,
        otime: a.otime.sec as i64,
        generation: a.generation,
        parent_uuid: uuid_text(&a.parent_uuid),
    })
}

/// `_IOW(0x94, 26, __u64)`: `BTRFS_IOC_SUBVOL_SETFLAGS`.
const IOC_SUBVOL_SETFLAGS: u64 = (1 << 30) | (8 << 16) | (0x94 << 8) | 26;
/// `BTRFS_SUBVOL_RDONLY`.
const SUBVOL_RDONLY: u64 = 1 << 1;

fn open_top(p: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(p)
        .map_err(|e| format!("{}: {e}", p.display()))
}

/// Sets or clears the read-only flag of the subvolume at `p` with the ioctl itself: unlike
/// `btrfs property set ro false`, the received UUID stays (a received snapshot keeps being
/// the parent of incremental sends; the NAS moves its own received snapshots the same way).
pub fn set_readonly(p: &Path, readonly: bool) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let f = open_top(p)?;
    let flags: u64 = if readonly { SUBVOL_RDONLY } else { 0 };
    // SAFETY: the kernel reads one u64
    if unsafe { libc::ioctl(f.as_raw_fd(), IOC_SUBVOL_SETFLAGS as _, &flags) } < 0 {
        return Err(format!("{}: {}", p.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Moves the read-only subvolume `from` to `to` (another folder of the same filesystem,
/// never replacing anything): a read-only subvolume cannot change parent folder, so the flag
/// is lifted for the moment of the move and set again wherever it is then.
pub fn move_readonly(from: &Path, to: &Path) -> Result<(), String> {
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string());
    let (f, t) = (c(from)?, c(to)?);
    set_readonly(from, false)?;
    // SAFETY: two valid C strings
    let rc = unsafe { libc::renameat2(libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), libc::RENAME_NOREPLACE) };
    let moved = if rc == 0 { Ok(()) } else { Err(format!("moving {} to {}: {}", from.display(), to.display(), std::io::Error::last_os_error())) };
    let at = if moved.is_ok() { to } else { from };
    let ro = set_readonly(at, true);
    moved.and(ro)
}

/// `FS_IOC_GETFLAGS` / `FS_IOC_SETFLAGS` (`_IOR/_IOW('f', 1/2, long)`) and `FS_NOCOW_FL`.
const FS_IOC_GETFLAGS: u64 = (2 << 30) | (8 << 16) | ((b'f' as u64) << 8) | 1;
const FS_IOC_SETFLAGS: u64 = (1 << 30) | (8 << 16) | ((b'f' as u64) << 8) | 2;
const FS_NOCOW_FL: libc::c_int = 0x0080_0000;

/// `chattr +C` on an empty folder or file (new files inside inherit it).
pub fn set_nocow(p: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(p)
        .map_err(|e| format!("{}: {e}", p.display()))?;
    let mut flags: libc::c_int = 0;
    // SAFETY: the kernel reads and writes one int
    unsafe {
        if libc::ioctl(f.as_raw_fd(), FS_IOC_GETFLAGS as _, &mut flags) < 0 {
            return Err(format!("{}: {}", p.display(), std::io::Error::last_os_error()));
        }
        flags |= FS_NOCOW_FL;
        if libc::ioctl(f.as_raw_fd(), FS_IOC_SETFLAGS as _, &flags) < 0 {
            return Err(format!("{}: {}", p.display(), std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// A writable snapshot of `src` at `dst` (`dst` must not exist).
pub fn snapshot_rw(src: &Path, dst: &Path) -> Result<(), String> {
    btrfs(&["subvolume".as_ref(), "snapshot".as_ref(), src.as_os_str(), dst.as_os_str()]).map(|_| ())
}

/// An empty subvolume at `p`.
pub fn create(p: &Path) -> Result<(), String> {
    btrfs(&["subvolume".as_ref(), "create".as_ref(), p.as_os_str()]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions() {
        assert!(excluded_subvolume(".snapshots"));
        assert!(excluded_subvolume("home/.snapshots"));
        assert!(excluded_subvolume("var/lib/docker/btrfs/subvolumes/abc"));
        assert!(excluded_subvolume(".rynas-snap/run-1"));
        assert!(excluded_subvolume(".btrbk_snapshots/@.20261112T0300"));
        assert!(!excluded_subvolume("var/lib/machines"));
        assert!(!excluded_subvolume("srv"));
    }

    #[test]
    fn subvol_info_layout() {
        // the kernel's struct is 504 bytes (`_IOR(0x94, 60, …)` = 0x81f8943c)
        assert_eq!(std::mem::size_of::<SubvolInfoArgs>(), 504);
        assert_eq!(IOC_GET_SUBVOL_INFO, 0x81f8_943c);
        assert_eq!(uuid_text(&[0; 16]), None);
        assert_eq!(uuid_text(&[0x11; 16]).unwrap(), "11111111-1111-1111-1111-111111111111");
        // a plain folder is no subvolume
        assert_eq!(subvol_info(&std::env::temp_dir()), None);
    }
}
