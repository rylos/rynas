//! Emptying the `#recycle` bins (Samba never does it): task `smb.recycle-cleanup`.
//!
//! An item's age is its ctime, which the move into `#recycle` sets (a rename changes the
//! inode's ctime), so the original modification time stays intact for a restore. The walk
//! is done with directory file descriptors (`openat` with `O_NOFOLLOW`, `unlinkat`): users
//! own what is in the bin and could swap a directory for a symlink while rynas, as root,
//! walks it; symlinks are removed, never followed.

use std::ffi::{CStr, CString};
use std::os::fd::AsFd;

use nix::dir::Dir;
use nix::fcntl::{AtFlags, OFlag};
use nix::sys::stat::{Mode, SFlag, fstatat};
use nix::unistd::{UnlinkatFlags, unlinkat};

/// Name of the bin inside a share (`recycle:repository`).
pub const RECYCLE_DIR: &str = "#recycle";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CleanupStats {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

fn open_dir_at(parent: impl AsFd, name: &CStr) -> nix::Result<Dir> {
    Dir::openat(
        parent,
        name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
}

/// Removes what is older than `cutoff_s` (seconds since the epoch) below `dir`; returns
/// whether `dir` is empty afterwards.
fn clean(
    dir: &mut Dir,
    cutoff_s: i64,
    stats: &mut CleanupStats,
    cancelled: &dyn Fn() -> bool,
) -> Result<bool, String> {
    let names: Vec<CString> = dir
        .iter()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_owned())
        .filter(|n| n.as_bytes() != b"." && n.as_bytes() != b"..")
        .collect();
    let mut empty = true;
    for name in names {
        if cancelled() {
            return Err("cancelled".into());
        }
        let st = match fstatat(dir.as_fd(), name.as_c_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(s) => s,
            Err(_) => continue, // gone meanwhile
        };
        let is_dir = SFlag::from_bits_truncate(st.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR;
        if is_dir {
            let mut sub = match open_dir_at(dir.as_fd(), &name) {
                Ok(d) => d,
                Err(_) => {
                    empty = false;
                    continue;
                }
            };
            if clean(&mut sub, cutoff_s, stats, cancelled)? {
                drop(sub);
                match unlinkat(dir.as_fd(), name.as_c_str(), UnlinkatFlags::RemoveDir) {
                    Ok(()) => stats.dirs += 1,
                    Err(_) => empty = false,
                }
            } else {
                empty = false;
            }
        } else if st.st_ctime < cutoff_s {
            match unlinkat(dir.as_fd(), name.as_c_str(), UnlinkatFlags::NoRemoveDir) {
                Ok(()) => {
                    stats.files += 1;
                    stats.bytes += st.st_size as u64;
                }
                Err(_) => empty = false,
            }
        } else {
            empty = false;
        }
    }
    Ok(empty)
}

/// Empties `<share_path>/#recycle` of items deleted more than `days` days ago. The bin
/// itself stays. `now_s`: current time in seconds (a parameter for the tests).
pub fn cleanup(
    share_path: &str,
    days: u32,
    now_s: i64,
    cancelled: &dyn Fn() -> bool,
) -> Result<CleanupStats, String> {
    let mut stats = CleanupStats::default();
    let root = CString::new(share_path).map_err(|_| "invalid path".to_string())?;
    let share = Dir::open(
        root.as_c_str(),
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| format!("{share_path}: {e}"))?;
    let bin = CString::new(RECYCLE_DIR).expect("no NUL");
    let mut dir = match open_dir_at(share.as_fd(), &bin) {
        Ok(d) => d,
        Err(nix::errno::Errno::ENOENT) => return Ok(stats),
        Err(e) => return Err(format!("{share_path}/{RECYCLE_DIR}: {e}")),
    };
    let cutoff = now_s - i64::from(days) * 86_400;
    clean(&mut dir, cutoff, &mut stats, cancelled)?;
    Ok(stats)
}

/// What a bin holds, read only: every item, and those a cleanup with `days` would remove
/// now (`days` 0: none). Same walk rules as [`cleanup`] (descriptors, links never followed).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BinUsage {
    pub total_bytes: u64,
    pub old_bytes: u64,
    pub old_items: u64,
}

fn tally(dir: &mut Dir, cutoff_s: Option<i64>, u: &mut BinUsage, stop: &dyn Fn() -> bool) -> Result<(), String> {
    let names: Vec<CString> = dir
        .iter()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_owned())
        .filter(|n| n.as_bytes() != b"." && n.as_bytes() != b"..")
        .collect();
    for name in names {
        if stop() {
            return Err("stopped".into());
        }
        let Ok(st) = fstatat(dir.as_fd(), name.as_c_str(), AtFlags::AT_SYMLINK_NOFOLLOW) else { continue };
        let bytes = st.st_blocks as u64 * 512;
        u.total_bytes += bytes;
        if SFlag::from_bits_truncate(st.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR {
            if let Ok(mut sub) = open_dir_at(dir.as_fd(), &name) {
                tally(&mut sub, cutoff_s, u, stop)?;
            }
        } else if cutoff_s.is_some_and(|c| st.st_ctime < c) {
            u.old_bytes += bytes;
            u.old_items += 1;
        }
    }
    Ok(())
}

/// [`BinUsage`] of `<share_path>/#recycle` (an empty answer without a bin).
pub fn usage(share_path: &str, days: u32, now_s: i64, stop: &dyn Fn() -> bool) -> Result<BinUsage, String> {
    let mut u = BinUsage::default();
    let root = CString::new(share_path).map_err(|_| "invalid path".to_string())?;
    let share = match Dir::open(root.as_c_str(), OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC, Mode::empty()) {
        Ok(d) => d,
        Err(nix::errno::Errno::ENOENT) => return Ok(u),
        Err(e) => return Err(format!("{share_path}: {e}")),
    };
    let bin = CString::new(RECYCLE_DIR).expect("no NUL");
    let mut dir = match open_dir_at(share.as_fd(), &bin) {
        Ok(d) => d,
        Err(nix::errno::Errno::ENOENT) => return Ok(u),
        Err(e) => return Err(format!("{share_path}/{RECYCLE_DIR}: {e}")),
    };
    let cutoff = (days > 0).then(|| now_s - i64::from(days) * 86_400);
    tally(&mut dir, cutoff, &mut u, stop)?;
    Ok(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_old_items_and_never_follows_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let share = d.path().join("share");
        let bin = share.join(RECYCLE_DIR);
        std::fs::create_dir_all(bin.join("docs/sub")).unwrap();
        std::fs::write(bin.join("docs/sub/a.txt"), "aaaa").unwrap();
        std::fs::write(bin.join("b.txt"), "bb").unwrap();
        // a symlink to something outside the bin
        let outside = d.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("precious"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, bin.join("link")).unwrap();
        let now = (rynas_tasks::now_ms() / 1000) as i64;

        // everything is new: nothing goes
        let s = cleanup(share.to_str().unwrap(), 30, now, &|| false).unwrap();
        assert_eq!(s, CleanupStats::default());
        assert!(bin.join("b.txt").exists());

        // "31 days later"
        let s = cleanup(share.to_str().unwrap(), 30, now + 31 * 86_400, &|| false).unwrap();
        // a.txt, b.txt and the link; docs/sub and docs
        assert_eq!((s.files, s.dirs), (3, 2));
        assert!(s.bytes >= 6);
        assert!(bin.exists(), "the bin itself stays");
        assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 0);
        assert!(
            outside.join("precious").exists(),
            "symlink target untouched"
        );

        // no bin: nothing to do
        std::fs::remove_dir(&bin).unwrap();
        assert_eq!(
            cleanup(share.to_str().unwrap(), 30, now, &|| false).unwrap(),
            CleanupStats::default()
        );
    }

    #[test]
    fn usage_reads_without_removing() {
        let d = tempfile::tempdir().unwrap();
        let share = d.path().join("share");
        let bin = share.join(RECYCLE_DIR);
        std::fs::create_dir_all(bin.join("docs")).unwrap();
        std::fs::write(bin.join("docs/a.txt"), vec![0u8; 8192]).unwrap();
        std::fs::write(bin.join("b.txt"), "bb").unwrap();
        let now = (rynas_tasks::now_ms() / 1000) as i64;
        let p = share.to_str().unwrap();
        let fresh = usage(p, 30, now, &|| false).unwrap();
        assert!(fresh.total_bytes > 0);
        assert_eq!((fresh.old_bytes, fresh.old_items), (0, 0));
        let later = usage(p, 30, now + 31 * 86_400, &|| false).unwrap();
        assert_eq!(later.old_items, 2);
        assert!(later.old_bytes > 0 && later.old_bytes <= later.total_bytes);
        // retention 0 keeps everything
        assert_eq!(usage(p, 0, now + 365 * 86_400, &|| false).unwrap().old_items, 0);
        assert!(bin.join("docs/a.txt").exists(), "nothing removed");
        // no bin, no share
        assert_eq!(usage(d.path().join("none").to_str().unwrap(), 30, now, &|| false).unwrap(), BinUsage::default());
    }
}
