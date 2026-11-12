//! What the interface reads and the quick changes it makes, run in the CGI
//! process after it became the user.

use std::ffi::CString;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::wire::ApiError;

/// Never shown: the file manager's own temporary names (and those SynoFile left on a
/// volume that comes from a NAS that ran it), and the thumbnail folders another NAS's
/// photo app keeps beside the photos.
pub fn is_system_name(name: &str) -> bool {
    name == "@eaDir"
        || name.starts_with(".rynas-x-")
        || name.ends_with(".rynas-part")
        || name.ends_with(".rynas-upload")
        || name.ends_with(".rynas-edit")
        || name.starts_with(".synofile-")
        || name.ends_with(".synofile-part")
        || name.ends_with(".synofile-upload")
}

#[derive(Serialize)]
pub struct Entry {
    pub name: String,
    /// d directory, f file, l symlink, o anything else
    pub kind: char,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
    /// for a symlink: whether it points to a directory
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub link_dir: bool,
    /// for a symlink: it leads out of its shared folder, and is never followed
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub outside: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub thumb: bool,
    /// a read-only btrfs snapshot: it cannot be renamed, moved or deleted
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub readonly: bool,
    /// for a recycle bin: whether something is in it
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full: Option<bool>,
}

fn kind(meta: &Metadata) -> char {
    let t = meta.file_type();
    if t.is_dir() {
        'd'
    } else if t.is_file() {
        'f'
    } else if t.is_symlink() {
        'l'
    } else {
        'o'
    }
}

pub fn entry(path: &Path, name: String, meta: &Metadata) -> Entry {
    let kind = kind(meta);
    let (link_dir, outside) = if kind == 'l' { link(path) } else { (false, false) };
    Entry { name, kind, size: if kind == 'd' { 0 } else { meta.len() }, mtime: meta.mtime(), mode: meta.mode() & 0o7777, link_dir, outside, thumb: false, readonly: kind == 'd' && readonly_subvolume(path, meta), full: None }
}

/// For a symbolic link: whether it leads to a folder, and whether out of its share (then it
/// is not followed, not even to tell what it points to).
fn link(path: &Path) -> (bool, bool) {
    match crate::confine::link_target(path) {
        Some(Ok(target)) => (fs::metadata(target).is_ok_and(|m| m.is_dir()), false),
        Some(Err(e)) => (false, crate::confine::is_outside(&e)),
        None => (fs::metadata(path).is_ok_and(|m| m.is_dir()), false),
    }
}

/// Whether a folder is the top of a read-only btrfs subvolume: a snapshot
/// (btrbk's, a `#snapshot` folder). Only subvolume tops (inode 256) are asked.
fn readonly_subvolume(path: &Path, meta: &Metadata) -> bool {
    const BTRFS_FIRST_FREE_OBJECTID: u64 = 256;
    // _IOR(0x94, 25, u64)
    const BTRFS_IOC_SUBVOL_GETFLAGS: libc::c_ulong = 0x8008_9419;
    const BTRFS_SUBVOL_RDONLY: u64 = 1 << 1;
    if meta.ino() != BTRFS_FIRST_FREE_OBJECTID {
        return false;
    }
    let Ok(dir) = crate::confine::open(path) else { return false };
    let mut flags: u64 = 0;
    // SAFETY: an open directory and a u64 for the kernel to fill
    let rc = unsafe { libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(&dir), BTRFS_IOC_SUBVOL_GETFLAGS as _, &mut flags) };
    rc == 0 && flags & BTRFS_SUBVOL_RDONLY != 0
}

/// Refuses to rename, move or delete the top of a read-only btrfs snapshot
/// ([`Entry::readonly`]). Its contents the kernel protects (EROFS, whatever the
/// capabilities), but its folder lies in the writable subvolume above: an administrator's
/// worker, which writes over the parent's permissions, would otherwise rename it or move it
/// into the recycle bin.
pub fn refuse_snapshot(path: &Path) -> Result<(), ApiError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && readonly_subvolume(path, &meta) => {
            Err(ApiError::read_only(t!("a read-only snapshot cannot be renamed, moved or deleted")))
        }
        _ => Ok(()),
    }
}

