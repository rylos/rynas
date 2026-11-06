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
    let link_dir = kind == 'l' && fs::metadata(path).is_ok_and(|m| m.is_dir());
    Entry { name, kind, size: if kind == 'd' { 0 } else { meta.len() }, mtime: meta.mtime(), mode: meta.mode() & 0o7777, link_dir, thumb: false, readonly: kind == 'd' && readonly_subvolume(path, meta), full: None }
}

/// Whether a folder is the top of a read-only btrfs subvolume: a snapshot
/// (btrbk's, DSM's #snapshot). Only subvolume tops (inode 256) are asked.
fn readonly_subvolume(path: &Path, meta: &Metadata) -> bool {
    const BTRFS_FIRST_FREE_OBJECTID: u64 = 256;
    // _IOR(0x94, 25, u64)
    const BTRFS_IOC_SUBVOL_GETFLAGS: libc::c_ulong = 0x8008_9419;
    const BTRFS_SUBVOL_RDONLY: u64 = 1 << 1;
    if meta.ino() != BTRFS_FIRST_FREE_OBJECTID {
        return false;
    }
    let Ok(dir) = fs::File::open(path) else { return false };
    let mut flags: u64 = 0;
    // SAFETY: an open directory and a u64 for the kernel to fill
    let rc = unsafe { libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(&dir), BTRFS_IOC_SUBVOL_GETFLAGS as _, &mut flags) };
    rc == 0 && flags & BTRFS_SUBVOL_RDONLY != 0
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
    for item in fs::read_dir(dir)?.flatten() {
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
    fs::read_dir(bin).is_ok_and(|d| d.flatten().any(|i| !is_system_name(&i.file_name().to_string_lossy())))
}

pub fn access(path: &Path, mode: i32) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return false };
    // SAFETY: valid C string
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
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
    pub readable: bool,
    pub writable: bool,
    pub device: u64,
}

pub fn details(path: &Path) -> Result<Details, ApiError> {
    let meta = fs::symlink_metadata(path)?;
    let target = meta.file_type().is_symlink().then(|| fs::read_link(path).map(|t| t.to_string_lossy().into_owned()).unwrap_or_default());
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
        readable: access(path, libc::R_OK),
        writable: access(path, libc::W_OK),
        device: meta.dev(),
    })
}

pub fn mkdir(path: &Path) -> Result<(), ApiError> {
    fs::create_dir(path).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists { ApiError::conflict(t!("an item with this name already exists")) } else { e.into() }
    })
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
        if e.kind() == io::ErrorKind::AlreadyExists { ApiError::conflict(t!("an item with this name already exists")) } else { e.into() }
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
            match fs::read_dir(&path) {
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
        let Ok(rd) = fs::read_dir(root.join(&rel)) else { continue };
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
