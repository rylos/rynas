//! Ownership remap: files owned by one uid (or gid) are given to another, on chosen folders.
//!
//! Used after a UID/GID change (`usermod -u` only fixes the home directory and the mail
//! spool) and after a migration where data was copied with numeric owners that no local
//! account has (e.g. 1026 from another NAS). The walk:
//! - works on file descriptors, never on full paths: each folder of a root is opened
//!   `O_NOFOLLOW` from `/` down, each entry relative to its parent's descriptor
//!   (`statx`/`openat` with `AT_SYMLINK_NOFOLLOW`/`O_NOFOLLOW`), and every change is made
//!   through the descriptor of the entry that was checked (`fchownat(AT_EMPTY_PATH)`,
//!   `fchmod`, `fgetxattr`/`fsetxattr`). An entry swapped for a symbolic link (or another
//!   file) while the walk runs is either refused (inode changed between the check and the
//!   open) or changed as the inode that was opened, never the link's target;
//! - stays on the mount it starts on: an entry on another mount (`STATX_MNT_ID`; without it,
//!   the mount points of `/proc/self/mountinfo`) is skipped and reported. btrfs subvolumes
//!   of the same mount have their own device number but the same mount and are walked;
//! - skips read-only btrfs subvolumes (snapshots) and anything answering `EROFS`, and
//!   reports them: snapshots keep the old ids, which is what a snapshot is for;
//! - rewrites POSIX ACL entries naming the old id (`system.posix_acl_access` and, on
//!   directories, `system.posix_acl_default`) of directories and regular files, read and
//!   written as raw xattrs. When the new id has its own entry already, that entry stays,
//!   the old one goes and the file is reported as an ACL conflict;
//! - with [`RemapOpts::keep_setid`] (an account's own id change), puts back the
//!   set-user-ID/set-group-ID bits the kernel clears on `chown` of a regular file (`fchmod`
//!   of the descriptor). Directories keep theirs (the kernel never clears them) and are never
//!   chmod-ed. Taking over files of another id does not put them back: data from another
//!   system must not become set-user-ID to a real account;
//! - is cancellable, reports progress and can stop at a deadline (dry-run counts for the
//!   plan, marked partial).
//!
//! All changes go through [`RemapFs`]: [`HostRemap`] on a real system, [`DryRemap`] for
//! `rynasd --dry-system` (reads the host, writes nothing), fakes in tests. The reads of the
//! walk (statx, openat, readdir) are always real.

use std::collections::BTreeSet;
use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub use rynas_api_types::RemapCounts;

/// Which ids move: `(from, to)` for owners and/or groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IdMap {
    pub uid: Option<(u32, u32)>,
    pub gid: Option<(u32, u32)>,
}

impl IdMap {
    fn new_uid(&self, uid: u32) -> Option<u32> {
        self.uid.filter(|(f, _)| *f == uid).map(|(_, t)| t)
    }

    fn new_gid(&self, gid: u32) -> Option<u32> {
        self.gid.filter(|(f, _)| *f == gid).map(|(_, t)| t)
    }
}

/// Type of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
    /// FIFO, socket, device.
    Other,
}

fn kind_of(mode: u32) -> Kind {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => Kind::Dir,
        libc::S_IFREG => Kind::File,
        libc::S_IFLNK => Kind::Symlink,
        _ => Kind::Other,
    }
}

/// One entry of the walk, opened relative to its parent without following links.
pub struct Entry<'a> {
    /// Full path (reports and fakes only: nothing is done through it).
    pub path: &'a Path,
    /// Directories and regular files: opened for reading; anything else: `O_PATH`.
    pub fd: BorrowedFd<'a>,
    pub kind: Kind,
    pub uid: u32,
    pub gid: u32,
    /// `st_mode` (type and permission bits).
    pub mode: u32,
    pub ino: u64,
}

/// File system changes of the walk, all through the entry's descriptor.
pub trait RemapFs: Send + Sync {
    /// `fchownat(fd, "", AT_EMPTY_PATH)`; `None` leaves that id as it is.
    fn chown(&self, e: &Entry, uid: Option<u32>, gid: Option<u32>) -> io::Result<()>;
    /// `fchmod` of a regular file (set-id bits back after `chown`). Never called on
    /// directories, symbolic links or special files.
    fn chmod(&self, e: &Entry, mode: u32) -> io::Result<()>;
    /// The raw POSIX ACL xattr (`default`: the default ACL of a directory); `None` when the
    /// file has none or the file system does not support ACLs.
    fn get_acl(&self, e: &Entry, default: bool) -> io::Result<Option<Vec<u8>>>;
    fn set_acl(&self, e: &Entry, default: bool, value: &[u8]) -> io::Result<()>;
    /// A read-only btrfs subvolume (a snapshot), checked on every directory.
    fn readonly_subvolume(&self, e: &Entry) -> bool;
    /// Every mount point of the system (used when the kernel gives no mount ids).
    fn mount_points(&self) -> BTreeSet<PathBuf>;
    /// The mount `path` is on: its mount id when known, else the deepest mount point of
    /// `mounts` containing it. The walk never leaves the mount of its root.
    fn mount_of(&self, path: &Path, mnt_id: Option<u64>, mounts: &BTreeSet<PathBuf>) -> MountKey {
        match mnt_id {
            Some(id) => MountKey::Id(id),
            None => MountKey::Point(mount_point_of(path, mounts)),
        }
    }
}

/// See [`RemapFs::mount_of`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountKey {
    Id(u64),
    Point(Option<PathBuf>),
}

/// The deepest of `mounts` that is `path` or one of its parents.
pub fn mount_point_of(path: &Path, mounts: &BTreeSet<PathBuf>) -> Option<PathBuf> {
    path.ancestors()
        .find(|a| mounts.contains(*a))
        .map(Path::to_path_buf)
}

const ACL_ACCESS: &str = "system.posix_acl_access\0";
const ACL_DEFAULT: &str = "system.posix_acl_default\0";
const ACL_VERSION: u32 = 2;
const ACL_USER: u16 = 0x02;
const ACL_GROUP: u16 = 0x08;

/// A rewritten ACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclRemap {
    pub raw: Vec<u8>,
    /// Entries that named the old id.
    pub entries: u32,
    /// Of those, entries dropped because the new id had its own entry (which is kept).
    pub conflicts: u32,
}

