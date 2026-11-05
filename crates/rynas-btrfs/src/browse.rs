//! Files inside a snapshot, reached by descriptor.
//!
//! From btrbk-nas `browse.rs` (MIT, same author). Nothing goes by path: the snapshot is
//! opened once and checked to be a read-only subvolume, everything below it is reached one
//! name at a time from the directory already open (`O_PATH|O_NOFOLLOW`), never through a
//! symlink and never leaving the snapshot's subvolume (`st_dev`). Swapping entries cannot
//! lead anywhere else.

use std::ffi::{CString, OsStr};
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rynas_api_types::snapshots::{EntryKind, SnapshotEntry};

use crate::ioctl;
use crate::scan::SnapError;

/// Listing cap: a bigger directory is cut (first names in order) and says so.
pub const MAX_ENTRIES: usize = 10_000;

/// Something inside a snapshot, held open.
#[derive(Debug)]
pub struct Item {
    pub name: String,
    /// `O_PATH` handle of this very inode (the snapshot root is a normal descriptor).
    pub handle: File,
    pub meta: Metadata,
}

impl Item {
    /// Opens the item for reading, if it is a regular file.
    pub fn open_file(&self) -> io::Result<File> {
        if !self.meta.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
        }
        File::open(fd_path(&self.handle))
    }
}

/// The descriptor's own inode, reopened through procfs.
pub fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// `name` in `dir` as an `O_PATH` handle; a symlink is the link itself.
pub fn handle_in(dir: &File, name: &OsStr) -> io::Result<File> {
    let c = CString::new(name.as_bytes()).map_err(io::Error::other)?;
    // SAFETY: valid descriptor and C string
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor we own
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Splits a relative path into names, refusing `.`, `..`, NUL and absolute paths.
pub fn components(rel: &str) -> Result<Vec<&str>, SnapError> {
    if rel.contains('\0') {
        return Err(SnapError::Invalid("invalid path".into()));
    }
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    if parts.iter().any(|p| *p == "." || *p == "..") {
        return Err(SnapError::Invalid("invalid path".into()));
    }
    Ok(parts)
}

/// Opens snapshot `path` and checks it is a read-only subvolume.
pub fn open_snapshot(path: &Path) -> Result<File, SnapError> {
    let f = ioctl::open_dir(path)
        .map_err(|_| SnapError::NotFound(format!("snapshot {} not found", path.display())))?;
    let ro = ioctl::is_subvolume(&f) && ioctl::subvol_info(&f).is_ok_and(|i| i.readonly);
    if !ro {
        return Err(SnapError::Refused(
            "not a read-only snapshot (an interrupted receive, or not a subvolume)".into(),
        ));
    }
    Ok(f)
}

/// Walks `rel` from the open snapshot root, one name at a time.
pub fn resolve_in(root: File, root_name: &str, rel: &str) -> Result<Item, SnapError> {
    let root_meta = root.metadata()?;
    let mut item = Item {
        name: root_name.to_string(),
        handle: root,
        meta: root_meta.clone(),
    };
    for part in components(rel)? {
        if !item.meta.is_dir() {
            return Err(SnapError::Invalid(
                "the path goes through something that is not a folder".into(),
            ));
        }
        let handle = handle_in(&item.handle, OsStr::new(part))
            .map_err(|_| SnapError::NotFound(format!("{rel} is not in this snapshot")))?;
        let meta = handle.metadata()?;
        if meta.dev() != root_meta.dev() {
            // a nested subvolume or a mount point: not part of this snapshot
            return Err(SnapError::Invalid("the path leaves the snapshot".into()));
        }
        item = Item {
            name: part.to_string(),
            handle,
            meta,
        };
    }
    Ok(item)
}

/// Resolves `rel` inside the read-only snapshot at `path`.
pub fn resolve(path: &Path, rel: &str) -> Result<Item, SnapError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    resolve_in(open_snapshot(path)?, &name, rel)
}

pub fn entry_kind(meta: &Metadata) -> EntryKind {
    let t = meta.file_type();
    if t.is_dir() {
        EntryKind::Dir
    } else if t.is_file() {
        EntryKind::File
    } else if t.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    }
}

/// Entries of the directory `dir` (folders first, then by name, case-insensitive), at most
/// [`MAX_ENTRIES`]; returns whether the list was cut.
pub fn list(dir: &Item) -> Result<(Vec<SnapshotEntry>, bool), SnapError> {
    if !dir.meta.is_dir() {
        return Err(SnapError::Invalid("not a folder".into()));
    }
    let base = fd_path(&dir.handle);
    let mut found = Vec::new();
    for e in std::fs::read_dir(&base)? {
        let e = e?;
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        found.push((!is_dir, e.file_name().to_string_lossy().to_lowercase(), e));
    }
    let truncated = found.len() > MAX_ENTRIES;
    found.sort_unstable_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    found.truncate(MAX_ENTRIES);
    let mut out = Vec::with_capacity(found.len());
    for (_, _, e) in found {
        // lstat: entries are never followed
        let Ok(meta) = e.metadata() else { continue };
        let kind = entry_kind(&meta);
        let link = (kind == EntryKind::Symlink)
            .then(|| std::fs::read_link(e.path()).ok())
            .flatten()
            .map(|l| l.to_string_lossy().into_owned());
        out.push(SnapshotEntry {
            name: e.file_name().to_string_lossy().into_owned(),
            kind,
            size_bytes: meta.len(),
            mtime_ms: (meta.mtime().max(0) as u64) * 1000
                + meta.mtime_nsec().max(0) as u64 / 1_000_000,
            mode: meta.mode() & 0o7777,
            uid: meta.uid(),
            gid: meta.gid(),
            link,
        });
    }
    Ok((out, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_stays_inside() {
        let d = tempfile::tempdir().unwrap();
        let snap = d.path().join("docs.20261105T1200");
        std::fs::create_dir_all(snap.join("alice/docs")).unwrap();
        std::fs::write(snap.join("alice/docs/a.txt"), "a").unwrap();
        std::os::unix::fs::symlink("/etc", snap.join("alice/etc")).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", snap.join("alice/host")).unwrap();
        let at = |rel: &str| resolve_in(File::open(&snap).unwrap(), "docs.20261105T1200", rel);

        let item = at("alice/docs/a.txt").unwrap();
        assert_eq!(item.name, "a.txt");
        assert_eq!(
            std::fs::read_to_string(fd_path(&item.open_file().unwrap())).unwrap(),
            "a"
        );
        assert_eq!(at("").unwrap().name, "docs.20261105T1200");
        assert!(matches!(at("alice/../.."), Err(SnapError::Invalid(_))));
        assert!(at("alice/etc").unwrap().meta.is_symlink());
        assert!(matches!(at("alice/etc/passwd"), Err(SnapError::Invalid(_))));
        assert!(at("alice/host").unwrap().open_file().is_err());
        assert!(matches!(at("alice/nothing"), Err(SnapError::NotFound(_))));
        // a plain directory is not a read-only snapshot
        assert!(matches!(resolve(&snap, ""), Err(SnapError::Refused(_))));
        assert!(matches!(
            resolve(&d.path().join("nope"), ""),
            Err(SnapError::NotFound(_))
        ));

        let (entries, cut) = list(&at("alice").unwrap()).unwrap();
        assert!(!cut);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["docs", "etc", "host"]);
        assert_eq!(entries[1].kind, EntryKind::Symlink);
        assert_eq!(entries[1].link.as_deref(), Some("/etc"));
        assert!(list(&item).is_err());
    }
}
