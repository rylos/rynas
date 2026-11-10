//! Paths that stay inside their shared folder, symbolic links included.
//!
//! Samba's default (`wide links = no`, which rynas keeps): a link is followed while its
//! target stays inside the same share, and refused when it leads anywhere else — the
//! system, another share, a home. The file manager does the same, so that a link planted
//! in a share (`etc -> /`, from an archive or a copy) never shows the rest of the system to
//! its users, nor `/etc/shadow` to an administrator's worker, which keeps
//! `CAP_DAC_READ_SEARCH` ([`crate::caps`]). Such a link is listed as a link and copied as a
//! link; following it answers `outside_share`.
//!
//! Two parts:
//! - [`Root::resolve`]: a path below a share root, every link resolved, as a real path that
//!   is checked to stay inside. `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)` from the
//!   share's directory (Linux 5.6, Debian 13) where the kernel has it; otherwise, and for
//!   the absolute links the kernel refuses beneath a root (`/srv/vol1/docs/a` inside
//!   `docs`), a walk of our own, one component at a time with `O_NOFOLLOW` from the
//!   directory opened before, links read and resolved against the share.
//! - the [`Jail`] of a worker: the share roots of its request or job. [`open`] and
//!   [`read_dir`] open what they read with `O_NOFOLLOW` and then check where the open
//!   descriptor really is (`/proc/self/fd`): a folder swapped for a link between the
//!   resolution and the read (a race of the user's) is refused, not followed.
//!
//! What external programs open (ffmpeg, 7z, unrar, tar) they open themselves, without the
//! administrator's capability (exec drops it): the worst a race gets them is what the user
//! may read anyway. Extraction tools also run under Landlock where the kernel has it
//! (`rynas-files-worker`), allowed to write below their own folder only.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rynas_api_types::error_codes;

/// How many links one resolution follows at most (the kernel's own limit).
const MAX_LINKS: u32 = 40;

const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_BENEATH: u64 = 0x08;

/// `struct open_how` of openat2(2).
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// Cleared once openat2 answered ENOSYS (a kernel before 5.6): the walk does it all.
static OPENAT2: AtomicBool = AtomicBool::new(true);

/// The error of a link that leads out of its share.
pub fn outside() -> io::Error {
    crate::fsops::coded(
        io::ErrorKind::PermissionDenied,
        error_codes::OUTSIDE_SHARE,
        t!("a symbolic link leads out of the shared folder: it is not followed"),
    )
}

/// Whether `e` is [`outside`].
pub fn is_outside(e: &io::Error) -> bool {
    crate::fsops::io_code(e) == error_codes::OUTSIDE_SHARE
}

