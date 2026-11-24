//! What removing a set of snapshots gives back: a walk over the data extents of the set and
//! of what keeps sharing them (Space, `docs/phase2/space.md`).
//!
//! `btrfs filesystem du -s` answers per argument: its "Exclusive" counts the extents only one
//! file references and its "Set shared" the union of the shared extents inside that one
//! argument. Neither says how much of what a *group* of snapshots shares is also referenced
//! by something outside the group, which is exactly what deleting the group cannot free: a
//! file deleted from the share but present in 30 hourly snapshots is "shared" in every one of
//! them, so summing their "Exclusive" says 0 while removing all 30 frees the whole file.
//!
//! The walk here does what `du` does (`FS_IOC_FIEMAP` on every regular file) but keeps the
//! physical ranges across arguments:
//!
//! - extents without `FIEMAP_EXTENT_SHARED` are referenced once: removing the set frees them
//!   for sure ([`SetMeasure::exclusive`]);
//! - the shared extents of the set are merged into one range set `A`; the shared extents of
//!   the kept members (the other snapshots of the same source and the live subvolume) are
//!   clipped to `A` and merged into `K`; what the set frees is `exclusive + |A| - |A ∩ K|`.
//!
//! Over-estimates (documented in the design): data the set also shares with subvolumes that
//! are not in the kept list (deduplicated or reflinked into another share), and extents of
//! which the set and the kept members reference different, non-overlapping parts (btrfs frees
//! an extent only when no part of it is referenced). Lengths are logical: compressed extents
//! count their uncompressed size, as in `du`.
//!
//! Nested subvolumes are not entered (another `st_dev`), symbolic links are never followed,
//! files that cannot be opened are counted in [`Walk::unreadable`].

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// `FS_IOC_FIEMAP` = `_IOWR('f', 11, struct fiemap)` (32-byte header).
const FS_IOC_FIEMAP: u64 = (3 << 30) | (32 << 16) | ((b'f' as u64) << 8) | 11;
const FIEMAP_EXTENT_LAST: u32 = 0x1;
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x2;
const FIEMAP_EXTENT_DELALLOC: u32 = 0x4;
const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x200;
const FIEMAP_EXTENT_SHARED: u32 = 0x2000;
/// Extents asked per call.
const BATCH: usize = 256;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FiemapExtent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct Fiemap {
    start: u64,
    length: u64,
    flags: u32,
    mapped: u32,
    count: u32,
    reserved: u32,
    extents: [FiemapExtent; BATCH],
}

/// One extent of a file as the walk sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub physical: u64,
    pub length: u64,
    pub shared: bool,
}

/// The data extents of an open file (`FIEMAP`); delayed, inline and unknown extents are
/// left out (no stable physical address).
pub fn file_extents(f: &std::fs::File, out: &mut Vec<Extent>) -> io::Result<()> {
    let mut m: Box<Fiemap> = Box::new(Fiemap {
        start: 0,
        length: u64::MAX,
        flags: 0,
        mapped: 0,
        count: BATCH as u32,
        reserved: 0,
        extents: [FiemapExtent::default(); BATCH],
    });
    let mut start = 0u64;
    loop {
        m.start = start;
        m.length = u64::MAX - start;
        m.flags = 0;
        m.mapped = 0;
        m.count = BATCH as u32;
        // SAFETY: the kernel writes at most `count` extents into the fixed array
        if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FIEMAP as _, &mut *m) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = (m.mapped as usize).min(BATCH);
        if n == 0 {
            return Ok(());
        }
        let mut last = false;
        for e in &m.extents[..n] {
            last |= e.flags & FIEMAP_EXTENT_LAST != 0;
            start = e.logical.saturating_add(e.length);
            if e.flags & (FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_DELALLOC | FIEMAP_EXTENT_DATA_INLINE) != 0 || e.length == 0 {
                continue;
            }
            out.push(Extent { physical: e.physical, length: e.length, shared: e.flags & FIEMAP_EXTENT_SHARED != 0 });
        }
        if last {
            return Ok(());
        }
    }
}

/// A set of physical byte ranges `[start, end)`, merged on demand.
#[derive(Debug, Clone, Default)]
pub struct Ranges {
    v: Vec<(u64, u64)>,
    /// Length of `v` after the last merge.
    merged_len: usize,
}

impl Ranges {
    pub fn push(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        self.v.push((start, start.saturating_add(len)));
        if self.v.len() > 2 * self.merged_len + 4096 {
            self.merge();
        }
    }