#[derive(Serialize)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub writable: bool,
}

pub fn list(dir: &Path) -> Result<Listing, ApiError> {
    let meta = fs::metadata(dir)?;
    if !meta.is_dir() {
        return Err(ApiError::invalid("path", t!("not a folder")));
    }
    // a set: a folder of photos has thousands of entries in both
    let thumbs = crate::thumbs::has_eadir(dir);
    let mut entries = Vec::new();
    for item in crate::confine::read_dir(dir)?.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if is_system_name(&name) {
            continue;
        }
        let path = item.path();
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        let mut e = entry(&path, name, &meta);
        e.thumb = e.kind == 'f' && (thumbs.contains(&item.file_name()) || crate::thumbs::can_make(&e.name));
        if e.kind == 'd' && e.name == crate::shares::RECYCLE {
            e.full = Some(bin_full(&path));
        }
        entries.push(e);
    }
    Ok(Listing { entries, writable: access(dir, libc::W_OK | libc::X_OK) })
}

/// Whether a recycle bin holds anything: one entry read, not the whole bin.
fn bin_full(bin: &Path) -> bool {
    crate::confine::read_dir(bin).is_ok_and(|d| d.flatten().any(|i| !is_system_name(&i.file_name().to_string_lossy())))
}

/// Whether the worker may `mode` the file.
///
/// An ordinary worker asks access(2): the real user's permissions and ACLs. A worker that
/// writes over owners ([`crate::caps::writes_over_owners`]: an administrator's) must ask with
/// its effective identity *and capabilities*: access(2), and faccessat(2) without
/// `AT_EACCESS`, check with the real credentials and, for a uid other than 0, clear the
/// effective capabilities first (`access_override_creds` in the kernel), so a folder of
/// another uid would show as not writable although the worker can write there, and the
/// interface would hide Delete. faccessat2(2) with `AT_EACCESS` (Linux 5.8) keeps the
/// worker's credentials: `CAP_DAC_OVERRIDE` counts, and a read-only mount or a read-only
/// btrfs subvolume still answers EROFS, which no capability overrides. Asked as a system call
/// of its own: glibc emulates `AT_EACCESS` without faccessat2 from the mode bits alone.
/// Landlock is not asked (it does not answer access checks): the callers only ask inside the
/// shares, which the ruleset allows.
pub fn access(path: &Path, mode: i32) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return false };
    // SAFETY: valid C string
    let plain = || unsafe { libc::access(c.as_ptr(), mode) == 0 };
    if !crate::caps::writes_over_owners() {
        return plain();
    }
    // SAFETY: valid C string, integer arguments
    let rc = unsafe { libc::syscall(libc::SYS_faccessat2, libc::AT_FDCWD, c.as_ptr(), mode, libc::AT_EACCESS) };
    if rc == 0 {
        return true;
    }
    if io::Error::last_os_error().raw_os_error() != Some(libc::ENOSYS) {
        return false;
    }
    // a kernel older than 5.8: what the capabilities let through, the volume permitting
    let exists = fs::symlink_metadata(path).is_ok();
    access_without_faccessat2(plain(), exists, mode & libc::W_OK != 0, || read_only_mount(path))
}

/// [`access`] for a worker that writes over owners, where the kernel has no faccessat2:
/// what the user may do anyway, or anything on an existing item but writing on a read-only
/// volume. (A read-only btrfs snapshot on a writable volume is then not seen here: its top
/// is flagged by [`Entry::readonly`] and refused by [`refuse_snapshot`], and the kernel
/// refuses every write inside it.)
fn access_without_faccessat2(plain: bool, exists: bool, write: bool, read_only: impl FnOnce() -> bool) -> bool {
    plain || (exists && !(write && read_only()))
}

