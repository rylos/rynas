//! btrfs: read-only snapshots of every mounted subvolume and of the subvolumes nested in
//! them (abb-agent's `btrfs-image` rules: snapper/Timeshift snapshot folders and container
//! engines' subvolumes are not PC data), the default subvolume, cleanup of leftovers. Uses
//! the system's btrfs-progs.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The folder of the agent's own snapshots, inside each snapshotted subvolume.
pub const SNAP_DIR: &str = ".rynas-snap";

fn btrfs(args: &[&std::ffi::OsStr]) -> Result<String, String> {
    let o = Command::new("btrfs")
        .args(args)
        .output()
        .map_err(|e| format!("btrfs: {e} (install btrfs-progs)"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    } else {
        Err(format!("btrfs {}: {}", args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "), String::from_utf8_lossy(&o.stderr).trim()))
    }
}

/// Excluded subvolumes a restore makes again, empty (snapper's `.snapshots`, btrbk's
/// snapshot folders): container engines' layers, Timeshift's and the agent's own snapshots
/// are not made again.
pub fn recreated_on_restore(rel: &str) -> bool {
    !(rel.contains("docker/btrfs") || rel.contains("containers/storage") || rel.contains("timeshift-btrfs") || rel.split('/').any(|c| c == SNAP_DIR))
}

/// Nested subvolumes never backed up (some are made again empty at restore:
/// [`recreated_on_restore`]).
pub fn excluded_subvolume(rel: &str) -> bool {
    let last = rel.rsplit('/').next().unwrap_or(rel);
    // snapper's folder, and anything inside it
    rel.split('/').any(|c| c == ".snapshots")
        || last == SNAP_DIR
        // btrbk's local snapshots (the usual folder names and the agent's default)
        || rel.split('/').any(|c| c == "btrbk_snapshots" || c == ".btrbk_snapshots")
        || rel.contains("timeshift-btrfs")
        || rel.contains("var/lib/docker/btrfs")
        || rel.contains("var/lib/containers/storage")
        || rel.starts_with(SNAP_DIR)
        || rel.contains(&format!("/{SNAP_DIR}/"))
}

/// Subvolumes nested below the subvolume mounted at `mount`, as paths relative to it (with
/// their ids), from `btrfs subvolume list -o` (direct children) on it and on each child, down
/// to `max_depth` levels. Errors give an empty list ([`nested_checked`] tells them).
pub fn nested(mount: &Path, max_depth: usize) -> Vec<(String, u64)> {
    nested_checked(mount, max_depth, &|rel| !excluded_subvolume(rel)).unwrap_or_default()
}

/// [`nested`] with its errors; `descend(rel)` says whether to look inside a child (snapshot
/// folders hold many subvolumes and are never sent: no need to list them).
pub fn nested_checked(mount: &Path, max_depth: usize, descend: &dyn Fn(&str) -> bool) -> Result<Vec<(String, u64)>, String> {
    let mut v = Vec::new();
    if max_depth == 0 {
        return Ok(v);
    }
    let out = btrfs(&["subvolume".as_ref(), "list".as_ref(), "-o".as_ref(), mount.as_os_str()])?;
    let base = btrfs(&["subvolume".as_ref(), "show".as_ref(), mount.as_os_str()])?.lines().next().map(|l| l.trim().to_string()).unwrap_or_default();
    let base = if base == "/" || base.starts_with('<') { String::new() } else { base.trim_start_matches("<FS_TREE>/").trim_start_matches('/').to_string() };
    for l in out.lines() {
        // ID 258 gen 10 top level 256 path @/var/lib/machines
        let id = l.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let Some(path) = l.split(" path ").nth(1) else { continue };
        let path = path.trim().trim_start_matches("<FS_TREE>/");
        let rel = if base.is_empty() {
            path.to_string()
        } else {
            match path.strip_prefix(&format!("{base}/")) {
                Some(r) => r.to_string(),
                None => continue,
            }
        };
        if rel.is_empty() || rel.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
            continue;
        }
        if max_depth > 1 && descend(&rel) {
            for (r, i) in nested_checked(&mount.join(&rel), max_depth - 1, &|x| descend(&format!("{rel}/{x}")))? {
                v.push((format!("{rel}/{r}"), i));
            }
        }
        v.push((rel, id));
    }
    v.sort();
    Ok(v)
}

/// The default subvolume as a path from the top level (`None`: the top level itself).
pub fn default_subvol_path(mount: &Path) -> Option<String> {
    let out = btrfs(&["subvolume".as_ref(), "get-default".as_ref(), mount.as_os_str()]).ok()?;
    // ID 256 gen 10 top level 5 path @
    let p = out.split(" path ").nth(1)?.trim().to_string();
    (!p.is_empty() && out.split_whitespace().nth(1) != Some("5")).then_some(p)
}

/// A read-only snapshot of the subvolume at `src` into `<parent>/.rynas-snap/<name>`.
pub fn snapshot(src: &Path, snap_parent: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = snap_parent.join(SNAP_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let dst = dir.join(name);
    btrfs(&["subvolume".as_ref(), "snapshot".as_ref(), "-r".as_ref(), src.as_os_str(), dst.as_os_str()])?;
    Ok(dst)
}

pub fn delete(p: &Path) -> Result<(), String> {
    btrfs(&["subvolume".as_ref(), "delete".as_ref(), p.as_os_str()]).map(|_| ())
}

/// Removes snapshots an interrupted run left in `<mount>/.rynas-snap/` (only the agent's
/// own names: `run-…`).
pub fn cleanup_leftovers(mount: &Path) {
    let dir = mount.join(SNAP_DIR);
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        if e.file_name().to_string_lossy().starts_with("run-") {
            match delete(&e.path()) {
                Ok(()) => eprintln!("{}: removed (left by an interrupted backup)", e.path().display()),
                Err(err) => eprintln!("{err}"),
            }
        }
    }
    let _ = std::fs::remove_dir(&dir);
}

/// A read-only snapshot of the subvolume at `src` at exactly `dst`.
pub fn snapshot_ro(src: &Path, dst: &Path) -> Result<(), String> {
    btrfs(&["subvolume".as_ref(), "snapshot".as_ref(), "-r".as_ref(), src.as_os_str(), dst.as_os_str()]).map(|_| ())
}

/// What `BTRFS_IOC_GET_SUBVOL_INFO` says about a subvolume (unprivileged, kernel ≥ 4.18).
pub use crate::ioctl::SubvolInfo;

/// `/` is on btrfs (the btrfs method can serve this PC).
pub fn root_is_btrfs() -> bool {
    crate::ioctl::open_dir(Path::new("/")).is_ok_and(|f| crate::ioctl::is_btrfs(&f))
}

/// The facts of the subvolume whose top folder is `p` (not following a final symlink);
/// `None` when it is not a subvolume or the ioctl fails.
pub fn subvol_info(p: &Path) -> Option<SubvolInfo> {
    let f = crate::ioctl::open_dir(p).ok()?;
    if !crate::ioctl::is_subvolume(&f) {
        return None;
    }
    crate::ioctl::subvol_info(&f).ok()
}

/// The UUID of the btrfs filesystem mounted at (or holding) `p`.
pub fn fs_uuid(p: &Path) -> Option<String> {
    crate::ioctl::open_dir(p).ok().and_then(|f| crate::ioctl::fs_uuid(&f).ok())
}

/// A plain folder (not a subvolume, not a symlink) with nothing in it: the place a nested
/// subvolume leaves in its parent's snapshot.
pub fn is_empty_plain_dir(p: &Path) -> bool {
    let Ok(f) = crate::ioctl::open_dir(p) else { return false };
    !crate::ioctl::is_subvolume(&f) && std::fs::read_dir(p).is_ok_and(|mut r| r.next().is_none())
}

/// Moves the read-only subvolume `name` of folder `from` to `to_name` in folder `to` (same
/// filesystem, never replacing anything, no symlink followed in either name): a read-only
/// subvolume cannot change parent folder, so the flag is lifted for the moment of the move
/// and set again on the same subvolume (by descriptor) wherever it is then. The ioctl keeps
/// the received UUID, unlike `btrfs property set ro false`: a received snapshot stays the
/// parent of incremental sends (the NAS moves its own received snapshots the same way).
pub fn move_readonly(from: &Path, name: &str, to: &Path, to_name: &str) -> Result<(), String> {
    use crate::ioctl;
    let at = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let fd = ioctl::open_dir(from).map_err(|e| at(from, e))?;
    let td = ioctl::open_dir(to).map_err(|e| at(to, e))?;
    let sub = ioctl::open_dir_at(&fd, name).map_err(|e| at(&from.join(name), e))?;
    if !ioctl::is_subvolume(&sub) {
        return Err(format!("{}: not a subvolume", from.join(name).display()));
    }
    ioctl::set_readonly(&sub, false).map_err(|e| at(&from.join(name), e))?;
    let moved = ioctl::rename_noreplace_at(&fd, name, &td, to_name).map_err(|e| format!("moving {} to {}: {e}", from.join(name).display(), to.join(to_name).display()));
    let ro = ioctl::set_readonly(&sub, true).map_err(|e| at(&to.join(to_name), e));
    moved.and(ro)
}

/// `FS_IOC_GETFLAGS` / `FS_IOC_SETFLAGS` (`_IOR/_IOW('f', 1/2, long)`) and `FS_NOCOW_FL`.
const FS_IOC_GETFLAGS: u64 = (2 << 30) | (8 << 16) | ((b'f' as u64) << 8) | 1;
const FS_IOC_SETFLAGS: u64 = (1 << 30) | (8 << 16) | ((b'f' as u64) << 8) | 2;
const FS_NOCOW_FL: libc::c_int = 0x0080_0000;

/// `chattr +C` on an empty folder or file (new files inside inherit it).
pub fn set_nocow(p: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(p)
        .map_err(|e| format!("{}: {e}", p.display()))?;
    let mut flags: libc::c_int = 0;
    // SAFETY: the kernel reads and writes one int
    unsafe {
        if libc::ioctl(f.as_raw_fd(), FS_IOC_GETFLAGS as _, &mut flags) < 0 {
            return Err(format!("{}: {}", p.display(), std::io::Error::last_os_error()));
        }
        flags |= FS_NOCOW_FL;
        if libc::ioctl(f.as_raw_fd(), FS_IOC_SETFLAGS as _, &flags) < 0 {
            return Err(format!("{}: {}", p.display(), std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// A writable snapshot of `src` at `dst` (`dst` must not exist).
pub fn snapshot_rw(src: &Path, dst: &Path) -> Result<(), String> {
    btrfs(&["subvolume".as_ref(), "snapshot".as_ref(), src.as_os_str(), dst.as_os_str()]).map(|_| ())
}

/// An empty subvolume at `p`.
pub fn create(p: &Path) -> Result<(), String> {
    btrfs(&["subvolume".as_ref(), "create".as_ref(), p.as_os_str()]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions() {
        assert!(excluded_subvolume(".snapshots"));
        assert!(excluded_subvolume("home/.snapshots"));
        assert!(excluded_subvolume("var/lib/docker/btrfs/subvolumes/abc"));
        assert!(excluded_subvolume(".rynas-snap/run-1"));
        assert!(excluded_subvolume(".btrbk_snapshots/@.20261112T0300"));
        assert!(!excluded_subvolume("var/lib/machines"));
        assert!(!excluded_subvolume("srv"));
    }

    #[test]
    fn plain_folders() {
        // a plain folder is no subvolume (the ioctl layouts are tested in `ioctl`)
        let d = tempfile::tempdir().unwrap();
        assert_eq!(subvol_info(d.path()), None);
        assert!(is_empty_plain_dir(d.path()));
        std::fs::write(d.path().join("f"), b"x").unwrap();
        assert!(!is_empty_plain_dir(d.path()));
        std::os::unix::fs::symlink("/", d.path().join("l")).unwrap();
        assert!(!is_empty_plain_dir(&d.path().join("l")), "a symlink is not followed");
        assert!(recreated_on_restore("@/.snapshots"));
        assert!(!recreated_on_restore("@/var/lib/docker/btrfs/subvolumes/x"));
        assert!(!recreated_on_restore("@docker/btrfs/subvolumes/x"));
        assert!(!recreated_on_restore("@/.rynas-snap/run-1"));
    }
}
