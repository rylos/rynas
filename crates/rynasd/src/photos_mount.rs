//! The read-only mount of a user's `Photos` folder into a photo app's staging folder
//! (D-PHOTOS, `docs/phase2/photos.md` §3). Run as `rynasd photos-mount` in a transient unit
//! (rynasd's own sandbox has no mount calls), in the host's mount namespace.
//!
//! The user owns their home and may replace `Photos` with a link at any time, so the folder is
//! never named by a path the user can change: the home is opened without following its last
//! component (its parents are root's), `Photos` is opened below it with `O_NOFOLLOW` and
//! checked (a folder of the user), and the mount is made **from that descriptor**
//! (`open_tree` + `mount_setattr` read-only, nosuid, nodev, noexec + `move_mount`; on
//! kernels without them a bind of `/proc/self/fd/N` and a read-only remount). A link swapped
//! in afterwards changes nothing: the mount holds the folder, not the name. The target is
//! a root-owned folder of the staging folder. A folder that fails the checks leaves the
//! target as it was: a user already mounted stays mounted (an app that loses a folder
//! trashes its photos after a while), a user not mounted yet stays an empty folder.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

/// The folder of a home the RyNAS app backs up into.
pub const FOLDER: &str = "Photos";

fn cstr(p: &Path) -> Result<CString, String> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| format!("{}: a NUL in the path", p.display()))
}

fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn fstat(fd: RawFd) -> Result<libc::stat, String> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat fills `st` for a valid descriptor; checked by its return value
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } != 0 {
        return Err(errno().to_string());
    }
    // SAFETY: fstat succeeded
    Ok(unsafe { st.assume_init() })
}