/// Whether `path` lies on a filesystem mounted read-only.
fn read_only_mount(path: &Path) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return false };
    // SAFETY: valid C string, a statvfs for the kernel to fill
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        libc::statvfs(c.as_ptr(), &mut st) == 0 && st.f_flag & libc::ST_RDONLY != 0
    }
}

#[derive(Serialize)]
pub struct Details {
    pub name: String,
    pub kind: char,
    pub size: u64,
    pub mtime: i64,
    pub atime: i64,
    pub ctime: i64,
    pub mode: u32,
    pub owner: String,
    pub group: String,
    pub inode: u64,
    pub links: u64,
    pub target: Option<String>,
    /// a symbolic link out of its shared folder: not followed
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub outside: bool,
    pub readable: bool,
    pub writable: bool,
    pub device: u64,
}

pub fn details(path: &Path) -> Result<Details, ApiError> {
    let meta = fs::symlink_metadata(path)?;
    let target = meta.file_type().is_symlink().then(|| fs::read_link(path).map(|t| t.to_string_lossy().into_owned()).unwrap_or_default());
    // what a link leads to is asked only when it stays inside the share
    let (outside, followed) = match target.as_ref().and_then(|_| crate::confine::link_target(path)) {
        Some(Ok(real)) => (false, Some(real)),
        Some(Err(e)) => (crate::confine::is_outside(&e), None),
        None => (false, Some(path.to_path_buf())),
    };
    let can = |mode| followed.as_deref().is_some_and(|p| access(p, mode));
    Ok(Details {
        name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        kind: kind(&meta),
        size: meta.len(),
        mtime: meta.mtime(),
        atime: meta.atime(),
        ctime: meta.ctime(),
        mode: meta.permissions().mode() & 0o7777,
        owner: crate::identity::names("/etc/passwd", meta.uid()).unwrap_or_else(|| meta.uid().to_string()),
        group: crate::identity::names("/etc/group", meta.gid()).unwrap_or_else(|| meta.gid().to_string()),
        inode: meta.ino(),
        links: meta.nlink(),
        target,
        outside,
        readable: can(libc::R_OK),
        writable: can(libc::W_OK),
        device: meta.dev(),
    })
}

pub fn mkdir(path: &Path) -> Result<(), ApiError> {
    fs::create_dir(path).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists { ApiError::exists(t!("an item with this name already exists")) } else { ApiError::from(e) }
    })?;
    Ok(crate::confine::made_dir(path)?)
}

/// Renames without ever replacing: renameat2 with RENAME_NOREPLACE, and a
/// plain check where the filesystem lacks it.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (Ok(a), Ok(b)) = (CString::new(from.as_os_str().as_bytes()), CString::new(to.as_os_str().as_bytes())) else {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    };
    const RENAME_NOREPLACE: libc::c_uint = 1;
    // SAFETY: valid C strings
    let rc = unsafe { libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, a.as_ptr(), libc::AT_FDCWD, b.as_ptr(), RENAME_NOREPLACE) };
    if rc == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EINVAL) | Some(libc::ENOSYS) => {
            // a case-only rename on a case-insensitive name is still fine
            if fs::symlink_metadata(to).is_ok() && !same_file(from, to) {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            fs::rename(from, to)
        }
        _ => Err(e),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

pub fn rename(from: &Path, to: &Path) -> Result<(), ApiError> {
    rename_noreplace(from, to).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists { ApiError::exists(t!("an item with this name already exists")) } else { e.into() }
    })
}

#[derive(Serialize, Default)]
pub struct Usage {
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
    pub unreadable: u64,
    /// stopped at the deadline: the numbers are a lower bound
    pub partial: bool,
}

