//! Restore of a file or folder from a snapshot into the live subvolume, and rollback of the
//! whole subvolume by rename.
//!
//! Both sides are walked by descriptor: the snapshot as in [`crate::browse`], the live
//! subvolume one folder at a time with `O_DIRECTORY|O_NOFOLLOW` and the same `st_dev`, so a
//! symlink planted in the share cannot redirect the restore elsewhere. Files are reflinked
//! (`FICLONE`: instant, no extra space) and fall back to a copy; owner, permissions, extended
//! attributes (POSIX ACLs) and times are kept. Nothing existing is overwritten: a "replace"
//! first renames the current version, a "copy" picks a new name.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rynas_api_types::snapshots::RestoreMode;

use crate::browse::{self, Item, fd_path, handle_in};
use crate::ioctl;
use crate::name::{human_minute, stamp_seconds};
use crate::scan::SnapError;

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Restored {
    /// Where the restored copy is, relative to the subvolume.
    pub restored_to: String,
    /// Where the previous version went (replace), relative to the subvolume.
    pub saved_as: Option<String>,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Entries left out: devices, sockets, nested subvolumes.
    pub skipped: u64,
}

const MAX_DEPTH: usize = 512;

fn cstr(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(io::Error::other)
}

fn last_err<T>(rc: libc::c_int, v: T) -> io::Result<T> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(v)
    }
}

fn exists_at(dir: &File, name: &str) -> bool {
    handle_in(dir, OsStr::new(name)).is_ok()
}