fn open_dir_at(dirfd: RawFd, name: &CString) -> std::io::Result<OwnedFd> {
    // SAFETY: plain openat with a NUL-terminated name; the descriptor is owned below
    let fd = unsafe { libc::openat(dirfd, name.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(errno());
    }
    // SAFETY: a fresh descriptor nobody else owns
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Whether `name` below `dirfd` is a symbolic link (for the message only).
fn is_link_at(dirfd: RawFd, name: &CString) -> bool {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstatat fills `st`; checked by its return value
    unsafe { libc::fstatat(dirfd, name.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) == 0 && st.assume_init().st_mode & libc::S_IFMT == libc::S_IFLNK }
}

/// The user's `Photos` folder, opened: `home` (the real home, links of its parents already
/// resolved by rynasd) must be a folder of `uid` and not a link, `Photos` below it a folder of
/// `uid` and not a link.
pub fn open_photos(home: &Path, uid: u32) -> Result<OwnedFd, String> {
    let h = open_dir_at(libc::AT_FDCWD, &cstr(home)?).map_err(|e| match e.raw_os_error() {
        Some(libc::ELOOP) | Some(libc::ENOTDIR) => format!("the home {} is not a folder", home.display()),
        Some(libc::ENOENT) => format!("the home {} does not exist", home.display()),
        _ => format!("the home {}: {e}", home.display()),
    })?;
    let st = fstat(h.as_raw_fd())?;
    if st.st_uid != uid {
        return Err(format!("the home {} does not belong to the user", home.display()));
    }
    let name = CString::new(FOLDER).unwrap_or_default();
    let p = open_dir_at(h.as_raw_fd(), &name).map_err(|e| match e.raw_os_error() {
        Some(libc::ELOOP | libc::ENOTDIR) if is_link_at(h.as_raw_fd(), &name) => format!("{FOLDER} is a link, not a folder"),
        Some(libc::ELOOP | libc::ENOTDIR) => format!("{FOLDER} is not a folder"),
        Some(libc::ENOENT) => format!("no {FOLDER} folder yet"),
        _ => format!("{FOLDER}: {e}"),
    })?;
    let st = fstat(p.as_raw_fd())?;
    if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(format!("{FOLDER} is not a folder"));
    }
    if st.st_uid != uid {
        return Err(format!("{FOLDER} does not belong to the user"));
    }
    Ok(p)
}

/// The target: a folder owned by root that only root may change.
pub fn check_target(target: &Path) -> Result<(), String> {
    let t = open_dir_at(libc::AT_FDCWD, &cstr(target)?).map_err(|e| format!("{}: {e}", target.display()))?;
    let st = fstat(t.as_raw_fd())?;
    if st.st_uid != 0 || st.st_mode & 0o022 != 0 {
        return Err(format!("{} is not a folder only root may change", target.display()));
    }
    Ok(())
}

/// Unescapes a mount point of `/proc/self/mountinfo` (`\040` for a space…).
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `path` is a mount point in `mountinfo` (the text of `/proc/self/mountinfo`).
pub fn is_mount_point(mountinfo: &str, path: &Path) -> bool {
    let want = path.to_string_lossy();
    mountinfo.lines().filter_map(|l| l.split(' ').nth(4)).any(|m| unescape(m) == want)
}

fn mounted(path: &Path) -> bool {
    is_mount_point(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default(), path)
}

/// Detaches every mount stacked at `target` (nothing when it is not a mount point).
pub fn unmount(target: &Path) -> Result<bool, String> {
    let t = cstr(target)?;
    let mut any = false;
    for _ in 0..16 {
        if !mounted(target) {
            return Ok(any);
        }
        // SAFETY: umount2 with a NUL-terminated path
        if unsafe { libc::umount2(t.as_ptr(), libc::MNT_DETACH | libc::UMOUNT_NOFOLLOW) } != 0 {
            return Err(format!("{}: {}", target.display(), errno()));
        }
        any = true;
    }
    Err(format!("{}: still a mount point after 16 unmounts", target.display()))
}

const OPEN_TREE_CLONE: libc::c_uint = 1;
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x4;
const MOUNT_ATTR_RDONLY: u64 = 0x1;
const MOUNT_ATTR_NOSUID: u64 = 0x2;
const MOUNT_ATTR_NODEV: u64 = 0x4;
const MOUNT_ATTR_NOEXEC: u64 = 0x8;
/// Links are never followed inside the mount (Linux 5.10): without it a user's
/// `Photos/x -> /mnt/photos/<other user>` would show another user's photos in the app.
const MOUNT_ATTR_NOSYMFOLLOW: u64 = 0x0020_0000;
/// `MS_PRIVATE` as `mount_setattr` wants it (`linux/mount.h`).
const MS_PRIVATE: u64 = 1 << 18;
/// `MS_NOSYMFOLLOW` (`linux/mount.h`, Linux 5.10).
const MS_NOSYMFOLLOW: libc::c_ulong = 256;

/// The attributes the new mount gets before it is attached.
pub const fn mount_attrs() -> u64 {
    MOUNT_ATTR_RDONLY | MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV | MOUNT_ATTR_NOEXEC | MOUNT_ATTR_NOSYMFOLLOW
}

/// The flags of the read-only remount of the older kernels' path.
pub const fn remount_flags() -> libc::c_ulong {
    libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | MS_NOSYMFOLLOW
}

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

/// `open_tree` + `mount_setattr` + `move_mount`: the mount is read-only, without set-id,
/// devices, programs or links before it is attached. `Err(None)`: the kernel lacks the calls
/// (an attribute it does not know is an error: never a mount without it).
fn mount_new_api(src: &OwnedFd, target: &CString) -> Result<(), Option<String>> {
    let empty = c"";
    // SAFETY: open_tree on a valid descriptor with an empty path; the result is owned below
    let tree = unsafe { libc::syscall(libc::SYS_open_tree, src.as_raw_fd(), empty.as_ptr(), OPEN_TREE_CLONE | libc::O_CLOEXEC as libc::c_uint | libc::AT_EMPTY_PATH as libc::c_uint) };
    if tree < 0 {
        let e = errno();
        return Err((e.raw_os_error() != Some(libc::ENOSYS)).then(|| format!("open_tree: {e}")));
    }
    // SAFETY: a fresh descriptor nobody else owns
    let tree = unsafe { OwnedFd::from_raw_fd(tree as RawFd) };
    let attr = MountAttr { attr_set: mount_attrs(), attr_clr: 0, propagation: MS_PRIVATE, userns_fd: 0 };
    // SAFETY: mount_setattr on the detached tree with a correctly sized attribute struct
    let r = unsafe { libc::syscall(libc::SYS_mount_setattr, tree.as_raw_fd(), empty.as_ptr(), libc::AT_EMPTY_PATH as libc::c_uint, &attr as *const MountAttr, std::mem::size_of::<MountAttr>()) };
    if r < 0 {
        let e = errno();
        return Err((e.raw_os_error() != Some(libc::ENOSYS)).then(|| format!("mount_setattr (read-only, no links followed): {e}")));
    }
    // SAFETY: move_mount of the detached tree onto a NUL-terminated target path
    let r = unsafe { libc::syscall(libc::SYS_move_mount, tree.as_raw_fd(), empty.as_ptr(), libc::AT_FDCWD, target.as_ptr(), MOVE_MOUNT_F_EMPTY_PATH) };
    if r < 0 {
        return Err(Some(format!("move_mount: {}", errno())));
    }
    Ok(())
}

fn sys_mount(src: Option<&CString>, target: &CString, flags: libc::c_ulong) -> Result<(), std::io::Error> {
    // SAFETY: mount with NUL-terminated paths (or none) and no data
    let r = unsafe { libc::mount(src.map_or(std::ptr::null(), |s| s.as_ptr()), target.as_ptr(), std::ptr::null(), flags, std::ptr::null()) };
    if r != 0 { Err(errno()) } else { Ok(()) }
}

/// `statvfs` flags every mount of a Photos folder must have (`sys/statvfs.h`).
pub const ST_RDONLY: u64 = 1;
pub const ST_NOSUID: u64 = 2;
pub const ST_NODEV: u64 = 4;
pub const ST_NOEXEC: u64 = 8;
pub const ST_NOSYMFOLLOW: u64 = 0x2000;
pub const REQUIRED_ST: u64 = ST_RDONLY | ST_NOSUID | ST_NODEV | ST_NOEXEC | ST_NOSYMFOLLOW;

/// The flags of [`REQUIRED_ST`] missing from `flags` (`statvfs` `f_flag`), by name.
pub fn missing_flags(flags: u64) -> Vec<&'static str> {
    [(ST_RDONLY, "read-only"), (ST_NOSUID, "nosuid"), (ST_NODEV, "nodev"), (ST_NOEXEC, "noexec"), (ST_NOSYMFOLLOW, "nosymfollow")]
        .into_iter()
        .filter(|(f, _)| flags & f == 0)
        .map(|(_, n)| n)
        .collect()
}