pub fn usage(paths: &[PathBuf], budget: Duration) -> Usage {
    let deadline = Instant::now() + budget;
    let mut u = Usage::default();
    let mut stack: Vec<PathBuf> = paths.to_vec();
    while let Some(path) = stack.pop() {
        if Instant::now() > deadline {
            u.partial = true;
            break;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            u.unreadable += 1;
            continue;
        };
        if meta.is_dir() {
            u.dirs += 1;
            match crate::confine::read_dir(&path) {
                Ok(rd) => stack.extend(rd.flatten().filter(|e| !is_system_name(&e.file_name().to_string_lossy())).map(|e| e.path())),
                Err(_) => u.unreadable += 1,
            }
        } else {
            u.files += 1;
            u.bytes += meta.len();
        }
    }
    u
}

#[derive(Serialize)]
pub struct Hit {
    pub dir: String,
    #[serde(flatten)]
    pub entry: Entry,
}

#[derive(Serialize)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    pub scanned: u64,
    pub partial: bool,
}

/// What the search below a folder looks for: words that must all appear
/// in the name ("mkv" finds every .mkv), or a glob with * and ?. The fuzzy
/// matching is the interface's filter, on one folder.
pub struct Pattern {
    glob: Option<Vec<char>>,
    words: Vec<String>,
}

impl Pattern {
    pub fn new(query: &str) -> Self {
        let q = query.trim().to_lowercase();
        if q.contains('*') || q.contains('?') {
            Self { glob: Some(q.chars().collect()), words: Vec::new() }
        } else {
            Self { glob: None, words: q.split_whitespace().map(String::from).collect() }
        }
    }

    pub fn matches(&self, name: &str) -> bool {
        let name = name.to_lowercase();
        match &self.glob {
            Some(glob) => glob_match(glob, &name.chars().collect::<Vec<_>>()),
            None => !self.words.is_empty() && self.words.iter().all(|w| name.contains(w.as_str())),
        }
    }
}

fn glob_match(p: &[char], s: &[char]) -> bool {
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Walks below `root` breadth first, so near results come first, and
/// returns the matches with their directory relative to `root`.
pub fn search(root: &Path, pattern: &Pattern, limit: usize, budget: Duration) -> SearchResult {
    let deadline = Instant::now() + budget;
    let mut hits = Vec::new();
    let mut scanned = 0u64;
    let mut queue = std::collections::VecDeque::from([PathBuf::new()]);
    while let Some(rel) = queue.pop_front() {
        let Ok(rd) = crate::confine::read_dir(&root.join(&rel)) else { continue };
        for item in rd.flatten() {
            if (scanned.is_multiple_of(256) && Instant::now() > deadline) || hits.len() >= limit {
                return SearchResult { hits, scanned, partial: true };
            }
            let name = item.file_name().to_string_lossy().into_owned();
            if is_system_name(&name) || name == crate::shares::RECYCLE || name == "#snapshot" {
                continue;
            }
            scanned += 1;
            let Ok(kind) = item.file_type() else { continue };
            if kind.is_dir() {
                queue.push_back(rel.join(&name));
            }
            if pattern.matches(&name) {
                let Ok(meta) = fs::symlink_metadata(item.path()) else { continue };
                hits.push(Hit { dir: rel.to_string_lossy().into_owned(), entry: entry(&item.path(), name, &meta) });
            }
        }
    }
    SearchResult { hits, scanned, partial: false }
}

/// Space on the filesystem holding `path`: (free for the user, total).
pub fn space(path: &Path) -> Option<(u64, u64)> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: zeroed out-parameter, valid C string
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail * st.f_frsize, st.f_blocks * st.f_frsize))
}

