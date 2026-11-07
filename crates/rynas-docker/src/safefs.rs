//! Descriptor-based file operations for root writing into folders that another user can
//! write (stack folders, bind folders, Caddy's state folder).
//!
//! The rules (review 2026-11-07, H1):
//! - paths are walked one component at a time with `openat(O_DIRECTORY|O_NOFOLLOW)`: a
//!   symlink anywhere below the trusted start refuses the operation (`ELOOP`/`ENOTDIR`);
//! - a new folder is made with `mkdirat`, opened again without following, and gets its
//!   owner and mode through `fchown`/`fchmod` on the descriptor;
//! - a file is written to a fresh temporary name (`O_CREAT|O_EXCL|O_NOFOLLOW`, after an
//!   `unlinkat` of a leftover), owner and mode are set on the descriptor before any byte is
//!   written, and `renameat` puts it in place (a rename replaces a planted symlink, it never
//!   follows it).
//!
//! No path-based `chown`, `chmod` or `open(O_CREAT)` is used anywhere below the start.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;

/// `(uid, gid)` to give, `None`: keep the creator's (tests, `--dry-system`).
pub type Owner = Option<(u32, u32)>;

fn cstr(name: &str) -> io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid path component {name:?}"),
        ));
    }
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}

fn check(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Opens the trusted start of a walk (`/`, a dry root, a temporary folder): it may be a
/// symlink itself, nothing below it may.
pub fn open_start(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
}

/// Opens the folder `name` of `dir`, refusing a symlink.
pub fn open_dir_at(dir: &File, name: &str) -> io::Result<File> {
    let c = cstr(name)?;
    // SAFETY: valid descriptor and C string; the new descriptor is owned by the File
    let fd = check(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })?;
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// The components of an absolute or relative path, refusing `.`, `..` and empty parts.
pub fn components(p: &str) -> io::Result<Vec<&str>> {
    let parts: Vec<&str> = p.trim_start_matches('/').split('/').collect();
    if parts.iter().any(|x| x.is_empty() || *x == "." || *x == "..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid path {p:?}"),
        ));
    }
    Ok(parts)
}

/// Opens the existing folder `abs` below `start` without following any symlink below it.
pub fn open_dir(start: &Path, abs: &str) -> io::Result<File> {
    let mut d = open_start(start)?;
    if abs.trim_matches('/').is_empty() {
        return Ok(d);
    }
    for c in components(abs)? {
        d = open_dir_at(&d, c)?;
    }
    Ok(d)
}

/// Sets owner (when given) and mode on an open descriptor.
pub fn set_owner_mode(f: &File, mode: u32, owner: Owner) -> io::Result<()> {
    if let Some((u, g)) = owner {
        // SAFETY: valid descriptor
        check(unsafe { libc::fchown(f.as_raw_fd(), u, g) })?;
    }
    // SAFETY: valid descriptor
    check(unsafe { libc::fchmod(f.as_raw_fd(), mode as libc::mode_t) })?;
    Ok(())
}

/// The folder `name` of `dir`: opened when it exists (a symlink is refused), else made
/// with `mode` and `owner` on its descriptor. `true` when it was made.
pub fn ensure_dir_at(dir: &File, name: &str, mode: u32, owner: Owner) -> io::Result<(File, bool)> {
    let c = cstr(name)?;
    // SAFETY: valid descriptor and C string
    let r = unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o700) };
    let made = if r == 0 {
        true
    } else {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EEXIST) {
            return Err(e);
        }
        false
    };
    let d = open_dir_at(dir, name)?;
    if made {
        set_owner_mode(&d, mode, owner)?;
    }
    Ok((d, made))
}

/// Walks `rel` below `dir`, making the missing folders with `mode` and `owner`. Returns the
/// last folder and whether any folder was made.
pub fn ensure_dirs(dir: &File, rel: &str, mode: u32, owner: Owner) -> io::Result<(File, bool)> {
    let mut cur = dir.try_clone()?;
    let mut any = false;
    for c in components(rel)? {
        let (d, made) = ensure_dir_at(&cur, c, mode, owner)?;
        any |= made;
        cur = d;
    }
    Ok((cur, any))
}

fn unlink_at(dir: &File, name: &CString, flags: libc::c_int) -> io::Result<()> {
    // SAFETY: valid descriptor and C string
    check(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) }).map(|_| ())
}