/// Checks the mount at `path` with `statvfs`: what the kernel really applied.
fn verify_flags(path: &CString) -> Result<(), String> {
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: statvfs fills `st`; checked by its return value
    if unsafe { libc::statvfs(path.as_ptr(), st.as_mut_ptr()) } != 0 {
        return Err(format!("statvfs: {}", errno()));
    }
    // SAFETY: statvfs succeeded
    #[allow(clippy::unnecessary_cast)]
    let flags = unsafe { st.assume_init() }.f_flag as u64;
    match missing_flags(flags).as_slice() {
        [] => Ok(()),
        m => Err(format!("the kernel did not apply {}: not mounted", m.join(", "))),
    }
}

/// The private scratch folder of the older kernels' path: made once, a mount of itself made
/// private, so nothing mounted inside propagates anywhere.
const SCRATCH: &str = "/run/rynas-photos";

fn scratch_root() -> Result<std::path::PathBuf, String> {
    let root = std::path::PathBuf::from(SCRATCH);
    if !root.is_dir() {
        std::fs::DirBuilder::new().mode(0o700).create(&root).map_err(|e| format!("{SCRATCH}: {e}"))?;
    }
    let s = cstr(&root)?;
    if !mounted(&root) {
        sys_mount(Some(&s), &s, libc::MS_BIND).map_err(|e| format!("{SCRATCH}: bind: {e}"))?;
    }
    sys_mount(None, &s, libc::MS_PRIVATE).map_err(|e| format!("{SCRATCH}: private: {e}"))?;
    // what a helper killed halfway left: subfolders of processes that are gone
    for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.parse::<u32>().is_ok_and(|pid| !Path::new(&format!("/proc/{pid}")).exists()) {
            clean_scratch(&e.path());
        }
    }
    Ok(root)
}

fn clean_scratch(dir: &Path) {
    let inner = dir.join("m");
    for _ in 0..16 {
        if !mounted(&inner) {
            break;
        }
        if let Ok(i) = cstr(&inner) {
            // SAFETY: umount2 with a NUL-terminated path
            unsafe { libc::umount2(i.as_ptr(), libc::MNT_DETACH) };
        }
    }
    let _ = std::fs::remove_dir(&inner);
    let _ = std::fs::remove_dir(dir);
}