/// The rynas error code of an I/O error (`error_codes`), for the API and for the error
/// list of a job.
pub fn io_code(e: &io::Error) -> &'static str {
    use rynas_api_types::error_codes as c;
    if let Some(coded) = e.get_ref().and_then(|r| r.downcast_ref::<Coded>()) {
        return coded.code;
    }
    match e.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => c::PERMISSION_DENIED,
        Some(libc::ENOENT) => c::NOT_FOUND,
        Some(libc::EEXIST) | Some(libc::ENOTEMPTY) => c::ALREADY_EXISTS,
        Some(libc::ENOSPC) | Some(libc::EDQUOT) => c::INSUFFICIENT_STORAGE,
        Some(libc::EROFS) | Some(libc::ETXTBSY) => c::READ_ONLY,
        Some(libc::ENAMETOOLONG) | Some(libc::EISDIR) | Some(libc::ENOTDIR) | Some(libc::EINVAL) | Some(libc::ELOOP) => c::INVALID_VALUE,
        Some(libc::EBUSY) => c::BUSY,
        Some(libc::ETIMEDOUT) => c::TIMEOUT,
        Some(libc::EHOSTUNREACH) | Some(libc::ECONNREFUSED) | Some(libc::ECONNRESET) | Some(libc::ENETUNREACH) => c::REMOTE_UNREACHABLE,
        _ => match e.kind() {
            io::ErrorKind::NotFound => c::NOT_FOUND,
            io::ErrorKind::PermissionDenied => c::PERMISSION_DENIED,
            io::ErrorKind::AlreadyExists => c::ALREADY_EXISTS,
            io::ErrorKind::InvalidInput => c::INVALID_VALUE,
            io::ErrorKind::TimedOut => c::TIMEOUT,
            _ => c::INTERNAL,
        },
    }
}

/// An I/O error that knows its rynas error code (a server refusing the login, a changed
/// host key): [`io_code`] reads it back.
#[derive(Debug)]
pub struct Coded {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for Coded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Coded {}

/// `io::Error` of `kind` with a rynas error `code` and an English message.
pub fn coded(kind: io::ErrorKind, code: &'static str, message: String) -> io::Error {
    io::Error::new(kind, Coded { code, message })
}

/// An I/O error in words (English) for the message next to the code.
pub fn describe(e: &io::Error) -> String {
    match e.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => t!("permission denied"),
        Some(libc::ENOENT) => t!("no longer exists"),
        Some(libc::EEXIST) => t!("already exists"),
        Some(libc::ENOSPC) => t!("the volume is full"),
        Some(libc::EDQUOT) => t!("disk quota exceeded"),
        Some(libc::EROFS) => t!("the volume is read-only"),
        Some(libc::ENOTEMPTY) => t!("the folder is not empty"),
        Some(libc::ENAMETOOLONG) => t!("name too long"),
        Some(libc::EISDIR) => t!("is a folder"),
        Some(libc::ENOTDIR) => t!("not a folder"),
        Some(libc::EXDEV) => t!("different volumes"),
        Some(libc::EIO) => t!("disk read or write error"),
        Some(libc::EBUSY) => t!("in use"),
        Some(libc::ELOOP) => t!("too many symbolic links"),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        let p = Pattern::new("fattura 2026");
        assert!(p.matches("Fattura_luce_2026.pdf"));
        assert!(!p.matches("fattura 2025.pdf"));
        assert!(Pattern::new("mkv").matches("Il Padrino (1972).MKV"));
        assert!(!Pattern::new("mkv").matches("trailer.mp4"));
        let g = Pattern::new("*.JPG");
        assert!(g.matches("IMG_0001.jpg"));
        assert!(!g.matches("IMG_0001.jpeg"));
        assert!(Pattern::new("img_00??.*").matches("IMG_0042.heic"));
        assert!(!Pattern::new("  ").matches("x"));
    }

    #[test]
    fn without_faccessat2_the_powers_count_but_never_on_a_read_only_volume() {
        let ro = || true;
        let rw = || false;
        // what the user may do anyway
        assert!(access_without_faccessat2(true, true, true, ro));
        // an administrator's worker: any existing item, writable where the volume is
        assert!(access_without_faccessat2(false, true, true, rw));
        assert!(!access_without_faccessat2(false, true, true, ro));
        // reading or opening on a read-only volume is fine
        assert!(access_without_faccessat2(false, true, false, ro));
        // nothing there: nothing to do
        assert!(!access_without_faccessat2(false, false, false, rw));
    }

