//! Ownership remap: files owned by one uid (or gid) are given to another, on chosen folders.
//!
//! Used after a UID/GID change (`usermod -u` only fixes the home directory and the mail
//! spool) and after a migration where data was copied with numeric owners that no local
//! account has (e.g. 1026 from another NAS). The walk:
//! - never follows symbolic links (`lchown`, `symlink_metadata`), so a link pointing out of
//!   the chosen folders cannot pull other files in;
//! - stays on the filesystems it starts on: a directory that is a mount point is skipped
//!   and reported (btrfs subvolumes are not mount points and are walked);
//! - skips read-only btrfs subvolumes (snapshots) and anything answering `EROFS`, and
//!   reports them: snapshots keep the old ids, which is what a snapshot is for;
//! - rewrites POSIX ACL entries naming the old id (`system.posix_acl_access` and, on
//!   directories, `system.posix_acl_default`), read and written as raw xattrs;
//! - keeps the set-user-ID/set-group-ID bits that the kernel clears on `chown`;
//! - is cancellable, reports progress and can stop at a deadline (dry-run counts for the
//!   plan, marked partial).
//!
//! All file system access goes through [`RemapFs`]: [`HostRemap`] on a real system,
//! [`DryRemap`] for `rynasd --dry-system` (reads the host, writes nothing), fakes in tests.

use std::collections::BTreeSet;
use std::fs::Metadata;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
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

/// File system access of the walk.
pub trait RemapFs: Send + Sync {
    /// `lchown(2)`; `None` leaves that id as it is.
    fn lchown(&self, path: &Path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()>;
    /// `chmod(2)` of a file that is not a symbolic link (set-id bits back after `chown`).
    fn chmod(&self, path: &Path, mode: u32) -> io::Result<()>;
    /// The raw POSIX ACL xattr (`default`: the default ACL of a directory); `None` when the
    /// file has none or the file system does not support ACLs.
    fn get_acl(&self, path: &Path, default: bool) -> io::Result<Option<Vec<u8>>>;
    fn set_acl(&self, path: &Path, default: bool, value: &[u8]) -> io::Result<()>;
    /// A read-only btrfs subvolume (a snapshot), checked on every directory.
    fn readonly_subvolume(&self, path: &Path, meta: &Metadata) -> bool;
    /// Every mount point of the system (directories the walk does not enter).
    fn mount_points(&self) -> BTreeSet<PathBuf>;
}

const ACL_ACCESS: &str = "system.posix_acl_access\0";
const ACL_DEFAULT: &str = "system.posix_acl_default\0";
const ACL_VERSION: u32 = 2;
const ACL_USER: u16 = 0x02;
const ACL_GROUP: u16 = 0x08;

/// Rewrites the entries of a raw POSIX ACL xattr (`posix_acl_xattr_header` + entries of tag,
/// perm, id, little endian) that name an id of `map`. `None`: nothing to change (or not a
/// format this code knows, which is left alone).
pub fn remap_acl(raw: &[u8], map: &IdMap) -> Option<(Vec<u8>, u32)> {
    if raw.len() < 4 || !(raw.len() - 4).is_multiple_of(8) {
        return None;
    }
    if u32::from_le_bytes(raw[0..4].try_into().ok()?) != ACL_VERSION {
        return None;
    }
    let mut out = raw.to_vec();
    let mut n = 0;
    for e in out[4..].as_chunks_mut::<8>().0 {
        let tag = u16::from_le_bytes([e[0], e[1]]);
        let id = u32::from_le_bytes([e[4], e[5], e[6], e[7]]);
        let new = match tag {
            ACL_USER => map.new_uid(id),
            ACL_GROUP => map.new_gid(id),
            _ => None,
        };
        if let Some(t) = new {
            e[4..8].copy_from_slice(&t.to_le_bytes());
            n += 1;
        }
    }
    (n > 0).then_some((out, n))
}

/// Options of a walk.
pub struct RemapOpts<'a> {
    pub map: IdMap,
    /// Rewrite ACL entries too.
    pub acls: bool,
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
/// Reported paths of each kind (read-only, other file systems).
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

/// Walks `roots` and changes (or, with `dry_run`, counts) what `opts.map` names.
pub fn remap(fs: &dyn RemapFs, roots: &[PathBuf], opts: &RemapOpts) -> RemapCounts {
    let mut c = RemapCounts::default();
    let mounts = fs.mount_points();
    for root in distinct_roots(roots) {
        let mut stack = vec![root.clone()];
        while let Some(p) = stack.pop() {
            if (opts.cancelled)() {
                c.cancelled = true;
                return c;
            }
            if opts.deadline.is_some_and(|d| Instant::now() >= d) {
                c.partial = true;
                return c;
            }
            let meta = match std::fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(e) => {
                    push_err(&mut c, &p, &e);
                    continue;
                }
            };
            c.scanned += 1;
            if c.scanned.is_multiple_of(TICK) {
                (opts.tick)(c.scanned);
            }
            let ft = meta.file_type();
            if ft.is_symlink() && p == root {
                push_err(
                    &mut c,
                    &p,
                    &io::Error::other("a symbolic link, not followed"),
                );
                continue;
            }
            let is_dir = ft.is_dir();
            if is_dir && p != root && mounts.contains(&p) {
                push_path(&mut c.other_fs, &p);
                continue;
            }
            if is_dir && fs.readonly_subvolume(&p, &meta) {
                push_path(&mut c.readonly, &p);
                continue;
            }
            match visit(fs, &p, &meta, opts, &mut c) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::EROFS) => {
                    push_path(&mut c.readonly, &p);
                    continue;
                }
                Err(e) => push_err(&mut c, &p, &e),
            }
            if is_dir {
                match std::fs::read_dir(&p) {
                    Ok(rd) => {
                        let mut kids: Vec<PathBuf> =
                            rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
                        // visited in name order (the stack pops from the end)
                        kids.sort_by(|a, b| b.cmp(a));
                        stack.extend(kids);
                    }
                    Err(e) => push_err(&mut c, &p, &e),
                }
            }
        }
    }
    c
}