/// Older kernels: the bind is made and made read-only on the private scratch folder (nothing
/// propagates from there), checked with `statvfs`, then moved onto the target: an app never
/// sees it writable. A kernel without `MS_NOSYMFOLLOW` refuses the remount, or leaves the flag
/// out (checked): nothing is mounted.
fn mount_old_api(src: &OwnedFd, target: &CString) -> Result<(), String> {
    let dir = scratch_root()?.join(std::process::id().to_string());
    clean_scratch(&dir);
    std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let inner = dir.join("m");
    let r = (|| {
        std::fs::DirBuilder::new().mode(0o700).create(&inner).map_err(|e| format!("{}: {e}", inner.display()))?;
        let i = cstr(&inner)?;
        let from = CString::new(format!("/proc/self/fd/{}", src.as_raw_fd())).unwrap_or_default();
        sys_mount(Some(&from), &i, libc::MS_BIND).map_err(|e| format!("bind: {e}"))?;
        sys_mount(None, &i, remount_flags()).map_err(|e| match e.raw_os_error() {
            Some(libc::EINVAL) => "this kernel cannot keep links from being followed in a mount (Linux 5.10 or later is needed): not mounted".to_string(),
            _ => format!("read-only remount: {e}"),
        })?;
        verify_flags(&i)?;
        sys_mount(Some(&i), target, libc::MS_MOVE).map_err(|e| format!("move: {e}"))
    })();
    // whatever happened: what is left in the scratch folder goes
    clean_scratch(&dir);
    r
}

/// What [`mount`] does with a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountPlan {
    /// Not a mount point: checked (root's) and mounted.
    New,
    /// The same folder is mounted there: nothing to do.
    Already,
    /// Another folder is mounted there: unmounted, then mounted.
    Replace,
}

/// The plan for a target: a mount point shows the mounted folder (the user's, never root's),
/// so it is compared with the source instead of being checked as a staging folder.
pub fn plan_mount(is_mount: bool, same_source: bool) -> MountPlan {
    match (is_mount, same_source) {
        (false, _) => MountPlan::New,
        (true, true) => MountPlan::Already,
        (true, false) => MountPlan::Replace,
    }
}

/// What [`mount`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mounted {
    /// The folder was mounted.
    New,
    /// The same folder was already mounted there.
    Already,
}

