//! btrfs ioctls on open descriptors: subvolume facts (unprivileged `GET_SUBVOL_INFO`, kernel
//! ≥ 4.18), the filesystem UUID (`FS_INFO`), snapshot creation (`SNAP_CREATE_V2`), deletion
//! (`SNAP_DESTROY`, needs `CAP_SYS_ADMIN`) and reflink copies (`FICLONE`).
//!
//! Ported from btrbk-nas `btrfs.rs` (MIT, same author), which used `TREE_SEARCH` for the
//! DSM 4.4 kernel; Debian 13 has the unprivileged ioctl.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const IOC_MAGIC: u64 = 0x94;

const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | (IOC_MAGIC << 8) | nr
}

const IOC_SNAP_DESTROY: u64 = ioc(1, 15, 4096);
const IOC_SNAP_CREATE_V2: u64 = ioc(1, 23, 4096);
const IOC_FS_INFO: u64 = ioc(2, 31, 1024);
const IOC_GET_SUBVOL_INFO: u64 = ioc(2, 60, 504);
/// `FICLONE` = `_IOW(0x94, 9, int)`.
const IOC_FICLONE: u64 = ioc(1, 9, 4);

/// `BTRFS_SUBVOL_RDONLY` (snapshot creation flags).
const SUBVOL_RDONLY: u64 = 1 << 1;
/// `BTRFS_ROOT_SUBVOL_RDONLY` (root item flags, as `GET_SUBVOL_INFO` reports them).
const ROOT_SUBVOL_RDONLY: u64 = 1;
/// `statfs.f_type` of btrfs.
pub const BTRFS_MAGIC: i64 = 0x9123_683E;
/// Inode number of every subvolume's top directory.
pub const SUBVOL_TOP_INO: u64 = 256;

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

/// `struct btrfs_ioctl_vol_args_v2`.
#[repr(C)]
struct VolArgsV2 {
    fd: i64,
    transid: u64,
    flags: u64,
    unused: [u64; 4],
    name: [u8; 4040],
}

/// `struct btrfs_ioctl_vol_args`.
#[repr(C)]
struct VolArgs {
    fd: i64,
    name: [u8; 4088],
}

/// What the kernel tells about one subvolume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubvolInfo {
    pub root_id: u64,
    pub generation: u64,
    pub readonly: bool,
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub received_uuid: Option<String>,
    /// Transaction that created it.
    pub otransid: u64,
    /// Creation time (seconds).
    pub otime: i64,
}

fn uuid_text(b: &[u8; 16]) -> Option<String> {
    if b.iter().all(|&x| x == 0) {
        return None;
    }
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    ))
}