fn cstr(p: &OsStr) -> io::Result<CString> {
    CString::new(p.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn openat(dir: RawFd, path: &OsStr, flags: i32) -> io::Result<OwnedFd> {
    let c = cstr(path)?;
    // SAFETY: a valid C string; the descriptor returned is ours
    let fd = unsafe { libc::openat(dir, c.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just opened
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn openat2(dir: RawFd, path: &OsStr, flags: i32, resolve: u64) -> io::Result<OwnedFd> {
    let c = cstr(path)?;
    let how = OpenHow { flags: (flags | libc::O_CLOEXEC) as u32 as u64, mode: 0, resolve };
    // SAFETY: a valid C string and an open_how of the size given
    let fd = unsafe { libc::syscall(libc::SYS_openat2, dir, c.as_ptr(), &how as *const OpenHow, std::mem::size_of::<OpenHow>()) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just opened
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

fn fstat(fd: BorrowedFd) -> io::Result<libc::stat> {
    // SAFETY: zeroed out-parameter, an open descriptor
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn is_link(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFLNK
}

fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

/// The target of the link `name` in `dir`.
fn readlinkat(dir: RawFd, name: &OsStr) -> io::Result<PathBuf> {
    let c = cstr(name)?;
    let mut buf = vec![0u8; 4096];
    // SAFETY: a valid C string and a buffer of the length given
    let n = unsafe { libc::readlinkat(dir, c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

/// The path of an open descriptor, for the system calls that take one.
pub fn fd_path(fd: BorrowedFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

/// Where an open descriptor really is, as the kernel says (its mount namespace's path).
pub fn real_path(fd: BorrowedFd) -> io::Result<PathBuf> {
    fs::read_link(fd_path(fd))
}

/// A share's directory, opened: the start of every resolution below it.
#[derive(Debug)]
pub struct Root {
    fd: OwnedFd,
    /// Its real path (the share's path may go through a link: a home).
    canon: PathBuf,
    /// The path as configured: absolute links through it count as inside too.
    given: PathBuf,
}

impl Root {
    pub fn open(path: &Path) -> io::Result<Root> {
        // the share's own path is the administrator's: followed (a home below a link)
        let fd = openat(libc::AT_FDCWD, path.as_os_str(), libc::O_PATH | libc::O_DIRECTORY)?;
        let canon = real_path(fd.as_fd()).or_else(|_| fs::canonicalize(path))?;
        Ok(Root { fd, canon, given: path.to_path_buf() })
    }

    /// The share's real path.
    pub fn path(&self) -> &Path {
        &self.canon
    }

    /// Whether a real path is this share or inside it.
    pub fn holds(&self, real: &Path) -> bool {
        real.starts_with(&self.canon)
    }

    /// `rel` (plain components, as [`crate::shares::resolve`] checks them) below the share,
    /// as a real path without any link: the last component followed too when `follow`, else
    /// kept as it is (a link stays the link). A link that leads out of the share is
    /// [`outside`]; a missing item is `NotFound`, as the kernel says.
    pub fn resolve(&self, rel: &Path, follow: bool) -> io::Result<PathBuf> {
        if rel.as_os_str().is_empty() {
            return Ok(self.canon.clone());
        }
        let flags = libc::O_PATH | if follow { 0 } else { libc::O_NOFOLLOW };
        let mut tries = 0;
        while OPENAT2.load(Ordering::Relaxed) && tries < 3 {
            tries += 1;
            match openat2(self.fd.as_raw_fd(), rel.as_os_str(), flags, RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS) {
                Ok(fd) => match real_path(fd.as_fd()) {
                    Ok(p) if self.holds(&p) => return Ok(p),
                    // a bind mount seen under another name: let the walk decide
                    _ => break,
                },
                Err(e) => match e.raw_os_error() {
                    // a rename raced the resolution: the kernel asks to try again
                    Some(libc::EAGAIN) => continue,
                    Some(libc::ENOSYS) => {
                        OPENAT2.store(false, Ordering::Relaxed);
                        break;
                    }
                    // EXDEV: out of the root, or an absolute link (refused beneath a root
                    // even when it points inside): the walk tells which. EPERM, EINVAL:
                    // a filter or an old kernel
                    Some(libc::EXDEV) | Some(libc::EPERM) | Some(libc::EINVAL) | Some(libc::E2BIG) => break,
                    _ => return Err(e),
                },
            }
        }
        self.walk(rel, follow)
    }

    /// The path below the share of a path made of the share's path and a relative one (as
    /// [`crate::shares::resolve`] joins them), not resolved yet.
    pub fn rel_of<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        path.strip_prefix(&self.given).or_else(|_| path.strip_prefix(&self.canon)).ok()
    }

    /// The path below the share an absolute link target names, if it names one.
    fn below(&self, target: &Path) -> Option<PathBuf> {
        target.strip_prefix(&self.canon).or_else(|_| target.strip_prefix(&self.given)).ok().map(Path::to_path_buf)
    }

    /// [`Root::resolve`] without openat2: one component at a time, each opened with
    /// `O_NOFOLLOW` from the folder opened before; `..` (only in link targets) goes back up
    /// our own list, never above the share.
    fn walk(&self, rel: &Path, follow: bool) -> io::Result<PathBuf> {
        let mut names: Vec<OsString> = Vec::new();
        let mut dirs: Vec<OwnedFd> = Vec::new();
        let mut todo: VecDeque<OsString> = parts(rel)?;
        let mut links = 0;
        while let Some(part) = todo.pop_front() {
            if part == ".." {
                if names.pop().is_none() {
                    return Err(outside());
                }
                dirs.pop();
                continue;
            }
            let dir = dirs.last().map_or(self.fd.as_raw_fd(), AsRawFd::as_raw_fd);
            let fd = openat(dir, &part, libc::O_PATH | libc::O_NOFOLLOW)?;
            let st = fstat(fd.as_fd())?;
            let last = todo.is_empty();
            if is_link(&st) && (follow || !last) {
                links += 1;
                if links > MAX_LINKS {
                    return Err(io::Error::from_raw_os_error(libc::ELOOP));
                }
                let target = readlinkat(dir, &part)?;
                let mut next = if target.is_absolute() {
                    let rel = self.below(&target).ok_or_else(outside)?;
                    names.clear();
                    dirs.clear();
                    parts(&rel)?
                } else {
                    parts(&target)?
                };
                next.extend(todo.drain(..));
                todo = next;
                continue;
            }
            if !last && !is_dir(&st) {
                return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
            }
            names.push(part);
            dirs.push(fd);
        }
        Ok(names.iter().fold(self.canon.clone(), |p, n| p.join(n)))
    }
}

/// The components of a relative path or a link target: names and `..`, nothing else.
fn parts(p: &Path) -> io::Result<VecDeque<OsString>> {
    let mut out = VecDeque::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push_back(n.to_os_string()),
            Component::ParentDir => out.push_back("..".into()),
            Component::CurDir | Component::RootDir => {}
            Component::Prefix(_) => return Err(io::Error::from(io::ErrorKind::InvalidInput)),
        }
    }
    Ok(out)
}

thread_local! {
    /// The share roots of the request or job this thread works for: `None` outside a
    /// worker (rynasd, the tests of other modules), where nothing is checked.
    static JAIL: RefCell<Option<Vec<Arc<Root>>>> = const { RefCell::new(None) };
}

/// While it lives, [`open`] and [`read_dir`] on this thread accept only what lies inside
/// the roots [`Jail::admit`]ted (none at first: nothing at all). The worker holds one for
/// the whole request or job.
pub struct Jail {
    prev: Option<Vec<Arc<Root>>>,
}

impl Jail {
    pub fn enter() -> Jail {
        Jail { prev: JAIL.with(|j| j.borrow_mut().replace(Vec::new())) }
    }

    /// Opens a share root and lets what is inside it be read.
    pub fn admit(path: &Path) -> io::Result<Arc<Root>> {
        let root = Arc::new(Root::open(path)?);
        Ok(admit(root))
    }
}

/// The roots of this thread's jail, for a thread it starts ([`Jail::inherit`]).
#[derive(Clone)]
pub struct Roots(Option<Vec<Arc<Root>>>);

/// The current jail of this thread.
pub fn current() -> Roots {
    Roots(JAIL.with(|j| j.borrow().clone()))
}

impl Jail {
    /// The jail of the thread that started this one (the space analysis reads with many).
    pub fn inherit(roots: Roots) -> Jail {
        Jail { prev: JAIL.with(|j| std::mem::replace(&mut *j.borrow_mut(), roots.0)) }
    }
}

impl Drop for Jail {
    fn drop(&mut self) {
        let prev = self.prev.take();
        JAIL.with(|j| *j.borrow_mut() = prev);
    }
}

/// Adds `root` to the jail of this thread, if there is one; the root already there when
/// the same share was admitted before.
pub fn admit(root: Arc<Root>) -> Arc<Root> {
    JAIL.with(|j| match j.borrow_mut().as_mut() {
        Some(roots) => match roots.iter().find(|r| r.canon == root.canon) {
            Some(r) => Arc::clone(r),
            None => {
                roots.push(Arc::clone(&root));
                root
            }
        },
        None => root,
    })
}

/// The admitted root a real path lies in (the deepest, should shares nest).
pub fn root_of(real: &Path) -> Option<Arc<Root>> {
    JAIL.with(|j| j.borrow().as_ref()?.iter().filter(|r| r.holds(real)).max_by_key(|r| r.canon.as_os_str().len()).cloned())
}

/// Whether this thread works inside a jail.
pub fn active() -> bool {
    JAIL.with(|j| j.borrow().is_some())
}

/// Refuses an open descriptor that is not inside an admitted root (nothing is checked
/// outside a jail).
pub fn check(fd: BorrowedFd) -> io::Result<()> {
    if !active() {
        return Ok(());
    }
    let real = real_path(fd)?;
    if root_of(&real).is_some() { Ok(()) } else { Err(outside()) }
}

/// Opens a file to read it, the last component never followed, and checks where it is.
pub fn open(path: &Path) -> io::Result<File> {
    let fd = openat(libc::AT_FDCWD, path.as_os_str(), libc::O_RDONLY | libc::O_NOFOLLOW)?;
    check(fd.as_fd())?;
    Ok(File::from(fd))
}

/// Opens a file to write it in place (the editor, a file with more links), as [`open`].
pub fn open_write(path: &Path) -> io::Result<File> {
    let fd = openat(libc::AT_FDCWD, path.as_os_str(), libc::O_WRONLY | libc::O_NOFOLLOW)?;
    check(fd.as_fd())?;
    Ok(File::from(fd))
}

/// Checks a file this call just made with `create_new` (a part file, an archive) and
/// removes it again when it landed out of the jail (a folder on its way swapped for a link
/// meanwhile). In the apps' data the file then gets its folder's owner ([`adopt`]).
pub fn check_made(file: &File, path: &Path) -> io::Result<()> {
    check(file.as_fd()).inspect_err(|_| {
        let _ = fs::remove_file(path);
    })?;
    // in the apps' data: the folder's owner, as the app expects
    adopt(file, path)
}

/// The owner and group of the folder `path` is in: what an item made there is given in the
/// apps' data ([`adopt`]).
fn parent_owner(path: &Path) -> io::Result<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let meta = fs::metadata(parent)?;
    Ok((meta.uid(), meta.gid()))
}

/// In the apps' data ([`crate::caps::app_data`]): gives a file this worker just made at
/// `path` (open as `file`) the owner and group of its folder, so the app that owns the
/// folder can change it as before. Nothing anywhere else: a user's files are the user's.
pub fn adopt(file: &File, path: &Path) -> io::Result<()> {
    if !crate::caps::app_data() {
        return Ok(());
    }
    let (uid, gid) = parent_owner(path)?;
    std::os::unix::fs::fchown(file, Some(uid), Some(gid))
}

/// [`adopt`] for a symbolic link this worker just made (the link itself, never its target).
pub fn adopt_link(path: &Path) -> io::Result<()> {
    if !crate::caps::app_data() {
        return Ok(());
    }
    let (uid, gid) = parent_owner(path)?;
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))
}

/// Checks a folder this call just made with `mkdir`: opened without following a link, it
/// must lie in the jail, else it is removed again (a folder on its way swapped for a link
/// meanwhile); in the apps' data it gets its parent's owner ([`adopt`]).
pub fn made_dir(path: &Path) -> io::Result<()> {
    let fd = openat(libc::AT_FDCWD, path.as_os_str(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW)?;
    if let Err(e) = check(fd.as_fd()) {
        let _ = fs::remove_dir(path);
        return Err(e);
    }
    adopt(&File::from(fd), path)
}

/// In the apps' data: gives everything below `dir` (items this worker has just made, an
/// archive extracted into its hidden folder) the owner and group of the folder `like`;
/// links themselves, never followed. Nothing anywhere else.
pub fn adopt_tree(dir: &Path, like: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if !crate::caps::app_data() {
        return Ok(());
    }
    fn walk(dir: &Path, uid: u32, gid: u32) -> io::Result<()> {
        for e in read_dir(dir)?.flatten() {
            let path = e.path();
            std::os::unix::fs::lchown(&path, Some(uid), Some(gid))?;
            if e.file_type()?.is_dir() {
                walk(&path, uid, gid)?;
            }
        }
        Ok(())
    }
    let meta = fs::metadata(like)?;
    walk(dir, meta.uid(), meta.gid())
}

/// Hands an open file to a program (ffmpeg) as its descriptor 3, and returns the name the
/// program opens it by: it reads that very file, not whatever a path names by then.
pub fn child_input(cmd: &mut std::process::Command, file: &File) -> PathBuf {
    use std::os::unix::process::CommandExt;
    let fd = file.as_raw_fd();
    // SAFETY: only system calls between fork and exec
    unsafe {
        cmd.pre_exec(move || {
            if fd == 3 {
                // dup2 onto itself keeps close-on-exec: clear it
                if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(fd, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    PathBuf::from("/proc/self/fd/3")
}

/// A folder's entries: the folder opened with `O_NOFOLLOW` and checked, then read through
/// its descriptor. The entries' paths are below `dir` as given.
pub fn read_dir(dir: &Path) -> io::Result<ReadDir> {
    let fd = openat(libc::AT_FDCWD, dir.as_os_str(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW)?;
    check(fd.as_fd())?;
    // the magic link reopens this very folder, whatever its name is by now
    let inner = fs::read_dir(fd_path(fd.as_fd()))?;
    Ok(ReadDir { inner, dir: dir.to_path_buf() })
}

pub struct ReadDir {
    inner: fs::ReadDir,
    dir: PathBuf,
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|r| r.map(|inner| DirEntry { path: self.dir.join(inner.file_name()), inner }))
    }
}

/// An entry of [`read_dir`]: its metadata comes from the folder's descriptor (lstat).
pub struct DirEntry {
    inner: fs::DirEntry,
    path: PathBuf,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }
    pub fn file_name(&self) -> OsString {
        self.inner.file_name()
    }
    pub fn file_type(&self) -> io::Result<fs::FileType> {
        self.inner.file_type()
    }
    pub fn metadata(&self) -> io::Result<fs::Metadata> {
        self.inner.metadata()
    }
}

/// Where a symbolic link leads, inside a jail: its real target when that stays in the
/// link's own share, [`outside`] when not (or when the jail holds no share around it).
/// `None` outside a jail.
pub fn link_target(link: &Path) -> Option<io::Result<PathBuf>> {
    if !active() {
        return None;
    }
    let found = link.parent().and_then(root_of).and_then(|root| Some(root.resolve(link.strip_prefix(&root.canon).ok()?, true)));
    Some(found.unwrap_or_else(|| Err(outside())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// /…/vol: share "docs" and share "other", a secret beside them.
    fn layout() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let vol = fs::canonicalize(t.path()).unwrap();
        let docs = vol.join("docs");
        let other = vol.join("other");
        fs::create_dir_all(docs.join("a/b")).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(docs.join("a/b/f.txt"), "inside").unwrap();
        fs::write(other.join("o.txt"), "other share").unwrap();
        fs::write(vol.join("secret"), "secret").unwrap();
        (t, docs, other)
    }

    fn both(root: &Root, rel: &str, follow: bool) -> io::Result<PathBuf> {
        let a = root.resolve(Path::new(rel), follow);
        let b = root.walk(Path::new(rel), follow);
        match (&a, &b) {
            (Ok(x), Ok(y)) => assert_eq!(x, y, "{rel}: openat2 and the walk differ"),
            (Err(x), Err(y)) => assert_eq!(io_kind(x), io_kind(y), "{rel}: {x} / {y}"),
            _ => panic!("{rel}: {a:?} / {b:?}"),
        }
        a
    }

    fn io_kind(e: &io::Error) -> &'static str {
        crate::fsops::io_code(e)
    }

    #[test]
    fn links_inside_the_share_are_followed() {
        let (_t, docs, _) = layout();
        symlink("a/b", docs.join("rel")).unwrap();
        symlink(docs.join("a"), docs.join("abs")).unwrap();
        symlink("../b/f.txt", docs.join("a/b/up")).unwrap();
        symlink("rel", docs.join("chain")).unwrap();
        symlink("chain/f.txt", docs.join("chain2")).unwrap();
        let root = Root::open(&docs).unwrap();
        assert_eq!(both(&root, "rel/f.txt", true).unwrap(), docs.join("a/b/f.txt"));
        assert_eq!(both(&root, "abs/b", true).unwrap(), docs.join("a/b"));
        assert_eq!(both(&root, "a/b/up", true).unwrap(), docs.join("a/b/f.txt"));
        assert_eq!(both(&root, "chain2", true).unwrap(), docs.join("a/b/f.txt"));
        assert_eq!(both(&root, "", true).unwrap(), docs);
        // the last link kept when asked
        assert_eq!(both(&root, "rel", false).unwrap(), docs.join("rel"));
        assert_eq!(both(&root, "rel/f.txt", false).unwrap(), docs.join("a/b/f.txt"));
    }

    #[test]
    fn links_out_of_the_share_are_refused() {
        let (_t, docs, other) = layout();
        symlink("/etc", docs.join("etc")).unwrap();
        symlink("../secret", docs.join("up")).unwrap();
        symlink("a/../../secret", docs.join("dots")).unwrap();
        symlink(&other, docs.join("share2")).unwrap();
        symlink("../other/o.txt", docs.join("share2rel")).unwrap();
        symlink("etc", docs.join("chain")).unwrap();
        symlink(docs.join("a/../../secret"), docs.join("absdots")).unwrap();
        let root = Root::open(&docs).unwrap();
        for rel in ["etc", "etc/passwd", "up", "dots", "share2", "share2/o.txt", "share2rel", "chain/passwd", "absdots"] {
            let e = both(&root, rel, true).unwrap_err();
            assert!(is_outside(&e), "{rel}: {e}");
        }
        // not followed: the link itself is fine (listing, copying it as a link)
        assert_eq!(both(&root, "etc", false).unwrap(), docs.join("etc"));
        // ... but not as a folder on the way
        assert!(is_outside(&both(&root, "etc/passwd", false).unwrap_err()));
    }

    #[test]
    fn missing_items_and_loops() {
        let (_t, docs, _) = layout();
        symlink("loop2", docs.join("loop1")).unwrap();
        symlink("loop1", docs.join("loop2")).unwrap();
        symlink("nowhere", docs.join("broken")).unwrap();
        let root = Root::open(&docs).unwrap();
        assert_eq!(both(&root, "nope", true).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(both(&root, "broken", true).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(both(&root, "broken", false).unwrap(), docs.join("broken"));
        assert_eq!(both(&root, "loop1", true).unwrap_err().raw_os_error(), Some(libc::ELOOP));
        assert_eq!(both(&root, "a/b/f.txt/x", true).unwrap_err().raw_os_error(), Some(libc::ENOTDIR));
    }

    #[test]
    fn a_share_reached_through_a_link() {
        let (t, docs, _) = layout();
        let alias = t.path().join("alias");
        symlink(&docs, &alias).unwrap();
        // an absolute link through the configured path counts as inside
        symlink(alias.join("a"), docs.join("via")).unwrap();
        let root = Root::open(&alias).unwrap();
        assert_eq!(root.path(), docs);
        assert_eq!(both(&root, "via/b/f.txt", true).unwrap(), docs.join("a/b/f.txt"));
    }

    #[test]
    fn the_jail_checks_what_is_opened() {
        let (t, docs, other) = layout();
        symlink("/etc", docs.join("etc")).unwrap();
        // nothing checked without a jail
        assert!(open(&t.path().join("secret")).is_ok());
        let _jail = Jail::enter();
        // nothing admitted yet: nothing at all
        assert!(is_outside(&open(&docs.join("a/b/f.txt")).unwrap_err()));
        Jail::admit(&docs).unwrap();
        assert!(open(&docs.join("a/b/f.txt")).is_ok());
        assert!(is_outside(&open(&other.join("o.txt")).unwrap_err()));
        assert!(is_outside(&open(&t.path().join("secret")).unwrap_err()));
        // the last component is never followed
        assert_eq!(open(&docs.join("etc")).unwrap_err().raw_os_error(), Some(libc::ELOOP));
        // a folder on the way swapped for a link (the race): the descriptor tells
        assert!(is_outside(&open(&docs.join("etc/hostname")).unwrap_err()));
        assert!(is_outside(&read_dir(&docs.join("etc/ssl")).err().unwrap()));
        let names: Vec<_> = read_dir(&docs.join("a")).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(names, vec![docs.join("a/b")]);
        assert!(matches!(link_target(&docs.join("etc")), Some(Err(ref e)) if is_outside(e)));
        assert!(matches!(link_target(&other.join("o.txt")), Some(Err(ref e)) if is_outside(e)));
        drop(_jail);
        assert!(link_target(&docs.join("etc")).is_none());
    }

    #[test]
    fn a_folder_swapped_while_resolving_is_caught() {
        // the race: `a` becomes a link to the outside after the path was resolved; what is
        // opened afterwards through the old path is refused
        let (t, docs, _) = layout();
        let root = Root::open(&docs).unwrap();
        let real = root.resolve(Path::new("a/b/f.txt"), true).unwrap();
        let _jail = Jail::enter();
        admit(Arc::new(root));
        fs::rename(docs.join("a"), docs.join("a-old")).unwrap();
        fs::create_dir_all(t.path().join("x/b")).unwrap();
        fs::write(t.path().join("x/b/f.txt"), "outside").unwrap();
        symlink(t.path().join("x"), docs.join("a")).unwrap();
        assert!(is_outside(&open(&real).unwrap_err()));
    }
}