    /// Bytes the ranges take in memory now.
    pub fn memory(&self) -> usize {
        self.v.capacity() * std::mem::size_of::<(u64, u64)>()
    }

    /// Sorts and merges overlapping or touching ranges.
    pub fn merge(&mut self) {
        self.v.sort_unstable();
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(self.v.len());
        for &(s, e) in &self.v {
            match out.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => out.push((s, e)),
            }
        }
        self.v = out;
        self.merged_len = self.v.len();
    }

    /// Ranges held (after [`Ranges::merge`]: distinct ranges).
    pub fn len(&self) -> usize {
        self.v.len()
    }

    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }

    /// Total bytes (merges first).
    pub fn bytes(&mut self) -> u64 {
        self.merge();
        self.v.iter().map(|(s, e)| e - s).sum()
    }

    /// The parts of `[start, start + len)` inside this set (which must be merged), each as
    /// `(start, len)`.
    pub fn clip(&self, start: u64, len: u64, out: &mut Vec<(u64, u64)>) {
        let end = start.saturating_add(len);
        // first range that ends after `start`
        let mut i = self.v.partition_point(|&(_, e)| e <= start);
        while let Some(&(s, e)) = self.v.get(i) {
            if s >= end {
                break;
            }
            let (a, b) = (s.max(start), e.min(end));
            if b > a {
                out.push((a, b - a));
            }
            i += 1;
        }
    }

    /// Bytes both sets hold (both merged).
    pub fn overlap(&self, other: &Ranges) -> u64 {
        let (mut i, mut j, mut sum) = (0, 0, 0u64);
        while i < self.v.len() && j < other.v.len() {
            let (a, b) = (self.v[i], other.v[j]);
            let (s, e) = (a.0.max(b.0), a.1.min(b.1));
            if e > s {
                sum += e - s;
            }
            if a.1 <= b.1 {
                i += 1;
            } else {
                j += 1;
            }
        }
        sum
    }
}

/// Why a walk stopped early.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkError {
    /// The caller asked to stop (cancel, time budget).
    Stopped,
    /// More shared ranges than the memory budget allows.
    TooLarge,
    Io(String),
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkError::Stopped => f.write_str("stopped"),
            WalkError::TooLarge => f.write_str("too many shared extents to measure"),
            WalkError::Io(e) => f.write_str(e),
        }
    }
}

/// Counters of one walk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Walk {
    pub files: u64,
    pub unreadable: u64,
}

/// Limits of a measurement.
pub struct Limits<'a> {
    /// Most ranges kept for the set and, separately, for the kept members (16 bytes each;
    /// a vector may hold twice that while it grows, so the peak is about 4 × 16 × this).
    pub max_ranges: usize,
    /// Asked every few hundred files: `true` stops the walk.
    pub stop: &'a dyn Fn() -> bool,
}

/// Calls `each` with the extents of every regular file below `root` (same filesystem
/// device: nested subvolumes are left out; symbolic links never followed).
pub fn walk(root: &Path, stop: &dyn Fn() -> bool, each: &mut dyn FnMut(&[Extent]) -> Result<(), WalkError>) -> Result<Walk, WalkError> {
    let meta = std::fs::symlink_metadata(root).map_err(|e| WalkError::Io(format!("{}: {e}", root.display())))?;
    let dev = meta.dev();
    let mut w = Walk::default();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut buf = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            w.unreadable += 1;
            continue;
        };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else {
                w.unreadable += 1;
                continue;
            };
            if m.dev() != dev {
                continue;
            }
            let ft = m.file_type();
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                w.files += 1;
                if w.files % 256 == 0 && stop() {
                    return Err(WalkError::Stopped);
                }
                let f = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                    .open(e.path());
                let Ok(f) = f else {
                    w.unreadable += 1;
                    continue;
                };
                buf.clear();
                if file_extents(&f, &mut buf).is_err() {
                    w.unreadable += 1;
                    continue;
                }
                each(&buf)?;
            }
        }
    }
    Ok(w)
}

/// What removing a set gives back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetMeasure {
    /// Bytes given back (estimate, see the module text).
    pub freed: u64,
    /// Bytes only one file of the set references: given back for sure.
    pub exclusive: u64,
    pub files: u64,
    pub unreadable: u64,
}