    #[test]
    fn an_ordinary_worker_asks_the_real_permissions() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: plain getter
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let locked = t.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        assert!(!crate::caps::writes_over_owners());
        assert!(access(t.path(), libc::W_OK | libc::X_OK));
        assert!(access(&locked, libc::R_OK | libc::X_OK));
        assert!(!access(&locked, libc::W_OK | libc::X_OK));
        assert!(!access(&t.path().join("missing"), libc::R_OK));
        assert!(!read_only_mount(t.path()));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The trap of access(2) for an administrator's worker, checked in an unprivileged user
    /// namespace: there the test runs as its own uid (not 0) with every capability effective
    /// (`unshare --keep-caps`), as a worker with `CAP_DAC_OVERRIDE` does. access(2) says a
    /// folder of mode 0555 is not writable (the kernel clears the capabilities of a uid other
    /// than 0 for it), [`access`] says it is (faccessat2 `AT_EACCESS`), and a read-only
    /// mount stays not writable (EROFS before any capability). Skipped where user namespaces
    /// or `unshare` are missing.
    #[test]
    fn an_administrators_worker_asks_with_its_powers() {
        use std::os::unix::fs::PermissionsExt;
        const STEP: &str = "RYNAS_TEST_ACCESS_DIR";
        if let Ok(dir) = std::env::var(STEP) {
            // inside the namespace
            let dir = Path::new(&dir);
            let (locked, ro) = (dir.join("locked"), dir.join("ro"));
            assert!(crate::caps::writes_over_owners(), "no CAP_DAC_OVERRIDE in the namespace");
            let c = CString::new(locked.as_os_str().as_bytes()).unwrap();
            // SAFETY: valid C string
            assert_ne!(unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) }, 0, "access(2) honoured the capabilities");
            assert!(access(&locked, libc::W_OK | libc::X_OK));
            let mounted = std::process::Command::new("mount").args(["--bind", "-o", "ro"]).arg(&ro).arg(&ro).status().is_ok_and(|s| s.success());
            if mounted {
                assert!(read_only_mount(&ro));
                assert!(!access(&ro, libc::W_OK | libc::X_OK));
                assert!(access(&ro, libc::R_OK | libc::X_OK));
            } else {
                eprintln!("no bind mount in the namespace: the read-only part is skipped");
            }
            return;
        }
        // SAFETY: plain getter
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let (locked, ro) = (t.path().join("locked"), t.path().join("ro"));
        fs::create_dir(&locked).unwrap();
        fs::create_dir(&ro).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new("unshare")
            .args(["--user", "--map-current-user", "--mount", "--keep-caps", "--"])
            .arg(&exe)
            .args(["--exact", "fsops::tests::an_administrators_worker_asks_with_its_powers", "--nocapture", "--test-threads=1"])
            .env(STEP, t.path())
            .output();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let Ok(out) = out else {
            eprintln!("no unshare: skipped");
            return;
        };
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        if !out.status.success() && (text.contains("unshare failed") || text.contains("Operation not permitted") || text.contains("unrecognized option")) {
            eprintln!("no user namespace here: skipped ({})", text.trim());
            return;
        }
        assert!(out.status.success() && text.contains("1 passed"), "{text}");
    }

    #[test]
    fn rename_never_replaces() {
        let dir = std::env::temp_dir().join(format!("rynas-files-rename-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a"), "a").unwrap();
        fs::write(dir.join("b"), "b").unwrap();
        assert_eq!(rename_noreplace(&dir.join("a"), &dir.join("b")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        rename_noreplace(&dir.join("a"), &dir.join("c")).unwrap();
        assert_eq!(fs::read_to_string(dir.join("c")).unwrap(), "a");
        fs::remove_dir_all(dir).unwrap();
    }
}