/// `name (restored 2026-11-05 1337).ext`, numbered if taken.
fn restored_name(dir: &File, name: &str, is_file: bool, now_s: i64) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if is_file && i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let when = human_minute(now_s);
    let mut n = 1;
    loop {
        let candidate = if n == 1 {
            format!("{stem} (restored {when}){ext}")
        } else {
            format!("{stem} (restored {when} {n}){ext}")
        };
        if !exists_at(dir, &candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Opens (creating when missing, with the snapshot's metadata) the live folders of
/// `parents`, starting at the live root.
fn live_parent(
    live: File,
    snap_root: &Item,
    parents: &[&str],
    created: &mut Restored,
) -> Result<File, SnapError> {
    let dev = live.metadata()?.dev();
    let mut cur = live;
    let mut snap_dir = snap_root.handle.try_clone()?;
    for p in parents {
        let snap_next = handle_in(&snap_dir, OsStr::new(p))?;
        match ioctl::open_dir_at(&cur, p) {
            Ok(next) => {
                if next.metadata()?.dev() != dev {
                    return Err(SnapError::Invalid(
                        "the path leaves the shared folder (nested subvolume or mount)".into(),
                    ));
                }
                cur = next;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // recreate a folder deleted since the snapshot
                let c = cstr(p)?;
                // SAFETY: valid descriptor and C string
                last_err(
                    unsafe { libc::mkdirat(cur.as_raw_fd(), c.as_ptr(), 0o700) },
                    (),
                )?;
                let next = ioctl::open_dir_at(&cur, p)?;
                let meta = snap_next.metadata()?;
                let src = File::open(fd_path(&snap_next))?;
                copy_meta(&src, &next, &meta);
                created.dirs += 1;
                cur = next;
            }
            Err(_) => {
                return Err(SnapError::Invalid(format!(
                    "{p} is not a folder in the shared folder (or is a symlink)"
                )));
            }
        }
        snap_dir = snap_next;
    }
    Ok(cur)
}

/// Owner, permissions, extended attributes and times of `meta`/`src` onto `dst`.
fn copy_meta(src: &File, dst: &File, meta: &std::fs::Metadata) {
    let fd = dst.as_raw_fd();
    // SAFETY: plain syscalls on valid descriptors; failures are ignored (best effort, e.g.
    // a filesystem without xattrs) except for what the caller checks
    unsafe {
        libc::fchown(fd, meta.uid(), meta.gid());
        libc::fchmod(fd, (meta.mode() & 0o7777) as libc::mode_t);
    }
    copy_xattrs(src, dst);
    let times = [
        libc::timespec {
            tv_sec: meta.atime() as libc::time_t,
            tv_nsec: meta.atime_nsec() as _,
        },
        libc::timespec {
            tv_sec: meta.mtime() as libc::time_t,
            tv_nsec: meta.mtime_nsec() as _,
        },
    ];
    // SAFETY: valid descriptor and array of two timespecs
    unsafe {
        libc::futimens(fd, times.as_ptr());
    }
}

fn copy_xattrs(src: &File, dst: &File) {
    let mut names = vec![0u8; 64 << 10];
    // SAFETY: the kernel writes at most names.len() bytes
    let n = unsafe { libc::flistxattr(src.as_raw_fd(), names.as_mut_ptr().cast(), names.len()) };
    if n <= 0 {
        return;
    }
    let mut value = vec![0u8; 64 << 10];
    for name in names[..n as usize]
        .split(|&b| b == 0)
        .filter(|n| !n.is_empty())
    {
        let Ok(c) = CString::new(name) else { continue };
        // SAFETY: valid descriptor, C string and buffer
        let len = unsafe {
            libc::fgetxattr(
                src.as_raw_fd(),
                c.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        if len < 0 {
            continue;
        }
        // SAFETY: as above
        unsafe {
            libc::fsetxattr(
                dst.as_raw_fd(),
                c.as_ptr(),
                value.as_ptr().cast(),
                len as usize,
                0,
            );
        }
    }
}

struct Copier<'a> {
    dev: u64,
    cancelled: &'a dyn Fn() -> bool,
    out: Restored,
}

impl Copier<'_> {
    /// Copies the snapshot entry `handle` (with `meta`) as `name` into `dst_dir`.
    fn copy(
        &mut self,
        handle: &File,
        meta: &std::fs::Metadata,
        dst_dir: &File,
        name: &str,
        depth: usize,
    ) -> Result<(), SnapError> {
        if (self.cancelled)() {
            return Err(SnapError::Failed("cancelled".into()));
        }
        if depth > MAX_DEPTH {
            return Err(SnapError::Failed("folders nested too deep".into()));
        }
        if meta.dev() != self.dev {
            self.out.skipped += 1;
            return Ok(());
        }
        let t = meta.file_type();
        let c = cstr(name)?;
        if t.is_dir() {
            // SAFETY: valid descriptor and C string
            last_err(
                unsafe { libc::mkdirat(dst_dir.as_raw_fd(), c.as_ptr(), 0o700) },
                (),
            )?;
            let dst = ioctl::open_dir_at(dst_dir, name)?;
            let src = File::open(fd_path(handle))?;
            let mut children: Vec<_> = std::fs::read_dir(fd_path(handle))?
                .flatten()
                .map(|e| e.file_name())
                .collect();
            children.sort();
            for child in children {
                let Ok(h) = handle_in(handle, &child) else {
                    continue;
                };
                let Ok(m) = h.metadata() else { continue };
                let child_name = child.to_string_lossy().into_owned();
                if child.as_bytes() != child_name.as_bytes() {
                    // names that are not UTF-8 are kept as they are
                    self.copy_raw(&h, &m, &dst, child.as_bytes(), depth + 1)?;
                } else {
                    self.copy(&h, &m, &dst, &child_name, depth + 1)?;
                }
            }
            copy_meta(&src, &dst, meta);
            self.out.dirs += 1;
        } else if t.is_file() {
            let mut src = File::open(fd_path(handle))?;
            // SAFETY: valid descriptor and C string
            let fd = unsafe {
                libc::openat(
                    dst_dir.as_raw_fd(),
                    c.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            let mut dst = last_err(fd, ()).map(|_| unsafe { File::from_raw_fd(fd) })?;
            if ioctl::clone_file(&src, &dst).is_err() {
                io::copy(&mut src, &mut dst)?;
            }
            copy_meta(&src, &dst, meta);
            self.out.files += 1;
            self.out.bytes += meta.len();
        } else if t.is_symlink() {
            let target = read_link(handle)?;
            let tc = CString::new(target.as_bytes()).map_err(io::Error::other)?;
            // SAFETY: valid descriptor and C strings
            last_err(
                unsafe { libc::symlinkat(tc.as_ptr(), dst_dir.as_raw_fd(), c.as_ptr()) },
                (),
            )?;
            // SAFETY: as above; best effort
            unsafe {
                libc::fchownat(
                    dst_dir.as_raw_fd(),
                    c.as_ptr(),
                    meta.uid(),
                    meta.gid(),
                    libc::AT_SYMLINK_NOFOLLOW,
                );
            }
            self.out.files += 1;
        } else {
            self.out.skipped += 1;
        }
        Ok(())
    }

    /// Same for a name that is not valid UTF-8.
    fn copy_raw(
        &mut self,
        handle: &File,
        meta: &std::fs::Metadata,
        dst_dir: &File,
        name: &[u8],
        depth: usize,
    ) -> Result<(), SnapError> {
        // the copy works on &str names: go through a temporary UTF-8 name, then rename
        let tmp = format!(
            ".rynas-restore-{}-{}",
            std::process::id(),
            self.out.files + self.out.dirs
        );
        self.copy(handle, meta, dst_dir, &tmp, depth)?;
        let (from, to) = (cstr(&tmp)?, CString::new(name).map_err(io::Error::other)?);
        // SAFETY: valid descriptor and C strings
        last_err(
            unsafe {
                libc::renameat2(
                    dst_dir.as_raw_fd(),
                    from.as_ptr(),
                    dst_dir.as_raw_fd(),
                    to.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            },
            (),
        )?;
        Ok(())
    }
}

fn read_link(handle: &File) -> io::Result<std::ffi::OsString> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    let empty = c"";
    // SAFETY: an O_PATH|O_NOFOLLOW descriptor of the link and an empty path read the link
    let n = unsafe {
        libc::readlinkat(
            handle.as_raw_fd(),
            empty.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    Ok(std::os::unix::ffi::OsStringExt::from_vec(buf))
}

/// Restores `rel` (a file or a folder, not the whole snapshot: see [`rollback`]) from the
/// read-only snapshot at `snapshot` into the live subvolume at `live_root`.
pub fn restore(
    snapshot: &Path,
    live_root: &Path,
    rel: &str,
    mode: RestoreMode,
    now_s: i64,
    cancelled: &dyn Fn() -> bool,
) -> Result<Restored, SnapError> {
    let parts = browse::components(rel)?;
    let Some((name, parents)) = parts.split_last() else {
        return Err(SnapError::Invalid(
            "choose a file or folder; the whole shared folder goes back with a rollback".into(),
        ));
    };
    let snap_root = browse::resolve(snapshot, "")?;
    let item = browse::resolve(snapshot, rel)?;
    let live = ioctl::open_dir(live_root)
        .map_err(|e| SnapError::Failed(format!("{}: {e}", live_root.display())))?;
    let mut out = Restored::default();
    let parent = live_parent(live, &snap_root, parents, &mut out)?;
    let prefix = parents.join("/");
    let rel_of = |n: &str| {
        if prefix.is_empty() {
            n.to_string()
        } else {
            format!("{prefix}/{n}")
        }
    };
    let target = match mode {
        RestoreMode::Copy if exists_at(&parent, name) => {
            restored_name(&parent, name, item.meta.is_file(), now_s)
        }
        RestoreMode::Copy => name.to_string(),
        RestoreMode::Replace => {
            if exists_at(&parent, name) {
                let saved = format!("{name}.rynas-before-restore-{}", stamp_seconds(now_s));
                ioctl::rename_noreplace(&parent, name, &saved)
                    .map_err(|e| SnapError::Failed(format!("saving the current {name}: {e}")))?;
                out.saved_as = Some(rel_of(&saved));
            }
            name.to_string()
        }
    };
    let mut c = Copier {
        dev: snap_root.meta.dev(),
        cancelled,
        out,
    };
    c.copy(&item.handle, &item.meta, &parent, &target, 0)?;
    c.out.restored_to = rel_of(&target);
    Ok(c.out)
}

/// What a rollback did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolledBack {
    /// The previous live subvolume, kept under this name next to it.
    pub kept_as: String,
}

/// The subvolume at `live` goes back to the read-only snapshot `snapshot`: a writable
/// snapshot of it is created next to `live`, then `live` is renamed
/// `<name>.rynas-rollback-<stamp>` and the new one takes its name (`rename(2)`, atomic per
/// step; nothing is deleted). Refused when `live` is a mount point.
pub fn rollback(snapshot: &Path, live: &Path, now_s: i64) -> Result<RolledBack, SnapError> {
    let snap = browse::open_snapshot(snapshot)?;
    let (Some(parent_path), Some(name)) = (live.parent(), live.file_name()) else {
        return Err(SnapError::Invalid("invalid subvolume path".into()));
    };
    let name = name
        .to_str()
        .ok_or_else(|| SnapError::Invalid("invalid subvolume path".into()))?;
    if ioctl::is_mount_point(live) {
        return Err(SnapError::Refused(format!(
            "{} is a mount point: it cannot be renamed",
            live.display()
        )));
    }
    let parent = ioctl::open_dir(parent_path)?;
    let cur = ioctl::open_dir_at(&parent, name)?;
    if !ioctl::is_subvolume(&cur) {
        return Err(SnapError::Refused(format!(
            "{} is not a btrfs subvolume",
            live.display()
        )));
    }
    match (ioctl::fs_uuid(&cur), ioctl::fs_uuid(&snap)) {
        (Ok(a), Ok(b)) if a == b => {}
        _ => {
            return Err(SnapError::Refused(
                "the snapshot is not on the same filesystem".into(),
            ));
        }
    }
    drop(cur);
    let stamp = stamp_seconds(now_s);
    let fresh = format!("{name}.rynas-new-{stamp}");
    let kept = format!("{name}.rynas-rollback-{stamp}");
    ioctl::create_snapshot(&snap, &parent, &fresh, false)
        .map_err(|e| SnapError::Failed(format!("writable copy of the snapshot: {e}")))?;
    if let Err(e) = ioctl::rename_noreplace(&parent, name, &kept) {
        let _ = ioctl::destroy_subvolume(&parent, &fresh);
        return Err(SnapError::Failed(format!(
            "renaming the current {name}: {e}"
        )));
    }
    if let Err(e) = ioctl::rename_noreplace(&parent, &fresh, name) {
        // put the current one back
        let _ = ioctl::rename_noreplace(&parent, &kept, name);
        return Err(SnapError::Failed(format!("moving the copy in place: {e}")));
    }
    // SAFETY: valid descriptor
    unsafe {
        libc::fsync(parent.as_raw_fd());
    }
    Ok(RolledBack { kept_as: kept })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never() -> bool {
        false
    }

    /// Plain directories stand in for the live side; the snapshot side needs a read-only
    /// subvolume, so the copier is tested directly.
    #[test]
    fn copies_trees_keeping_metadata_and_links() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        std::fs::write(src.join("a/b/f.txt"), "hello").unwrap();
        std::fs::write(src.join("a/x"), vec![1u8; 70_000]).unwrap();
        std::os::unix::fs::symlink("b/f.txt", src.join("a/link")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(src.join("a/x"), std::fs::Permissions::from_mode(0o640)).unwrap();
        let dst = ioctl::open_dir(d.path()).unwrap();
        let root = File::open(&src).unwrap();
        let item = browse::resolve_in(root, "src", "a").unwrap();
        let mut c = Copier {
            dev: item.meta.dev(),
            cancelled: &never,
            out: Restored::default(),
        };
        c.copy(&item.handle, &item.meta, &dst, "a-copy", 0).unwrap();
        let out = d.path().join("a-copy");
        assert_eq!(
            std::fs::read_to_string(out.join("b/f.txt")).unwrap(),
            "hello"
        );
        assert_eq!(std::fs::read(out.join("x")).unwrap().len(), 70_000);
        assert_eq!(
            std::fs::read_link(out.join("link")).unwrap(),
            Path::new("b/f.txt")
        );
        assert_eq!(
            std::fs::metadata(out.join("x"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            std::fs::metadata(out.join("x")).unwrap().mtime(),
            std::fs::metadata(src.join("a/x")).unwrap().mtime()
        );
        assert_eq!((c.out.files, c.out.dirs, c.out.bytes), (3, 2, 70_005));
        // never overwrites
        let item = browse::resolve_in(File::open(&src).unwrap(), "src", "a").unwrap();
        assert!(c.copy(&item.handle, &item.meta, &dst, "a-copy", 0).is_err());
    }

    #[test]
    fn restored_names_are_new() {
        let d = tempfile::tempdir().unwrap();
        let dir = ioctl::open_dir(d.path()).unwrap();
        let now = 1_793_842_674;
        let n1 = restored_name(&dir, "report.xlsx", true, now);
        assert!(
            n1.starts_with("report (restored ") && n1.ends_with(").xlsx"),
            "{n1}"
        );
        std::fs::write(d.path().join(&n1), "").unwrap();
        let n2 = restored_name(&dir, "report.xlsx", true, now);
        assert!(n2.ends_with(" 2).xlsx"), "{n2}");
        let n3 = restored_name(&dir, "photos.2026", false, now);
        assert!(n3.starts_with("photos.2026 (restored "), "{n3}");
    }

    #[test]
    fn restore_needs_a_path_and_a_snapshot() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(
            restore(d.path(), d.path(), "", RestoreMode::Copy, 0, &never),
            Err(SnapError::Invalid(_))
        ));
        assert!(matches!(
            restore(d.path(), d.path(), "a/../b", RestoreMode::Copy, 0, &never),
            Err(SnapError::Invalid(_))
        ));
        // a plain directory is not a read-only snapshot
        assert!(matches!(
            restore(d.path(), d.path(), "x", RestoreMode::Copy, 0, &never),
            Err(SnapError::Refused(_))
        ));
        assert!(matches!(
            rollback(d.path(), &d.path().join("live"), 0),
            Err(SnapError::Refused(_))
        ));
    }
}