/// Rewrites the entries of a raw POSIX ACL xattr (`posix_acl_xattr_header` + entries of tag,
/// perm, id, little endian) that name an id of `map`. When the new id already has an entry
/// of the same tag, that entry is kept as it is and the old id's entry is dropped (counted
/// in `conflicts`): the account's own permissions win over those copied from the other id.
/// Entries come out in canonical order (tag, then id). `None`: nothing to change (or not a
/// format this code knows, which is left alone).
pub fn remap_acl(raw: &[u8], map: &IdMap) -> Option<AclRemap> {
    if raw.len() < 4 || !(raw.len() - 4).is_multiple_of(8) {
        return None;
    }
    if u32::from_le_bytes(raw[0..4].try_into().ok()?) != ACL_VERSION {
        return None;
    }
    let ents: Vec<(u16, u16, u32)> = raw[4..]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|e| {
            (
                u16::from_le_bytes([e[0], e[1]]),
                u16::from_le_bytes([e[2], e[3]]),
                u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
            )
        })
        .collect();
    let has = |tag: u16, id: u32| ents.iter().any(|e| e.0 == tag && e.2 == id);
    let mut out = Vec::with_capacity(ents.len());
    let (mut n, mut conflicts) = (0, 0);
    for &(tag, perm, id) in &ents {
        let new = match tag {
            ACL_USER => map.new_uid(id),
            ACL_GROUP => map.new_gid(id),
            _ => None,
        };
        match new {
            Some(t) if t != id && has(tag, t) => {
                n += 1;
                conflicts += 1;
            }
            Some(t) => {
                n += 1;
                out.push((tag, perm, t));
            }
            None => out.push((tag, perm, id)),
        }
    }
    if n == 0 {
        return None;
    }
    out.sort_by_key(|e| (e.0, e.2));
    let mut v = raw[0..4].to_vec();
    for (t, p, id) in out {
        v.extend(t.to_le_bytes());
        v.extend(p.to_le_bytes());
        v.extend(id.to_le_bytes());
    }
    Some(AclRemap {
        raw: v,
        entries: n,
        conflicts,
    })
}

/// Options of a walk.
pub struct RemapOpts<'a> {
    pub map: IdMap,
    /// Rewrite ACL entries too.
    pub acls: bool,
    /// Put back the set-id bits of regular files after `chown` (the account's own files
    /// moving to its new id). Off when taking over another id's files.
    pub keep_setid: bool,
    /// Count only, change nothing.
    pub dry_run: bool,
    /// Stop there and mark the counts partial (the plan's budget).
    pub deadline: Option<Instant>,
    pub cancelled: &'a dyn Fn() -> bool,
    /// Called every [`TICK`] entries with the number of entries seen.
    pub tick: &'a dyn Fn(u64),
}

/// Entries between two calls of [`RemapOpts::tick`].
pub const TICK: u64 = 4096;
/// Error messages kept in the counts.
const MAX_ERRORS: usize = 20;
/// Reported paths of each kind (read-only, other file systems, ACL conflicts).
const MAX_PATHS: usize = 50;

/// Drops roots inside another root (each file is visited once) and keeps the order.
pub fn distinct_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = roots.to_vec();
    v.sort();
    v.dedup();
    let all = v.clone();
    v.retain(|p| !all.iter().any(|q| q != p && p.starts_with(q)));
    v
}

fn push_path(v: &mut Vec<String>, p: &Path) {
    if v.len() < MAX_PATHS {
        v.push(p.display().to_string());
    }
}

fn push_err(c: &mut RemapCounts, p: &Path, e: &io::Error) {
    c.errors += 1;
    if c.error_samples.len() < MAX_ERRORS {
        c.error_samples.push(format!("{}: {e}", p.display()));
    }
}

// ---------------------------------------------------------------------------------------------
// descriptors
// ---------------------------------------------------------------------------------------------

/// What `statx` says about an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct St {
    mode: u32,
    uid: u32,
    gid: u32,
    ino: u64,
    dev: (u32, u32),
    mnt_id: Option<u64>,
}

