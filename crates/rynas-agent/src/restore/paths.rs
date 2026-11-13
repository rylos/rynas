//! Paths of the manifest and of the restored tree. The manifest and the restored files come
//! from the PC: a path with `..` or a symlink inside the restored system must never reach
//! the live system the restore runs on.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// The path rules shared with the backup side.
pub use crate::mounts::{safe_rel, under};

/// Most symlinks followed on one path (Linux' own limit).
const MAX_LINKS: usize = 40;

/// `rel` inside `root` as a chroot sees it: every symlink on the way is followed **inside**
/// `root` (an absolute target starts again at `root`, `..` stops at it), so the result is
/// a path below `root` with no symlink in it. What does not exist yet stays as written (to
/// be made). The restored system is the restore's own (nothing races with it); the point
/// is that its links (NixOS' `/etc/fstab -> /etc/static/fstab`, a `/home -> /data/home`)
/// lead where they lead on the PC, never into the live system.
pub fn resolve_in_root(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let mut todo: Vec<OsString> = Vec::new();
    push_parts(&mut todo, Path::new(rel));
    let mut done: Vec<OsString> = Vec::new();
    let mut links = 0;
    while let Some(c) = todo.pop() {
        if c == ".." {
            done.pop();
            continue;
        }
        let mut p = root.to_path_buf();
        p.extend(&done);
        p.push(&c);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                links += 1;
                if links > MAX_LINKS {
                    return Err(format!("{rel}: too many symlinks in the restored system"));
                }
                let t = std::fs::read_link(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                if t.is_absolute() {
                    done.clear();
                }
                push_parts(&mut todo, &t);
            }
            _ => done.push(c),
        }
    }
    let mut p = root.to_path_buf();
    p.extend(done);
    Ok(p)
}

/// The components of `p` onto the stack `todo`, last first (`.` and `/` dropped).
fn push_parts(todo: &mut Vec<OsString>, p: &Path) {
    let parts: Vec<OsString> = p
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_os_string()),
            Component::ParentDir => Some(OsString::from("..")),
            _ => None,
        })
        .collect();
    todo.extend(parts.into_iter().rev());
}

/// `rel` (a [`safe_rel`] path) below the top level of a btrfs pool, refusing a symlink on
/// the way: the path of a subvolume from the top level never goes through one, and the
/// received data could hold one that points into the live system.
pub fn in_pool(top: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = safe_rel(rel).ok_or_else(|| format!("{rel}: not a plain path below the top level"))?;
    let mut p = top.to_path_buf();
    for c in rel.split('/').filter(|c| !c.is_empty()) {
        p.push(c);
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("{}: a symlink inside the pool", p.display()));
        }
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_only() {
        assert_eq!(safe_rel("/@/var/lib/").as_deref(), Some("@/var/lib"));
        assert_eq!(safe_rel("/").as_deref(), Some(""));
        assert_eq!(safe_rel("@/../../etc"), None);
        assert_eq!(safe_rel("a//b"), None);
        assert_eq!(safe_rel("./a"), None);
        assert_eq!(safe_rel("a\0b"), None);
    }

    #[test]
    fn prefixes() {
        assert!(under("/home", "/home/me"));
        assert!(under("/home/", "/home"));
        assert!(!under("/home", "/homework"));
        assert!(under("/", "/anything"));
        assert!(under("@", "@/var/lib"));
        assert!(!under("@", "@home"));
        assert!(under("", "@home"));
    }

    #[test]
    fn links_stay_inside_the_root() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("etc/static")).unwrap();
        std::fs::write(root.join("etc/static/fstab"), b"x").unwrap();
        std::os::unix::fs::symlink("/etc/static/fstab", root.join("etc/fstab")).unwrap();
        assert_eq!(resolve_in_root(root, "/etc/fstab").unwrap(), root.join("etc/static/fstab"));
        // an absolute link to the live system's `/` stays in the root
        std::os::unix::fs::symlink("/", root.join("home")).unwrap();
        assert_eq!(resolve_in_root(root, "/home/me").unwrap(), root.join("me"));
        // `..` stops at the root
        std::os::unix::fs::symlink("../../../../etc", root.join("up")).unwrap();
        assert_eq!(resolve_in_root(root, "up/passwd").unwrap(), root.join("etc/passwd"));
        // a loop is an error, never a path outside
        std::os::unix::fs::symlink("loop", root.join("loop")).unwrap();
        assert!(resolve_in_root(root, "loop/x").is_err());
        // missing parts stay as written
        assert_eq!(resolve_in_root(root, "/var/log/x.log").unwrap(), root.join("var/log/x.log"));
        // in a pool, a symlink is refused
        assert!(in_pool(root, "home/x").is_err());
        assert!(in_pool(root, "../x").is_err());
        assert_eq!(in_pool(root, "etc/static").unwrap(), root.join("etc/static"));
    }
}