/// One entry: owner, group, set-id bits, ACLs.
fn visit(
    fs: &dyn RemapFs,
    p: &Path,
    meta: &Metadata,
    opts: &RemapOpts,
    c: &mut RemapCounts,
) -> io::Result<()> {
    let uid = opts.map.new_uid(meta.uid());
    let gid = opts.map.new_gid(meta.gid());
    let symlink = meta.file_type().is_symlink();
    if uid.is_some() || gid.is_some() {
        if !opts.dry_run {
            fs.lchown(p, uid, gid)?;
            let mode = meta.permissions().mode();
            if !symlink && mode & 0o6000 != 0 {
                fs.chmod(p, mode & 0o7777)?;
            }
        }
        if uid.is_some() {
            c.owners += 1;
        }
        if gid.is_some() {
            c.groups += 1;
        }
    }
    if opts.acls && !symlink {
        let kinds: &[bool] = if meta.is_dir() { &[false, true] } else { &[false] };
        for &default in kinds {
            if let Some(raw) = fs.get_acl(p, default)?
                && let Some((new, n)) = remap_acl(&raw, &opts.map)
            {
                if !opts.dry_run {
                    fs.set_acl(p, default, &new)?;
                }
                c.acl_entries += n as u64;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// host
// ---------------------------------------------------------------------------------------------

fn cpath(p: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other)
}

fn acl_name(default: bool) -> &'static str {
    if default { ACL_DEFAULT } else { ACL_ACCESS }
}

/// Reads an ACL xattr without following symbolic links.
pub fn host_get_acl(path: &Path, default: bool) -> io::Result<Option<Vec<u8>>> {
    let c = cpath(path)?;
    let name = acl_name(default);
    let mut buf = vec![0u8; 512];
    loop {
        // SAFETY: valid C strings and a buffer of the given length.
        let n = unsafe {
            libc::lgetxattr(
                c.as_ptr(),
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

pub fn host_set_acl(path: &Path, default: bool, value: &[u8]) -> io::Result<()> {
    let c = cpath(path)?;
    let name = acl_name(default);
    // SAFETY: valid C strings and a buffer of the given length.
    let r = unsafe {
        libc::lsetxattr(
            c.as_ptr(),
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

const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;
/// `_IOR(0x94, 25, u64)`.
const BTRFS_IOC_SUBVOL_GETFLAGS: libc::c_ulong = 0x8008_9419;
const BTRFS_SUBVOL_RDONLY: u64 = 1 << 1;

/// A read-only btrfs subvolume: inode 256 on btrfs and the `RDONLY` subvolume flag.
pub fn host_readonly_subvolume(path: &Path, meta: &Metadata) -> bool {
    if meta.ino() != 256 {
        return false;
    }
    let Ok(c) = cpath(path) else { return false };
    // SAFETY: statfs on a valid C string into a zeroed struct.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 || st.f_type as i64 != BTRFS_SUPER_MAGIC
    {
        return false;
    }
    let Ok(f) = std::fs::File::open(path) else {
        return false;
    };
    let mut flags: u64 = 0;
    use std::os::fd::AsRawFd;
    // SAFETY: the ioctl writes one u64 into `flags`.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), BTRFS_IOC_SUBVOL_GETFLAGS as _, &mut flags) };
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
    fn lchown(&self, path: &Path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
        std::os::unix::fs::lchown(path, uid, gid)
    }

    fn chmod(&self, path: &Path, mode: u32) -> io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }

    fn get_acl(&self, path: &Path, default: bool) -> io::Result<Option<Vec<u8>>> {
        host_get_acl(path, default)
    }

    fn set_acl(&self, path: &Path, default: bool, value: &[u8]) -> io::Result<()> {
        host_set_acl(path, default, value)
    }

    fn readonly_subvolume(&self, path: &Path, meta: &Metadata) -> bool {
        host_readonly_subvolume(path, meta)
    }

    fn mount_points(&self) -> BTreeSet<PathBuf> {
        host_mount_points()
    }
}

/// `rynasd --dry-system`: reads the host, changes nothing.
pub struct DryRemap;

impl RemapFs for DryRemap {
    fn lchown(&self, _: &Path, _: Option<u32>, _: Option<u32>) -> io::Result<()> {
        Ok(())
    }

    fn chmod(&self, _: &Path, _: u32) -> io::Result<()> {
        Ok(())
    }

    fn get_acl(&self, path: &Path, default: bool) -> io::Result<Option<Vec<u8>>> {
        host_get_acl(path, default)
    }

    fn set_acl(&self, _: &Path, _: bool, _: &[u8]) -> io::Result<()> {
        Ok(())
    }

    fn readonly_subvolume(&self, path: &Path, meta: &Metadata) -> bool {
        host_readonly_subvolume(path, meta)
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
        fn lchown(&self, p: &Path, u: Option<u32>, g: Option<u32>) -> io::Result<()> {
            if self.erofs.iter().any(|x| x == p) {
                return Err(io::Error::from_raw_os_error(libc::EROFS));
            }
            self.chowns.lock().unwrap().push((p.into(), u, g));
            Ok(())
        }
        fn chmod(&self, p: &Path, m: u32) -> io::Result<()> {
            self.chmods.lock().unwrap().push((p.into(), m));
            Ok(())
        }
        fn get_acl(&self, p: &Path, d: bool) -> io::Result<Option<Vec<u8>>> {
            if self.real_acls {
                return host_get_acl(p, d);
            }
            Ok(self
                .stored
                .lock()
                .unwrap()
                .iter()
                .find(|(x, dd, _)| x == p && *dd == d)
                .map(|(_, _, r)| r.clone()))
        }
        fn set_acl(&self, p: &Path, d: bool, v: &[u8]) -> io::Result<()> {
            if self.real_acls {
                host_set_acl(p, d, v)?;
            }
            self.acls.lock().unwrap().push((p.into(), d, v.to_vec()));
            Ok(())
        }
        fn readonly_subvolume(&self, p: &Path, _: &Metadata) -> bool {
            self.readonly.iter().any(|x| x == p)
        }
        fn mount_points(&self) -> BTreeSet<PathBuf> {
            self.mounts.clone()
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
        // (lchown, not its target), sub and sub/b; not snap, not mnt, nothing under /etc
        let want: Vec<PathBuf> = ["", "a", "link", "sub", "sub/b"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(chowned, want);
        assert_eq!(c.owners, 5);
        assert_eq!(c.groups, 5);
        assert_eq!(c.readonly, vec![r.join("snap").display().to_string()]);
        assert_eq!(c.other_fs, vec![r.join("mnt").display().to_string()]);
        assert!(!c.partial && !c.cancelled && c.errors == 0);
        assert!(
            fake.chowns
                .lock()
                .unwrap()
                .iter()
                .all(|(_, u, g)| *u == Some(uid + 1) && *g == Some(gid + 1))
        );
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
    fn real_chown_to_own_ids_and_setid_bits_kept() {
        let t = tree();
        let r = t.path().to_path_buf();
        let f = r.join("a");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o2755)).unwrap();
        let (uid, gid) = me();
        let map = IdMap {
            uid: Some((uid, uid)),
            gid: Some((gid, gid)),
        };
        let c = remap(&HostRemap, std::slice::from_ref(&f), &opts(map, false));
        assert_eq!(c.owners, 1);
        assert_eq!(c.errors, 0, "{:?}", c.error_samples);
        let m = std::fs::metadata(&f).unwrap();
        assert_eq!((m.uid(), m.gid()), (uid, gid));
        assert_eq!(m.permissions().mode() & 0o7777, 0o2755);
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
        let (out, n) = remap_acl(&raw, &map).unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            out,
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
        let gmap = IdMap {
            uid: None,
            gid: Some((1026, 2000)),
        };
        assert_eq!(remap_acl(&raw, &gmap).unwrap().1, 1);
        assert!(remap_acl(&raw, &IdMap::default()).is_none());
        assert!(remap_acl(&raw[..7], &map).is_none());
        assert!(remap_acl(&[1, 0, 0, 0], &map).is_none());
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
    }
}