fn statx_at(dir: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<St> {
    // SAFETY: zeroed plain struct filled by the kernel; valid C string.
    let mut x: libc::statx = unsafe { std::mem::zeroed() };
    let r = unsafe {
        libc::statx(
            dir,
            name.as_ptr(),
            flags | libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT,
            libc::STATX_BASIC_STATS | libc::STATX_MNT_ID,
            &mut x,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(St {
        mode: u32::from(x.stx_mode),
        uid: x.stx_uid,
        gid: x.stx_gid,
        ino: x.stx_ino,
        dev: (x.stx_dev_major, x.stx_dev_minor),
        mnt_id: (x.stx_mask & libc::STATX_MNT_ID != 0).then_some(x.stx_mnt_id),
    })
}

fn openat(dir: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: valid C string; the descriptor returned is owned here.
    let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn changed() -> io::Error {
    io::Error::other("changed while the walk ran (replaced or made a link): skipped")
}

/// Opens `name` in `dir` as `pre` described it: a directory for reading, a regular file for
/// reading without blocking, anything else `O_PATH`; never through a symbolic link. The
/// opened inode must be the one `pre` saw.
fn open_checked(dir: BorrowedFd, name: &CStr, pre: &St) -> io::Result<(OwnedFd, St)> {
    let kind = kind_of(pre.mode);
    let flags = libc::O_NOFOLLOW
        | match kind {
            Kind::Dir => libc::O_RDONLY | libc::O_DIRECTORY,
            Kind::File => libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY,
            Kind::Symlink | Kind::Other => libc::O_PATH,
        };
    let fd = openat(dir.as_raw_fd(), name, flags).map_err(|e| {
        match e.raw_os_error() {
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => changed(),
            _ => e,
        }
    })?;
    let st = statx_at(fd.as_raw_fd(), c"", libc::AT_EMPTY_PATH)?;
    if st.ino != pre.ino || st.dev != pre.dev || kind_of(st.mode) != kind {
        return Err(changed());
    }
    Ok((fd, st))
}

/// The parent folder of `root`, opened from `/` one component at a time without following
/// any symbolic link, and the last component.
fn open_root(root: &Path) -> io::Result<(OwnedFd, CString)> {
    let mut parts = Vec::new();
    for c in root.components() {
        match c {
            Component::RootDir => {}
            Component::Normal(s) => parts.push(s),
            _ => return Err(io::Error::other("not an absolute, normalized path")),
        }
    }
    if !root.is_absolute() {
        return Err(io::Error::other("not an absolute, normalized path"));
    }
    let Some(last) = parts.pop() else {
        return Err(io::Error::other("the root folder is not walked"));
    };
    let cs = |s: &OsStr| CString::new(s.as_bytes()).map_err(io::Error::other);
    let mut fd = openat(libc::AT_FDCWD, c"/", libc::O_PATH | libc::O_DIRECTORY)?;
    for p in parts {
        fd = openat(
            fd.as_raw_fd(),
            &cs(p)?,
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => {
                io::Error::other("a symbolic link in the path, not followed")
            }
            _ => e,
        })?;
    }
    Ok((fd, cs(last)?))
}

/// Names in a directory (not `.` and `..`).
fn read_names(dir: BorrowedFd) -> io::Result<Vec<CString>> {
    // SAFETY: a duplicate descriptor handed to (and closed by) the directory stream.
    let dup = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `dup` is ours; on failure it is closed here.
    let d = unsafe { libc::fdopendir(dup) };
    if d.is_null() {
        let e = io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(e);
    }
    let mut out = Vec::new();
    let res = loop {
        // SAFETY: errno reset, then readdir on an open stream; the entry is copied before
        // the next call.
        unsafe { *libc::__errno_location() = 0 };
        let e = unsafe { libc::readdir(d) };
        if e.is_null() {
            let err = io::Error::last_os_error();
            break if err.raw_os_error().unwrap_or(0) == 0 {
                Ok(())
            } else {
                Err(err)
            };
        }
        let name = unsafe { CStr::from_ptr((*e).d_name.as_ptr()) };
        if name != c"." && name != c".." {
            out.push(name.to_owned());
        }
    };
    // SAFETY: the stream opened above.
    unsafe { libc::closedir(d) };
    res.map(|()| out)
}

// ---------------------------------------------------------------------------------------------
// walk
// ---------------------------------------------------------------------------------------------

/// An entry waiting to be visited: its name in its (open) parent directory.
struct Pending {
    parent: Arc<OwnedFd>,
    name: CString,
    path: PathBuf,
}

/// Walks `roots` and changes (or, with `dry_run`, counts) what `opts.map` names.
pub fn remap(fs: &dyn RemapFs, roots: &[PathBuf], opts: &RemapOpts) -> RemapCounts {
    remap_with(fs, roots, opts, &|_| {})
}

/// [`remap`] with a hook called on each entry between its check and its visit (tests swap
/// files there).
fn remap_with(
    fs: &dyn RemapFs,
    roots: &[PathBuf],
    opts: &RemapOpts,
    hook: &dyn Fn(&Path),
) -> RemapCounts {
    let mut c = RemapCounts::default();
    let mounts = fs.mount_points();
    for root in distinct_roots(roots) {
        let (parent, name) = match open_root(&root) {
            Ok(x) => x,
            Err(e) => {
                push_err(&mut c, &root, &e);
                continue;
            }
        };
        let mut root_mount: Option<MountKey> = None;
        let mut stack = vec![Pending {
            parent: Arc::new(parent),
            name,
            path: root.clone(),
        }];
        while let Some(p) = stack.pop() {
            if (opts.cancelled)() {
                c.cancelled = true;
                return c;
            }
            if opts.deadline.is_some_and(|d| Instant::now() >= d) {
                c.partial = true;
                return c;
            }
            let pre = match statx_at(p.parent.as_raw_fd(), &p.name, 0) {
                Ok(m) => m,
                Err(e) => {
                    push_err(&mut c, &p.path, &e);
                    continue;
                }
            };
            c.scanned += 1;
            if c.scanned.is_multiple_of(TICK) {
                (opts.tick)(c.scanned);
            }
            let kind = kind_of(pre.mode);
            let is_root = root_mount.is_none();
            if kind == Kind::Symlink && is_root {
                push_err(
                    &mut c,
                    &p.path,
                    &io::Error::other("a symbolic link, not followed"),
                );
                continue;
            }
            let mount = fs.mount_of(&p.path, pre.mnt_id, &mounts);
            match &root_mount {
                None => root_mount = Some(mount),
                Some(r) if *r != mount => {
                    push_path(&mut c.other_fs, &p.path);
                    continue;
                }
                Some(_) => {}
            }
            hook(&p.path);
            let (fd, st) = match open_checked(p.parent.as_fd(), &p.name, &pre) {
                Ok(x) => x,
                Err(e) => {
                    push_err(&mut c, &p.path, &e);
                    continue;
                }
            };
            {
                let e = Entry {
                    path: &p.path,
                    fd: fd.as_fd(),
                    kind,
                    uid: st.uid,
                    gid: st.gid,
                    mode: st.mode,
                    ino: st.ino,
                };
                if kind == Kind::Dir && fs.readonly_subvolume(&e) {
                    push_path(&mut c.readonly, &p.path);
                    continue;
                }
                match visit(fs, &e, opts, &mut c) {
                    Ok(()) => {}
                    Err(err) if err.raw_os_error() == Some(libc::EROFS) => {
                        push_path(&mut c.readonly, &p.path);
                        continue;
                    }
                    Err(err) => push_err(&mut c, &p.path, &err),
                }
            }
            if kind == Kind::Dir {
                match read_names(fd.as_fd()) {
                    Ok(mut names) => {
                        // visited in name order (the stack pops from the end)
                        names.sort_by(|a, b| b.cmp(a));
                        let dir = Arc::new(fd);
                        for n in names {
                            let path = p.path.join(OsStr::from_bytes(n.as_bytes()));
                            stack.push(Pending {
                                parent: dir.clone(),
                                name: n,
                                path,
                            });
                        }
                    }
                    Err(e) => push_err(&mut c, &p.path, &e),
                }
            }
        }
    }
    c
}

/// One entry: owner, group, set-id bits, ACLs.
fn visit(fs: &dyn RemapFs, e: &Entry, opts: &RemapOpts, c: &mut RemapCounts) -> io::Result<()> {
    let uid = opts.map.new_uid(e.uid);
    let gid = opts.map.new_gid(e.gid);
    if uid.is_some() || gid.is_some() {
        if !opts.dry_run {
            fs.chown(e, uid, gid)?;
            // the kernel clears the set-id bits of regular files on chown, never those of
            // directories (which are not touched)
            if opts.keep_setid && e.kind == Kind::File && e.mode & 0o6000 != 0 {
                fs.chmod(e, e.mode & 0o7777)?;
            }
        }
        if uid.is_some() {
            c.owners += 1;
        }
        if gid.is_some() {
            c.groups += 1;
        }
    }
    if opts.acls && matches!(e.kind, Kind::Dir | Kind::File) {
        let kinds: &[bool] = if e.kind == Kind::Dir { &[false, true] } else { &[false] };
        for &default in kinds {
            if let Some(raw) = fs.get_acl(e, default)?
                && let Some(r) = remap_acl(&raw, &opts.map)
            {
                if !opts.dry_run {
                    fs.set_acl(e, default, &r.raw)?;
                }
                c.acl_entries += u64::from(r.entries);
                if r.conflicts > 0 {
                    c.acl_conflicts += u64::from(r.conflicts);
                    if !c.acl_conflict_paths.iter().any(|x| *x == e.path.display().to_string()) {
                        push_path(&mut c.acl_conflict_paths, e.path);
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// host
// ---------------------------------------------------------------------------------------------

fn acl_name(default: bool) -> &'static str {
    if default { ACL_DEFAULT } else { ACL_ACCESS }
}

/// Reads an ACL xattr of an open file.
pub fn host_get_acl(fd: BorrowedFd, default: bool) -> io::Result<Option<Vec<u8>>> {
    let name = acl_name(default);
    let mut buf = vec![0u8; 512];
    loop {
        // SAFETY: valid C string and a buffer of the given length.
        let n = unsafe {
            libc::fgetxattr(
                fd.as_raw_fd(),
                name.as_ptr().cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if n >= 0 {
            buf.truncate(n as usize);
            return Ok(Some(buf));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ENODATA) | Some(libc::EOPNOTSUPP) => return Ok(None),
            Some(libc::ERANGE) if buf.len() < 1 << 20 => buf.resize(buf.len() * 4, 0),
            _ => return Err(e),
        }
    }
}

pub fn host_set_acl(fd: BorrowedFd, default: bool, value: &[u8]) -> io::Result<()> {
    let name = acl_name(default);
    // SAFETY: valid C string and a buffer of the given length.
    let r = unsafe {
        libc::fsetxattr(
            fd.as_raw_fd(),
            name.as_ptr().cast(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn host_chown(fd: BorrowedFd, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
    // SAFETY: empty path on an open descriptor; u32::MAX (-1) leaves an id as it is.
    let r = unsafe {
        libc::fchownat(
            fd.as_raw_fd(),
            c"".as_ptr(),
            uid.unwrap_or(u32::MAX),
            gid.unwrap_or(u32::MAX),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn host_fchmod(e: &Entry, mode: u32) -> io::Result<()> {
    if e.kind != Kind::File {
        return Err(io::Error::other("only regular files are chmod-ed"));
    }
    // SAFETY: an open regular file.
    if unsafe { libc::fchmod(e.fd.as_raw_fd(), mode) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;
/// `_IOR(0x94, 25, u64)`.
const BTRFS_IOC_SUBVOL_GETFLAGS: libc::c_ulong = 0x8008_9419;
const BTRFS_SUBVOL_RDONLY: u64 = 1 << 1;

/// A read-only btrfs subvolume: inode 256 on btrfs and the `RDONLY` subvolume flag.
pub fn host_readonly_subvolume(e: &Entry) -> bool {
    if e.ino != 256 || e.kind != Kind::Dir {
        return false;
    }
    // SAFETY: fstatfs on an open directory into a zeroed struct.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(e.fd.as_raw_fd(), &mut st) } != 0
        || st.f_type as i64 != BTRFS_SUPER_MAGIC
    {
        return false;
    }
    let mut flags: u64 = 0;
    // SAFETY: the ioctl writes one u64 into `flags`.
    let r = unsafe { libc::ioctl(e.fd.as_raw_fd(), BTRFS_IOC_SUBVOL_GETFLAGS as _, &mut flags) };
    r == 0 && flags & BTRFS_SUBVOL_RDONLY != 0
}

/// Mount points of `/proc/self/mountinfo` (field 5, octal escapes decoded).
pub fn parse_mount_points(text: &str) -> BTreeSet<PathBuf> {
    text.lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|mp| {
            let b = mp.as_bytes();
            let mut out = Vec::with_capacity(b.len());
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'\\'
                    && i + 3 < b.len()
                    && let Ok(v) = u8::from_str_radix(&mp[i + 1..i + 4], 8)
                {
                    out.push(v);
                    i += 4;
                    continue;
                }
                out.push(b[i]);
                i += 1;
            }
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(out))
        })
        .collect()
}

fn host_mount_points() -> BTreeSet<PathBuf> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|t| parse_mount_points(&t))
        .unwrap_or_default()
}

/// The real system.
pub struct HostRemap;

impl RemapFs for HostRemap {
    fn chown(&self, e: &Entry, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
        host_chown(e.fd, uid, gid)
    }

    fn chmod(&self, e: &Entry, mode: u32) -> io::Result<()> {
        host_fchmod(e, mode)
    }

    fn get_acl(&self, e: &Entry, default: bool) -> io::Result<Option<Vec<u8>>> {
        host_get_acl(e.fd, default)
    }

    fn set_acl(&self, e: &Entry, default: bool, value: &[u8]) -> io::Result<()> {
        host_set_acl(e.fd, default, value)
    }

    fn readonly_subvolume(&self, e: &Entry) -> bool {
        host_readonly_subvolume(e)
    }

    fn mount_points(&self) -> BTreeSet<PathBuf> {
        host_mount_points()
    }
}

/// `rynasd --dry-system`: reads the host, changes nothing.
pub struct DryRemap;

impl RemapFs for DryRemap {
    fn chown(&self, _: &Entry, _: Option<u32>, _: Option<u32>) -> io::Result<()> {
        Ok(())
    }

    fn chmod(&self, _: &Entry, _: u32) -> io::Result<()> {
        Ok(())
    }

    fn get_acl(&self, e: &Entry, default: bool) -> io::Result<Option<Vec<u8>>> {
        host_get_acl(e.fd, default)
    }

    fn set_acl(&self, _: &Entry, _: bool, _: &[u8]) -> io::Result<()> {
        Ok(())
    }

    fn readonly_subvolume(&self, e: &Entry) -> bool {
        host_readonly_subvolume(e)
    }

    fn mount_points(&self) -> BTreeSet<PathBuf> {
        host_mount_points()
    }
}

/// Plain text of the counts (task logs).
pub fn counts_text(c: &RemapCounts) -> String {
    let mut s = format!(
        "{} entries checked: {} owners, {} groups, {} ACL entries",
        c.scanned, c.owners, c.groups, c.acl_entries
    );
    if c.acl_conflicts > 0 {
        s.push_str(&format!(
            "; {} ACL entries of the old id dropped because the new id had its own (kept): {}",
            c.acl_conflicts,
            c.acl_conflict_paths.join(", ")
        ));
    }
    if !c.readonly.is_empty() {
        s.push_str(&format!(
            "; read-only (snapshots, skipped): {}",
            c.readonly.join(", ")
        ));
    }
    if !c.other_fs.is_empty() {
        s.push_str(&format!(
            "; other file systems (skipped): {}",
            c.other_fs.join(", ")
        ));
    }
    if c.errors > 0 {
        s.push_str(&format!("; {} errors", c.errors));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::Mutex;

    type Chown = (PathBuf, Option<u32>, Option<u32>);

    /// Records the changes instead of making them; reads the real temp dir.
    #[derive(Default)]
    struct Fake {
        chowns: Mutex<Vec<Chown>>,
        chmods: Mutex<Vec<(PathBuf, u32)>>,
        acls: Mutex<Vec<(PathBuf, bool, Vec<u8>)>>,
        /// Fake ACLs (path, default) → raw.
        stored: Mutex<Vec<(PathBuf, bool, Vec<u8>)>>,
        readonly: Vec<PathBuf>,
        mounts: BTreeSet<PathBuf>,
        erofs: Vec<PathBuf>,
        real_acls: bool,
    }

    impl RemapFs for Fake {
        fn chown(&self, e: &Entry, u: Option<u32>, g: Option<u32>) -> io::Result<()> {
            if self.erofs.iter().any(|x| x == e.path) {
                return Err(io::Error::from_raw_os_error(libc::EROFS));
            }
            self.chowns.lock().unwrap().push((e.path.into(), u, g));
            Ok(())
        }
        fn chmod(&self, e: &Entry, m: u32) -> io::Result<()> {
            self.chmods.lock().unwrap().push((e.path.into(), m));
            Ok(())
        }
        fn get_acl(&self, e: &Entry, d: bool) -> io::Result<Option<Vec<u8>>> {
            if self.real_acls {
                return host_get_acl(e.fd, d);
            }
            Ok(self
                .stored
                .lock()
                .unwrap()
                .iter()
                .find(|(x, dd, _)| x == e.path && *dd == d)
                .map(|(_, _, r)| r.clone()))
        }
        fn set_acl(&self, e: &Entry, d: bool, v: &[u8]) -> io::Result<()> {
            if self.real_acls {
                host_set_acl(e.fd, d, v)?;
            }
            self.acls.lock().unwrap().push((e.path.into(), d, v.to_vec()));
            Ok(())
        }
        fn readonly_subvolume(&self, e: &Entry) -> bool {
            self.readonly.iter().any(|x| x == e.path)
        }
        fn mount_points(&self) -> BTreeSet<PathBuf> {
            self.mounts.clone()
        }
        fn mount_of(&self, p: &Path, _: Option<u64>, m: &BTreeSet<PathBuf>) -> MountKey {
            MountKey::Point(mount_point_of(p, m))
        }
    }

    fn me() -> (u32, u32) {
        // SAFETY: plain getters.
        unsafe { (libc::getuid(), libc::getgid()) }
    }

    fn opts(map: IdMap, dry: bool) -> RemapOpts<'static> {
        RemapOpts {
            map,
            acls: true,
            keep_setid: true,
            dry_run: dry,
            deadline: None,
            cancelled: &|| false,
            tick: &|_| {},
        }
    }

    fn acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
        let mut v = ACL_VERSION.to_le_bytes().to_vec();
        for (t, p, id) in entries {
            v.extend(t.to_le_bytes());
            v.extend(p.to_le_bytes());
            v.extend(id.to_le_bytes());
        }
        v
    }

    fn mode(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().mode() & 0o7777
    }

    /// root/{a, sub/{b}, snap/{c}, mnt/{d}, link -> /etc}
    fn tree() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        std::fs::write(r.join("a"), "a").unwrap();
        for d in ["sub", "snap", "mnt"] {
            std::fs::create_dir(r.join(d)).unwrap();
        }
        std::fs::write(r.join("sub/b"), "b").unwrap();
        std::fs::write(r.join("snap/c"), "c").unwrap();
        std::fs::write(r.join("mnt/d"), "d").unwrap();
        std::os::unix::fs::symlink("/etc", r.join("link")).unwrap();
        t
    }

    #[test]
    fn walks_without_following_links_and_skips_snapshots_and_mounts() {
        let t = tree();
        let r = t.path().to_path_buf();
        let (uid, gid) = me();
        let fake = Fake {
            readonly: vec![r.join("snap")],
            mounts: [r.join("mnt"), r.clone()].into(),
            ..Default::default()
        };
        let map = IdMap {
            uid: Some((uid, uid + 1)),
            gid: Some((gid, gid + 1)),
        };
        let c = remap(&fake, std::slice::from_ref(&r), &opts(map, false));
        let chowned: Vec<PathBuf> = fake
            .chowns
            .lock()
            .unwrap()
            .iter()
            .map(|x| x.0.strip_prefix(&r).unwrap().to_path_buf())
            .collect();
        // the root (a mount point itself, but where the walk starts), a, the link itself
        // (not its target), sub and sub/b; not snap, not mnt, nothing under /etc
        let want: Vec<PathBuf> = ["", "a", "link", "sub", "sub/b"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(chowned, want);
        assert_eq!(c.owners, 5);
        assert_eq!(c.groups, 5);
        assert_eq!(c.readonly, vec![r.join("snap").display().to_string()]);
        assert_eq!(c.other_fs, vec![r.join("mnt").display().to_string()]);
        assert!(!c.partial && !c.cancelled && c.errors == 0, "{:?}", c.error_samples);
        assert!(
            fake.chowns
                .lock()
                .unwrap()
                .iter()
                .all(|(_, u, g)| *u == Some(uid + 1) && *g == Some(gid + 1))
        );
    }

    #[test]
    fn host_mount_ids_keep_the_walk_on_its_mount() {
        // the temp dir is on one mount: everything is walked with the real mount ids
        let t = tree();
        let r = t.path().to_path_buf();
        struct HostMounts(Fake);
        impl RemapFs for HostMounts {
            fn chown(&self, e: &Entry, u: Option<u32>, g: Option<u32>) -> io::Result<()> {
                self.0.chown(e, u, g)
            }
            fn chmod(&self, e: &Entry, m: u32) -> io::Result<()> {
                self.0.chmod(e, m)
            }
            fn get_acl(&self, _: &Entry, _: bool) -> io::Result<Option<Vec<u8>>> {
                Ok(None)
            }
            fn set_acl(&self, _: &Entry, _: bool, _: &[u8]) -> io::Result<()> {
                Ok(())
            }
            fn readonly_subvolume(&self, _: &Entry) -> bool {
                false
            }
            fn mount_points(&self) -> BTreeSet<PathBuf> {
                host_mount_points()
            }
        }
        let (uid, _) = me();
        let fs = HostMounts(Fake::default());
        let map = IdMap {
            uid: Some((uid, 4242)),
            gid: None,
        };
        let c = remap(&fs, std::slice::from_ref(&r), &opts(map, false));
        assert_eq!(c.owners, 9, "{c:?}");
        assert!(c.other_fs.is_empty());
        // /proc is another mount than / : a root on / does not enter it
        let k1 = fs.mount_of(Path::new("/"), statx_at(libc::AT_FDCWD, c"/", 0).unwrap().mnt_id, &BTreeSet::new());
        let k2 = fs.mount_of(
            Path::new("/proc"),
            statx_at(libc::AT_FDCWD, c"/proc", 0).unwrap().mnt_id,
            &BTreeSet::new(),
        );
        assert_ne!(k1, k2);
    }

    #[test]
    fn dry_run_counts_without_changing() {
        let t = tree();
        let r = t.path().to_path_buf();
        let (uid, _) = me();
        let fake = Fake::default();
        let map = IdMap {
            uid: Some((uid, 4242)),
            gid: None,
        };
        let c = remap(&fake, std::slice::from_ref(&r), &opts(map, true));
        assert!(fake.chowns.lock().unwrap().is_empty());
        assert_eq!(c.owners, 9); // root, a, link, sub, b, snap, c, mnt, d
        assert_eq!(c.groups, 0);
        // another id: nothing to do
        let other = IdMap {
            uid: Some((uid + 7, 4242)),
            gid: None,
        };
        let c = remap(&fake, &[r], &opts(other, true));
        assert_eq!(c.owners, 0);
        assert_eq!(c.scanned, 9);
    }

    #[test]
    fn real_chown_to_own_ids_and_setid_bits_kept_on_files_only() {
        let t = tree();
        let r = t.path().to_path_buf();
        let f = r.join("a");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o4755)).unwrap();
        std::fs::set_permissions(r.join("sub"), std::fs::Permissions::from_mode(0o2775)).unwrap();
        let (uid, gid) = me();
        let map = IdMap {
            uid: Some((uid, uid)),
            gid: Some((gid, gid)),
        };
        let c = remap(&HostRemap, std::slice::from_ref(&r), &opts(map, false));
        assert_eq!(c.owners, 9);
        assert_eq!(c.errors, 0, "{:?}", c.error_samples);
        let m = std::fs::metadata(&f).unwrap();
        assert_eq!((m.uid(), m.gid()), (uid, gid));
        assert_eq!(mode(&f), 0o4755);
        // the kernel does not clear a directory's set-group-ID bit
        assert_eq!(mode(&r.join("sub")), 0o2775);
        // the link was changed itself, /etc not touched
        assert_eq!(std::fs::symlink_metadata(r.join("link")).unwrap().uid(), uid);
    }

    #[test]
    fn take_over_does_not_put_set_id_bits_back() {
        let t = tree();
        let r = t.path().to_path_buf();
        std::fs::set_permissions(r.join("a"), std::fs::Permissions::from_mode(0o4755)).unwrap();
        std::fs::set_permissions(r.join("sub"), std::fs::Permissions::from_mode(0o2775)).unwrap();
        let (uid, _) = me();
        let map = IdMap {
            uid: Some((uid, uid)),
            gid: None,
        };
        let fake = Fake::default();
        let o = RemapOpts {
            keep_setid: false,
            ..opts(map, false)
        };
        remap(&fake, std::slice::from_ref(&r), &o);
        assert!(fake.chmods.lock().unwrap().is_empty());
        // with keep_setid only the regular file, never the directory
        remap(&fake, std::slice::from_ref(&r), &opts(map, false));
        let chmods = fake.chmods.lock().unwrap();
        assert_eq!(*chmods, vec![(r.join("a"), 0o4755)]);
        // the real thing: chown clears the bit and it stays cleared
        drop(chmods);
        remap(&HostRemap, std::slice::from_ref(&r), &o);
        assert_eq!(mode(&r.join("a")), 0o755);
        assert_eq!(mode(&r.join("sub")), 0o2775);
    }

    /// Changes the real file system (own ids) and swaps the entry for a link to `target`
    /// right after the chown, before the set-id bits go back.
    struct Swapper {
        swap: PathBuf,
        target: PathBuf,
        chowned: Mutex<Vec<PathBuf>>,
    }

    impl RemapFs for Swapper {
        fn chown(&self, e: &Entry, u: Option<u32>, g: Option<u32>) -> io::Result<()> {
            HostRemap.chown(e, u, g)?;
            self.chowned.lock().unwrap().push(e.path.into());
            if e.path == self.swap {
                std::fs::rename(e.path, e.path.with_extension("orig")).unwrap();
                std::os::unix::fs::symlink(&self.target, e.path).unwrap();
            }
            Ok(())
        }
        fn chmod(&self, e: &Entry, m: u32) -> io::Result<()> {
            HostRemap.chmod(e, m)
        }
        fn get_acl(&self, e: &Entry, d: bool) -> io::Result<Option<Vec<u8>>> {
            HostRemap.get_acl(e, d)
        }
        fn set_acl(&self, e: &Entry, d: bool, v: &[u8]) -> io::Result<()> {
            HostRemap.set_acl(e, d, v)
        }
        fn readonly_subvolume(&self, _: &Entry) -> bool {
            false
        }
        fn mount_points(&self) -> BTreeSet<PathBuf> {
            BTreeSet::new()
        }
    }

    #[test]
    fn symlink_swap_between_chown_and_chmod_changes_only_the_checked_file() {
        let t = tree();
        let r = t.path().join("share");
        std::fs::create_dir(&r).unwrap();
        let outside = t.path().join("shadow");
        std::fs::write(&outside, "secret").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o640)).unwrap();
        let f = r.join("tool");
        std::fs::write(&f, "x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o4755)).unwrap();
        let (uid, _) = me();
        let fs = Swapper {
            swap: f.clone(),
            target: outside.clone(),
            chowned: Mutex::default(),
        };
        let map = IdMap {
            uid: Some((uid, uid)),
            gid: None,
        };
        let c = remap(&fs, std::slice::from_ref(&r), &opts(map, false));
        assert_eq!(c.errors, 0, "{:?}", c.error_samples);
        // the target of the link keeps its mode; the file that was checked got its bit back
        assert_eq!(mode(&outside), 0o640);
        assert_eq!(mode(&f.with_extension("orig")), 0o4755);
    }

    #[test]
    fn directory_swapped_for_a_link_is_not_followed_nor_chmod_ed() {
        let t = tree();
        let r = t.path().join("share");
        std::fs::create_dir_all(r.join("d/inner")).unwrap();
        std::fs::write(r.join("d/inner/mine"), "x").unwrap();
        std::fs::set_permissions(r.join("d"), std::fs::Permissions::from_mode(0o2775)).unwrap();
        let outside = t.path().join("etc");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("shadow"), "secret").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (uid, _) = me();
        let fs = Swapper {
            swap: r.join("d"),
            target: outside.clone(),
            chowned: Mutex::default(),
        };
        let map = IdMap {
            uid: Some((uid, uid)),
            gid: None,
        };
        let c = remap(&fs, std::slice::from_ref(&r), &opts(map, false));
        assert_eq!(c.errors, 0, "{:?}", c.error_samples);
        assert_eq!(mode(&outside), 0o755);
        let chowned = fs.chowned.lock().unwrap();
        // the walk went on in the directory it opened (now d.orig), not in the link's target
        assert!(chowned.iter().any(|p| p.ends_with("d/inner/mine")), "{chowned:?}");
        assert!(!chowned.iter().any(|p| p.ends_with("shadow")), "{chowned:?}");
    }

    #[test]
    fn entry_swapped_between_check_and_open_is_refused() {
        let t = tree();
        let r = t.path().to_path_buf();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("evil"), "x").unwrap();
        let sub = r.join("sub");
        let target = outside.path().to_path_buf();
        let hook = |p: &Path| {
            if p == sub {
                std::fs::rename(&sub, r.join("sub.orig")).unwrap();
                std::os::unix::fs::symlink(&target, &sub).unwrap();
            }
        };
        let (uid, _) = me();
        let fake = Fake::default();
        let map = IdMap {
            uid: Some((uid, 4242)),
            gid: None,
        };
        let c = remap_with(&fake, std::slice::from_ref(&r), &opts(map, false), &hook);
        assert_eq!(c.errors, 1);
        assert!(c.error_samples[0].contains("changed while the walk ran"), "{:?}", c.error_samples);
        let chowns = fake.chowns.lock().unwrap();
        assert!(!chowns.iter().any(|x| x.0.ends_with("evil")), "{chowns:?}");
        assert!(!chowns.iter().any(|x| x.0 == sub));
    }

    #[test]
    fn intermediate_link_in_a_root_is_refused() {
        let t = tree();
        let r = t.path().to_path_buf();
        std::os::unix::fs::symlink(r.join("sub"), r.join("via")).unwrap();
        let (uid, _) = me();
        let fake = Fake::default();
        let map = IdMap {
            uid: Some((uid, 4242)),
            gid: None,
        };
        let c = remap(&fake, &[r.join("via/b")], &opts(map, false));
        assert_eq!(c.errors, 1);
        assert!(c.error_samples[0].contains("symbolic link in the path"), "{:?}", c.error_samples);
        assert!(fake.chowns.lock().unwrap().is_empty());
        // the root itself a link
        let c = remap(&fake, &[r.join("via")], &opts(map, false));
        assert_eq!(c.errors, 1);
        assert!(fake.chowns.lock().unwrap().is_empty());
    }

    #[test]
    fn erofs_skips_the_subtree() {
        let t = tree();
        let r = t.path().to_path_buf();
        let (uid, _) = me();
        let fake = Fake {
            erofs: vec![r.join("sub")],
            ..Default::default()
        };
        let map = IdMap {
            uid: Some((uid, 4242)),
            gid: None,
        };
        let c = remap(&fake, std::slice::from_ref(&r), &opts(map, false));
        assert_eq!(c.readonly, vec![r.join("sub").display().to_string()]);
        assert!(
            !fake
                .chowns
                .lock()
                .unwrap()
                .iter()
                .any(|x| x.0.starts_with(r.join("sub")))
        );
    }

    #[test]
    fn acl_entries_are_rewritten() {
        let raw = acl(&[
            (0x01, 6, u32::MAX),
            (ACL_USER, 6, 1026),
            (ACL_USER, 4, 1027),
            (0x04, 4, u32::MAX),
            (ACL_GROUP, 5, 1026),
            (0x10, 7, u32::MAX),
            (0x20, 5, u32::MAX),
        ]);
        let map = IdMap {
            uid: Some((1026, 1000)),
            gid: None,
        };
        let r = remap_acl(&raw, &map).unwrap();
        assert_eq!((r.entries, r.conflicts), (1, 0));
        assert_eq!(
            r.raw,
            acl(&[
                (0x01, 6, u32::MAX),
                (ACL_USER, 6, 1000),
                (ACL_USER, 4, 1027),
                (0x04, 4, u32::MAX),
                (ACL_GROUP, 5, 1026),
                (0x10, 7, u32::MAX),
                (0x20, 5, u32::MAX),
            ])
        );
        // ids moved past another entry come out sorted
        let up = IdMap {
            uid: Some((1026, 2000)),
            gid: None,
        };
        let r = remap_acl(&raw, &up).unwrap();
        assert_eq!(
            r.raw,
            acl(&[
                (0x01, 6, u32::MAX),
                (ACL_USER, 4, 1027),
                (ACL_USER, 6, 2000),
                (0x04, 4, u32::MAX),
                (ACL_GROUP, 5, 1026),
                (0x10, 7, u32::MAX),
                (0x20, 5, u32::MAX),
            ])
        );
        let gmap = IdMap {
            uid: None,
            gid: Some((1026, 2000)),
        };
        assert_eq!(remap_acl(&raw, &gmap).unwrap().entries, 1);
        assert!(remap_acl(&raw, &IdMap::default()).is_none());
        assert!(remap_acl(&raw[..7], &map).is_none());
        assert!(remap_acl(&[1, 0, 0, 0], &map).is_none());
    }

    #[test]
    fn acl_conflict_keeps_the_new_ids_entry() {
        let raw = acl(&[
            (0x01, 6, u32::MAX),
            (ACL_USER, 7, 1026),
            (ACL_USER, 4, 1027),
            (0x04, 4, u32::MAX),
            (0x10, 7, u32::MAX),
            (0x20, 0, u32::MAX),
        ]);
        let map = IdMap {
            uid: Some((1026, 1027)),
            gid: None,
        };
        let r = remap_acl(&raw, &map).unwrap();
        assert_eq!((r.entries, r.conflicts), (1, 1));
        assert_eq!(
            r.raw,
            acl(&[
                (0x01, 6, u32::MAX),
                (ACL_USER, 4, 1027),
                (0x04, 4, u32::MAX),
                (0x10, 7, u32::MAX),
                (0x20, 0, u32::MAX),
            ])
        );
        // reported by the walk
        let t = tree();
        let root = t.path().to_path_buf();
        let fake = Fake::default();
        fake.stored.lock().unwrap().push((root.join("a"), false, raw));
        let c = remap(&fake, std::slice::from_ref(&root), &opts(map, false));
        assert_eq!(c.acl_conflicts, 1);
        assert_eq!(c.acl_conflict_paths, vec![root.join("a").display().to_string()]);
        assert!(counts_text(&c).contains("dropped"));
    }

    #[test]
    fn acls_in_the_walk_fake_and_default_acl_on_dirs() {
        let t = tree();
        let r = t.path().to_path_buf();
        let fake = Fake::default();
        fake.stored.lock().unwrap().extend([
            (r.join("a"), false, acl(&[(ACL_USER, 6, 1026)])),
            (r.join("sub"), true, acl(&[(ACL_USER, 7, 1026)])),
            (r.join("snap/c"), false, acl(&[(ACL_USER, 6, 1026)])),
        ]);
        let fake = Fake {
            readonly: vec![r.join("snap")],
            ..fake
        };
        let map = IdMap {
            uid: Some((1026, 1000)),
            gid: None,
        };
        let c = remap(&fake, std::slice::from_ref(&r), &opts(map, false));
        assert_eq!(c.acl_entries, 2);
        let set = fake.acls.lock().unwrap();
        assert_eq!(set.len(), 2);
        assert!(set.iter().any(|(p, d, v)| *p == r.join("sub")
            && *d
            && *v == acl(&[(ACL_USER, 7, 1000)])));
    }

    /// With `setfacl` and a file system with ACLs: a real entry for uid 54321 becomes 54322.
    #[test]
    fn real_acl_entry_when_setfacl_is_available() {
        let t = tree();
        let f = t.path().join("a");
        let ok = std::process::Command::new("setfacl")
            .args(["-m", "u:54321:r"])
            .arg(&f)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("setfacl not available or no ACL support: skipped");
            return;
        }
        let fake = Fake {
            real_acls: true,
            ..Default::default()
        };
        let map = IdMap {
            uid: Some((54321, 54322)),
            gid: None,
        };
        let c = remap(&fake, std::slice::from_ref(&f), &opts(map, false));
        assert_eq!(c.acl_entries, 1);
        let out = std::process::Command::new("getfacl")
            .args(["-n", "-c"])
            .arg(&f)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("user:54322:r--"), "{text}");
        assert!(!text.contains("54321"), "{text}");
    }

    #[test]
    fn cancel_and_deadline() {
        let t = tree();
        let r = t.path().to_path_buf();
        let fake = Fake::default();
        let o = RemapOpts {
            cancelled: &|| true,
            ..opts(IdMap::default(), true)
        };
        assert!(remap(&fake, std::slice::from_ref(&r), &o).cancelled);
        let o = RemapOpts {
            deadline: Some(Instant::now()),
            ..opts(IdMap::default(), true)
        };
        let c = remap(&fake, &[r], &o);
        assert!(c.partial && c.scanned == 0);
    }

    #[test]
    fn nested_roots_once_and_mountinfo() {
        let v = distinct_roots(&[
            PathBuf::from("/v1/a"),
            PathBuf::from("/v1"),
            PathBuf::from("/v2"),
            PathBuf::from("/v1"),
        ]);
        assert_eq!(v, vec![PathBuf::from("/v1"), PathBuf::from("/v2")]);
        let m = parse_mount_points(
            "22 1 0:21 / / rw - btrfs /dev/sda1 rw\n30 22 0:30 / /volume\\0401 rw - btrfs /dev/sdb rw\n",
        );
        assert!(m.contains(Path::new("/")) && m.contains(Path::new("/volume 1")));
        assert_eq!(
            mount_point_of(Path::new("/volume 1/a/b"), &m),
            Some(PathBuf::from("/volume 1"))
        );
        assert_eq!(mount_point_of(Path::new("/srv"), &m), Some(PathBuf::from("/")));
    }
}