/// Mounts the user's `Photos` folder (checked first, [`open_photos`]) read-only at `target`
/// (root's, [`check_target`]). A failed check changes nothing at the target.
pub fn mount(home: &Path, uid: u32, target: &Path) -> Result<Mounted, String> {
    let src = open_photos(home, uid)?;
    let t = cstr(target)?;
    let is_mount = mounted(target);
    let same = is_mount && {
        // a mounted target shows the user's folder: compare it with the source
        let a = fstat(src.as_raw_fd())?;
        let b = std::fs::metadata(target).map_err(|e| e.to_string())?;
        use std::os::unix::fs::MetadataExt;
        a.st_dev == b.dev() && a.st_ino == b.ino()
    };
    match plan_mount(is_mount, same) {
        MountPlan::Already => return Ok(Mounted::Already),
        MountPlan::Replace => {
            // another folder (the user made a new Photos): the staging folder is checked
            // through its parent (root's), then the old mount goes and the target itself
            let parent = target.parent().ok_or("the target has no parent")?;
            check_target(parent)?;
            unmount(target)?;
            check_target(target)?;
        }
        MountPlan::New => check_target(target)?,
    }
    match mount_new_api(&src, &t) {
        Ok(()) => {}
        Err(Some(e)) => return Err(e),
        Err(None) => mount_old_api(&src, &t)?,
    }
    // what the kernel applied, whatever the path: a mount without a flag goes away
    if let Err(e) = verify_flags(&t) {
        let _ = unmount(target);
        return Err(e);
    }
    Ok(Mounted::New)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn me() -> u32 {
        // SAFETY: a plain getter
        unsafe { libc::getuid() }
    }

    #[test]
    fn photos_open_only_as_a_folder_of_the_user() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("ann");
        std::fs::create_dir_all(home.join("Photos/2026/11")).unwrap();
        let fd = open_photos(&home, me()).unwrap();
        assert_eq!(fstat(fd.as_raw_fd()).unwrap().st_ino, std::fs::metadata(home.join("Photos")).unwrap().ino());
        assert!(open_photos(&home, me() + 1).unwrap_err().contains("home"));
        // a link instead of the folder
        let bob = t.path().join("bob");
        std::fs::create_dir_all(&bob).unwrap();
        std::os::unix::fs::symlink("/", bob.join("Photos")).unwrap();
        assert_eq!(open_photos(&bob, me()).unwrap_err(), "Photos is a link, not a folder");
        // a home that is itself a link (never followed)
        std::os::unix::fs::symlink(&home, t.path().join("cid")).unwrap();
        assert!(open_photos(&t.path().join("cid"), me()).unwrap_err().contains("is not a folder"));
        // a file, and nothing yet
        let dan = t.path().join("dan");
        std::fs::create_dir_all(&dan).unwrap();
        std::fs::write(dan.join("Photos"), "x").unwrap();
        assert_eq!(open_photos(&dan, me()).unwrap_err(), "Photos is not a folder");
        let eve = t.path().join("eve");
        std::fs::create_dir_all(&eve).unwrap();
        assert_eq!(open_photos(&eve, me()).unwrap_err(), "no Photos folder yet");
    }

    /// The swap the review found: Photos replaced by a link to `/` after the check. The
    /// descriptor the mount is made from still names the folder that was checked.
    #[test]
    fn a_link_swapped_in_after_the_check_changes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("ann");
        std::fs::create_dir_all(home.join("Photos")).unwrap();
        std::fs::write(home.join("Photos/a.jpg"), "jpg").unwrap();
        let ino = std::fs::metadata(home.join("Photos")).unwrap().ino();
        let fd = open_photos(&home, me()).unwrap();
        std::fs::rename(home.join("Photos"), home.join("Old")).unwrap();
        std::os::unix::fs::symlink("/", home.join("Photos")).unwrap();
        // what a mount from the descriptor sees: the folder, by its inode
        let st = fstat(fd.as_raw_fd()).unwrap();
        assert_eq!((st.st_ino, st.st_mode & libc::S_IFMT), (ino, libc::S_IFDIR));
        let via = std::fs::read_dir(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap().flatten().map(|e| e.file_name()).collect::<Vec<_>>();
        assert_eq!(via, ["a.jpg"]);
        // and a new check refuses the link
        assert!(open_photos(&home, me()).is_err());
    }

    #[test]
    fn targets_are_root_folders() {
        let t = tempfile::tempdir().unwrap();
        if me() != 0 {
            assert!(check_target(t.path()).unwrap_err().contains("only root"));
        }
        assert!(check_target(&t.path().join("none")).is_err());
    }

    /// Review 2 H2: links are never followed inside a mount, on both paths, with the rest.
    #[test]
    fn mounts_never_follow_links_and_are_read_only() {
        assert_eq!(mount_attrs() & 0x0020_0000, 0x0020_0000, "MOUNT_ATTR_NOSYMFOLLOW");
        for a in [MOUNT_ATTR_RDONLY, MOUNT_ATTR_NOSUID, MOUNT_ATTR_NODEV, MOUNT_ATTR_NOEXEC] {
            assert_eq!(mount_attrs() & a, a);
        }
        assert_eq!(remount_flags() & 256, 256, "MS_NOSYMFOLLOW");
        // the statvfs check after the mount: every flag, by name when missing
        assert!(missing_flags(REQUIRED_ST).is_empty());
        assert_eq!(missing_flags(REQUIRED_ST & !ST_NOSYMFOLLOW), ["nosymfollow"]);
        assert_eq!(missing_flags(0).len(), 5);
        for f in [libc::MS_REMOUNT, libc::MS_BIND, libc::MS_RDONLY, libc::MS_NOSUID, libc::MS_NODEV, libc::MS_NOEXEC] {
            assert_eq!(remount_flags() & f, f);
        }
    }

    /// Real-kernel test finding: a second mount of a mounted user never checks the mounted
    /// (user-owned) folder as a staging folder; it compares the source instead.
    #[test]
    fn a_mounted_target_is_compared_not_checked() {
        assert_eq!(plan_mount(false, false), MountPlan::New);
        assert_eq!(plan_mount(true, true), MountPlan::Already);
        assert_eq!(plan_mount(true, false), MountPlan::Replace);
    }

    #[test]
    fn mount_points_from_mountinfo() {
        let mi = "36 35 98:0 / /mnt/a\\040b rw,noatime master:1 - ext4 /dev/sda1 rw\n37 35 98:0 /x /var/lib/rynas/docker/photos/immich/ann ro shared:2 - btrfs /dev/sdb rw\n";
        assert!(is_mount_point(mi, Path::new("/mnt/a b")));
        assert!(is_mount_point(mi, Path::new("/var/lib/rynas/docker/photos/immich/ann")));
        assert!(!is_mount_point(mi, Path::new("/var/lib/rynas/docker/photos/immich")));
    }
}