/// The entry names of the open folder `dir` (without `.` and `..`), read through a fresh
/// descriptor of the same folder (`openat(dir, ".")`), never through a path.
pub fn list_at(dir: &File) -> io::Result<Vec<String>> {
    let dot = cstr_dot();
    // SAFETY: valid descriptor and C string; the new descriptor goes to fdopendir
    let fd = check(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })?;
    // SAFETY: fd is a valid directory descriptor; on success the stream owns it
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: fdopendir failed, fd is still ours
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let mut out = Vec::new();
    loop {
        // SAFETY: valid stream; the entry stays valid until the next readdir
        let ent = unsafe { libc::readdir(stream) };
        if ent.is_null() {
            break;
        }
        // SAFETY: d_name is NUL-terminated
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
        let name = name.to_string_lossy();
        if name != "." && name != ".." {
            out.push(name.into_owned());
        }
    }
    // SAFETY: valid stream, closed once
    unsafe { libc::closedir(stream) };
    Ok(out)
}

fn cstr_dot() -> CString {
    CString::new(".").expect("no NUL")
}

/// Removes the entry `name` of `dir`: a file or a symlink is unlinked (a symlink is never
/// followed), a folder is emptied the same way, one descriptor at a time
/// (`O_DIRECTORY|O_NOFOLLOW`), then removed. A missing entry is fine. Nothing outside `dir`
/// is ever touched: a folder replaced by a symlink midway refuses the removal.
pub fn remove_tree_at(dir: &File, name: &str) -> io::Result<()> {
    remove_tree_depth(dir, name, 0)
}

fn remove_tree_depth(dir: &File, name: &str, depth: u32) -> io::Result<()> {
    if depth > 32 {
        return Err(io::Error::other("folder tree too deep"));
    }
    let c = cstr(name)?;
    match unlink_at(dir, &c, 0) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        // Linux answers EISDIR for a folder (POSIX allows EPERM)
        Err(e) if matches!(e.raw_os_error(), Some(libc::EISDIR) | Some(libc::EPERM)) => {}
        Err(e) => return Err(e),
    }
    let sub = open_dir_at(dir, name)?;
    for n in list_at(&sub)? {
        remove_tree_depth(&sub, &n, depth + 1)?;
    }
    drop(sub);
    match unlink_at(dir, &c, libc::AT_REMOVEDIR) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Renames the entry `from` of `dir` to `to` in the same folder (never follows a symlink).
pub fn rename_at(dir: &File, from: &str, to: &str) -> io::Result<()> {
    let (a, b) = (cstr(from)?, cstr(to)?);
    // SAFETY: valid descriptors and C strings
    check(unsafe { libc::renameat(dir.as_raw_fd(), a.as_ptr(), dir.as_raw_fd(), b.as_ptr()) })
        .map(|_| ())
}

/// Writes `content` as `dir/name` with `mode` and `owner`: a fresh temporary file
/// (`O_EXCL|O_NOFOLLOW`), owner and mode on the descriptor, `fsync`, then `renameat`.
pub fn write_at(dir: &File, name: &str, content: &[u8], mode: u32, owner: Owner) -> io::Result<()> {
    let target = cstr(name)?;
    let tmp = cstr(&format!(".{name}.rynas-tmp"))?;
    match unlink_at(dir, &tmp, 0) {
        Err(e) if e.raw_os_error() == Some(libc::EISDIR) => {
            // a folder planted under the temporary name: remove it if empty, else refuse
            unlink_at(dir, &tmp, libc::AT_REMOVEDIR)?;
        }
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    // SAFETY: valid descriptor and C string; the new descriptor is owned by the File
    let fd = check(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            tmp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    })?;
    let mut f = unsafe { File::from_raw_fd(fd) };
    let done = set_owner_mode(&f, mode, owner)
        .and_then(|_| f.write_all(content))
        .and_then(|_| f.sync_all());
    drop(f);
    if let Err(e) = done {
        let _ = unlink_at(dir, &tmp, 0);
        return Err(e);
    }
    // SAFETY: valid descriptors and C strings
    let r = unsafe {
        libc::renameat(dir.as_raw_fd(), tmp.as_ptr(), dir.as_raw_fd(), target.as_ptr())
    };
    if r != 0 {
        let e = io::Error::last_os_error();
        let _ = unlink_at(dir, &tmp, 0);
        return Err(e);
    }
    Ok(())
}

/// Writes `content` to the file `rel` below the folder `dir`: missing folders are made with
/// `dir_mode`/`owner`, symlinks anywhere are refused.
pub fn write_rel(
    dir: &File,
    rel: &str,
    content: &[u8],
    mode: u32,
    dir_mode: u32,
    owner: Owner,
) -> io::Result<()> {
    let parts = components(rel)?;
    let (name, parents) = parts.split_last().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "empty path")
    })?;
    let parent = if parents.is_empty() {
        dir.try_clone()?
    } else {
        ensure_dirs(dir, &parents.join("/"), dir_mode, owner)?.0
    };
    write_at(&parent, name, content, mode, owner)
}