/// Opens a directory without following a final symlink.
pub fn open_dir(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// Opens `name` inside `dir` as a directory, refusing symlinks.
pub fn open_dir_at(dir: &File, name: &str) -> io::Result<File> {
    let c = CString::new(name).map_err(io::Error::other)?;
    // SAFETY: valid descriptor and C string
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor we own
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn statfs_type(f: &File) -> io::Result<i64> {
    // SAFETY: fstatfs writes into the struct
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(f.as_raw_fd(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(s.f_type as i64)
}

/// The descriptor is on btrfs.
pub fn is_btrfs(f: &File) -> bool {
    statfs_type(f).is_ok_and(|t| t == BTRFS_MAGIC)
}

/// The descriptor is the top directory of a btrfs subvolume.
pub fn is_subvolume(f: &File) -> bool {
    is_btrfs(f)
        && f.metadata()
            .is_ok_and(|m| m.is_dir() && m.ino() == SUBVOL_TOP_INO)
}

/// Facts about the subvolume holding `f` (unprivileged).
pub fn subvol_info(f: &File) -> io::Result<SubvolInfo> {
    // SAFETY: plain old data, zeroes are valid
    let mut a: Box<SubvolInfoArgs> = Box::new(unsafe { std::mem::zeroed() });
    // SAFETY: the kernel writes at most size_of::<SubvolInfoArgs>() bytes
    if unsafe { libc::ioctl(f.as_raw_fd(), IOC_GET_SUBVOL_INFO as _, &mut *a) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(SubvolInfo {
        root_id: a.treeid,
        generation: a.generation,
        readonly: a.flags & ROOT_SUBVOL_RDONLY != 0,
        uuid: uuid_text(&a.uuid),
        parent_uuid: uuid_text(&a.parent_uuid),
        received_uuid: uuid_text(&a.received_uuid),
        otransid: a.otransid,
        otime: a.otime.sec as i64,
    })
}

/// UUID of the btrfs filesystem holding `f` (the same for all its subvolumes, unlike
/// `st_dev` and `f_fsid`).
pub fn fs_uuid(f: &File) -> io::Result<String> {
    let mut buf = [0u8; 1024];
    // SAFETY: the kernel writes at most 1024 bytes (struct btrfs_ioctl_fs_info_args)
    if unsafe { libc::ioctl(f.as_raw_fd(), IOC_FS_INFO as _, buf.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut id = [0u8; 16];
    id.copy_from_slice(&buf[16..32]);
    uuid_text(&id).ok_or_else(|| io::Error::other("no filesystem UUID"))
}

fn check_name(name: &str, max: usize) -> io::Result<()> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." || name.len() >= max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid subvolume name",
        ));
    }
    Ok(())
}

/// Snapshot of the subvolume open as `source` created as `name` inside `dest_dir`.
pub fn create_snapshot(
    source: &File,
    dest_dir: &File,
    name: &str,
    readonly: bool,
) -> io::Result<()> {
    check_name(name, 4040)?;
    // SAFETY: plain old data
    let mut a: Box<VolArgsV2> = Box::new(unsafe { std::mem::zeroed() });
    a.fd = source.as_raw_fd() as i64;
    a.flags = if readonly { SUBVOL_RDONLY } else { 0 };
    a.name[..name.len()].copy_from_slice(name.as_bytes());
    // SAFETY: the kernel reads size_of::<VolArgsV2>() bytes
    if unsafe { libc::ioctl(dest_dir.as_raw_fd(), IOC_SNAP_CREATE_V2 as _, &*a) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Deletes the subvolume `name` inside `parent` (like `btrfs subvolume delete`; the space
/// is freed in the background). Needs `CAP_SYS_ADMIN`.
pub fn destroy_subvolume(parent: &File, name: &str) -> io::Result<()> {
    check_name(name, 4088)?;
    // SAFETY: plain old data
    let mut a: Box<VolArgs> = Box::new(unsafe { std::mem::zeroed() });
    a.name[..name.len()].copy_from_slice(name.as_bytes());
    // SAFETY: the kernel reads size_of::<VolArgs>() bytes
    if unsafe { libc::ioctl(parent.as_raw_fd(), IOC_SNAP_DESTROY as _, &*a) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reflink copy of all of `src` into `dst` (same btrfs filesystem): instant, shares extents.
pub fn clone_file(src: &File, dst: &File) -> io::Result<()> {
    // SAFETY: two valid descriptors
    if unsafe { libc::ioctl(dst.as_raw_fd(), IOC_FICLONE as _, src.as_raw_fd()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `renameat(dir, from, dir, to)` refusing to replace an existing `to` (`RENAME_NOREPLACE`).
pub fn rename_noreplace(dir: &File, from: &str, to: &str) -> io::Result<()> {
    let (f, t) = (
        CString::new(from).map_err(io::Error::other)?,
        CString::new(to).map_err(io::Error::other)?,
    );
    // SAFETY: valid descriptor and C strings
    let rc = unsafe {
        libc::renameat2(
            dir.as_raw_fd(),
            f.as_ptr(),
            dir.as_raw_fd(),
            t.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The path is a mount point (listed in `/proc/self/mountinfo`).
pub fn is_mount_point(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let want = path.as_os_str().as_bytes();
    text.lines().any(|l| {
        l.split(' ')
            .nth(4)
            .is_some_and(|mp| unescape_mount(mp).as_slice() == want)
    })
}

/// `\040` style escapes of mountinfo.
fn unescape_mount(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 4 <= b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_and_numbers_match_the_kernel() {
        assert_eq!(std::mem::size_of::<SubvolInfoArgs>(), 504);
        assert_eq!(std::mem::size_of::<VolArgsV2>(), 4096);
        assert_eq!(std::mem::size_of::<VolArgs>(), 4096);
        assert_eq!(IOC_SNAP_DESTROY, 0x5000_940f);
        assert_eq!(IOC_SNAP_CREATE_V2, 0x5000_9417);
        assert_eq!(IOC_GET_SUBVOL_INFO, 0x81f8_943c);
        assert_eq!(IOC_FS_INFO, 0x8400_941f);
        assert_eq!(IOC_FICLONE, 0x4004_9409);
    }

    #[test]
    fn uuids_and_names() {
        let mut b = [0u8; 16];
        assert_eq!(uuid_text(&b), None);
        b[0] = 0xab;
        b[15] = 1;
        assert_eq!(
            uuid_text(&b).unwrap(),
            "ab000000-0000-0000-0000-000000000001"
        );
        assert!(check_name("a/b", 10).is_err() && check_name("..", 10).is_err());
        assert!(check_name("docs.20261105T1200", 4040).is_ok());
    }

    #[test]
    fn mountinfo_escapes() {
        assert_eq!(unescape_mount("/mnt/a\\040b"), b"/mnt/a b");
        assert_eq!(unescape_mount("/plain"), b"/plain");
        assert!(is_mount_point(Path::new("/")));
        assert!(!is_mount_point(Path::new("/nonexistent/rynas")));
    }

    #[test]
    fn a_plain_directory_is_not_a_subvolume() {
        let d = tempfile::tempdir().unwrap();
        let f = open_dir(d.path()).unwrap();
        assert!(!is_subvolume(&f) || is_btrfs(&f));
        std::fs::create_dir(d.path().join("x")).unwrap();
        let x = open_dir_at(&f, "x").unwrap();
        assert!(!is_subvolume(&x));
        std::os::unix::fs::symlink("x", d.path().join("l")).unwrap();
        assert!(open_dir_at(&f, "l").is_err(), "symlinks are not followed");
    }
}