/// Measures what removing `set` frees, given the members that stay (`keep`: the other
/// snapshots of the same source and the live subvolume).
pub fn measure_set(set: &[PathBuf], keep: &[PathBuf], lim: &Limits) -> Result<SetMeasure, WalkError> {
    let mut shared = Ranges::default();
    let mut m = SetMeasure::default();
    for p in set {
        let w = walk(p, lim.stop, &mut |ex| {
            for e in ex {
                if e.shared {
                    shared.push(e.physical, e.length);
                } else {
                    m.exclusive += e.length;
                }
            }
            if shared.len() > lim.max_ranges {
                shared.merge();
                if shared.len() > lim.max_ranges {
                    return Err(WalkError::TooLarge);
                }
            }
            Ok(())
        })?;
        m.files += w.files;
        m.unreadable += w.unreadable;
    }
    let shared_bytes = shared.bytes();
    let mut kept = Ranges::default();
    if shared_bytes > 0 {
        let mut clipped = Vec::new();
        for p in keep {
            let w = walk(p, lim.stop, &mut |ex| {
                for e in ex.iter().filter(|e| e.shared) {
                    clipped.clear();
                    shared.clip(e.physical, e.length, &mut clipped);
                    for &(s, l) in &clipped {
                        kept.push(s, l);
                    }
                }
                if kept.len() > lim.max_ranges {
                    kept.merge();
                    if kept.len() > lim.max_ranges {
                        return Err(WalkError::TooLarge);
                    }
                }
                Ok(())
            })?;
            m.unreadable += w.unreadable;
        }
        kept.merge();
    }
    m.freed = m.exclusive + shared_bytes - shared.overlap(&kept);
    Ok(m)
}

/// `btrfs filesystem du -s --raw` style sizes of a plain directory tree (no extent sharing
/// asked): allocated bytes (`st_blocks * 512`) and entries, without following links or
/// leaving the filesystem. `stop` is asked every few hundred entries.
pub fn tree_bytes(root: &Path, stop: &dyn Fn() -> bool) -> io::Result<(u64, u64)> {
    let meta = std::fs::symlink_metadata(root)?;
    let dev = meta.dev();
    let (mut bytes, mut n) = (0u64, 0u64);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.dev() != dev {
                continue;
            }
            n += 1;
            if n % 512 == 0 && stop() {
                return Err(io::Error::other("stopped"));
            }
            bytes += m.blocks() * 512;
            if m.file_type().is_dir() {
                stack.push(e.path());
            }
        }
    }
    Ok((bytes, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(v: &[(u64, u64)]) -> Ranges {
        let mut x = Ranges::default();
        for &(s, l) in v {
            x.push(s, l);
        }
        x.merge();
        x
    }

    #[test]
    fn ranges_merge_and_count_once() {
        let mut a = r(&[(0, 10), (5, 10), (20, 5), (15, 5)]);
        // 0..15 and 15..20 touch, 20..25: one range 0..25
        assert_eq!(a.len(), 1);
        assert_eq!(a.bytes(), 25);
        let mut b = r(&[(100, 10), (0, 1)]);
        assert_eq!(b.bytes(), 11);
        assert!(b.memory() >= 16);
    }

    #[test]
    fn clip_and_overlap() {
        let a = r(&[(0, 10), (20, 10)]);
        let mut out = Vec::new();
        a.clip(5, 20, &mut out);
        assert_eq!(out, vec![(5, 5), (20, 5)]);
        out.clear();
        a.clip(10, 10, &mut out);
        assert!(out.is_empty());
        let b = r(&[(8, 4), (25, 100)]);
        assert_eq!(a.overlap(&b), 2 + 5);
    }

    #[test]
    fn walk_counts_files_and_stays_on_the_device() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("a/b/f"), vec![1u8; 10_000]).unwrap();
        std::fs::write(d.path().join("g"), b"x").unwrap();
        std::os::unix::fs::symlink("/etc", d.path().join("link")).unwrap();
        let mut seen = 0;
        let w = walk(d.path(), &|| false, &mut |_| {
            seen += 1;
            Ok(())
        })
        .unwrap();
        // FIEMAP may be unsupported by the test filesystem (tmpfs): then both are unreadable
        assert_eq!(w.files, 2);
        assert_eq!(seen + w.unreadable, 2);
        let (bytes, n) = tree_bytes(d.path(), &|| false).unwrap();
        assert_eq!(n, 5);
        assert!(bytes > 0 || n > 0);
    }

    #[test]
    fn a_stop_ends_the_walk() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..600 {
            std::fs::write(d.path().join(format!("f{i}")), b"x").unwrap();
        }
        let r = walk(d.path(), &|| true, &mut |_| Ok(()));
        assert_eq!(r.unwrap_err(), WalkError::Stopped);
    }
}