/// Opens the regular file `name` of `dir` for reading without following a symlink.
pub fn open_file_at(dir: &File, name: &str) -> io::Result<File> {
    let c = cstr(name)?;
    // SAFETY: valid descriptor and C string; the new descriptor is owned by the File
    let fd = check(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    })?;
    let f = unsafe { File::from_raw_fd(fd) };
    if !f.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(f)
}

/// Reads the regular file `name` of `dir` (no symlink), at most `max` bytes.
pub fn read_at(dir: &File, name: &str, max: u64) -> io::Result<String> {
    let f = open_file_at(dir, name)?;
    let mut s = String::new();
    f.take(max + 1).read_to_string(&mut s)?;
    if s.len() as u64 > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
    }
    Ok(s)
}

/// `(mode & 0o7777, uid, gid)` of an open descriptor.
pub fn stat_of(f: &File) -> io::Result<(u32, u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let m = f.metadata()?;
    Ok((m.mode() & 0o7777, m.uid(), m.gid()))
}

/// The mode of the entry `name` of `dir` without following a symlink (`None`: missing).
pub fn mode_at(dir: &File, name: &str) -> Option<u32> {
    let c = cstr(name).ok()?;
    // SAFETY: plain old data filled by the kernel
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor, C string and buffer
    let r = unsafe { libc::fstatat(dir.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
    (r == 0).then_some(st.st_mode & 0o7777)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[test]
    fn writes_never_follow_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let victim = t.path().join("victim");
        std::fs::write(&victim, "secret").unwrap();
        let dir = t.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        // a planted temporary name pointing at the victim
        symlink(&victim, dir.join(".f.rynas-tmp")).unwrap();
        let d = open_dir(t.path(), "d").unwrap();
        write_at(&d, "f", b"new", 0o640, None).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "secret");
        assert_eq!(std::fs::read_to_string(dir.join("f")).unwrap(), "new");
        assert_eq!(std::fs::metadata(dir.join("f")).unwrap().mode() & 0o777, 0o640);
        // the target itself a symlink: replaced, not followed
        symlink(&victim, dir.join("g")).unwrap();
        write_at(&d, "g", b"x", 0o600, None).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "secret");
        assert!(!std::fs::symlink_metadata(dir.join("g")).unwrap().is_symlink());
        // a symlinked parent folder is refused
        let outside = t.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, dir.join("sub")).unwrap();
        assert!(write_rel(&d, "sub/x", b"x", 0o644, 0o755, None).is_err());
        assert!(!outside.join("x").exists());
        assert!(open_dir(t.path(), "d/sub").is_err());
        // made folders get the mode
        write_rel(&d, "a/b/c", b"x", 0o644, 0o750, None).unwrap();
        assert_eq!(std::fs::metadata(dir.join("a/b")).unwrap().mode() & 0o777, 0o750);
        assert!(components("a/../b").is_err());
        assert_eq!(mode_at(&d, "f"), Some(0o640));
        assert_eq!(read_at(&d, "f", 10).unwrap(), "new");
        assert!(read_at(&d, "f", 2).is_err());
    }

    #[test]
    fn tree_removal_never_crosses_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let outside = t.path().join("outside");
        std::fs::create_dir_all(outside.join("keep")).unwrap();
        std::fs::write(outside.join("keep/file"), "x").unwrap();
        let dir = t.path().join("d");
        std::fs::create_dir_all(dir.join("tree/sub")).unwrap();
        std::fs::write(dir.join("tree/sub/f"), "y").unwrap();
        symlink(&outside, dir.join("tree/link")).unwrap();
        symlink(&outside, dir.join("top-link")).unwrap();
        let d = open_dir(t.path(), "d").unwrap();
        let mut names = list_at(&d).unwrap();
        names.sort();
        assert_eq!(names, ["top-link", "tree"]);
        remove_tree_at(&d, "tree").unwrap();
        remove_tree_at(&d, "top-link").unwrap();
        remove_tree_at(&d, "missing").unwrap();
        assert!(list_at(&d).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(outside.join("keep/file")).unwrap(), "x");
        std::fs::create_dir(dir.join("a")).unwrap();
        rename_at(&d, "a", "b").unwrap();
        assert!(dir.join("b").is_dir());
    }
}
