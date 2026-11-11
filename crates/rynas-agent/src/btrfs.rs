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

/// Nested subvolumes never backed up (recreated empty at restore).
pub fn excluded_subvolume(rel: &str) -> bool {
    let last = rel.rsplit('/').next().unwrap_or(rel);
    last == ".snapshots"
        || last == SNAP_DIR
        || rel.contains("timeshift-btrfs")
        || rel.contains("var/lib/docker/btrfs")
        || rel.contains("var/lib/containers/storage")
        || rel.starts_with(SNAP_DIR)
        || rel.contains(&format!("/{SNAP_DIR}/"))
}

/// Subvolumes nested below the subvolume mounted at `mount`, as paths relative to it (with
/// their ids), from `btrfs subvolume list -o`.
pub fn nested(mount: &Path, max_depth: usize) -> Vec<(String, u64)> {
    let Ok(out) = btrfs(&["subvolume".as_ref(), "list".as_ref(), "-o".as_ref(), mount.as_os_str()]) else {
        return Vec::new();
    };
    let base = btrfs(&["subvolume".as_ref(), "show".as_ref(), mount.as_os_str()])
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .unwrap_or_default();
    let base = if base == "/" || base.starts_with('<') { String::new() } else { base.trim_start_matches('/').to_string() };
    let mut v = Vec::new();
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
        if rel.split('/').count() <= max_depth {
            v.push((rel, id));
        }
    }
    v.sort();
    v
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions() {
        assert!(excluded_subvolume(".snapshots"));
        assert!(excluded_subvolume("home/.snapshots"));
        assert!(excluded_subvolume("var/lib/docker/btrfs/subvolumes/abc"));
        assert!(excluded_subvolume(".rynas-snap/run-1"));
        assert!(!excluded_subvolume("var/lib/machines"));
        assert!(!excluded_subvolume("srv"));
    }
}
