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
//! [`measure_tree`] (the normal way) reads the subvolume trees instead of the files: a
//! `BTRFS_IOC_TREE_SEARCH_V2` over each subvolume's tree (root privileges) gives every
//! `EXTENT_DATA` item, i.e. every file range and the data extent it points to
//! (`disk_bytenr`, `disk_num_bytes`). Inline data (stored in metadata) and holes are left out.
//! The unit is the whole extent, deduplicated by its start: btrfs frees an extent only when no
//! file range anywhere references any part of it, so
//!
//! - freed = bytes of the distinct extents the set references and no kept member references.
//!
//! No file is opened and no back reference is walked (what makes `du` and `FIEMAP`'s shared
//! flag slow), so a subvolume with millions of files costs one pass over its metadata. Memory:
//! 8 bytes per distinct extent of the set (capped by [`Limits::max_extents`], past which the
//! answer is [`WalkError::TooLarge`]) and one bit per extent for the kept members' marks.
//!
//! Over-estimates (documented in the design): data the set also shares with subvolumes that
//! are not in the kept list (deduplicated or reflinked into another share, or, when the
//! caller keeps only the neighbours of the set in a line of snapshots, reflinked into a
//! snapshot further away). Never counted, so the number can only be low by them: the
//! metadata the set alone holds (tree blocks, inline data). Sizes are on-disk sizes
//! (compressed extents count what they take).
//!
//! [`measure_set`] is the older walk, kept as the fallback when the tree search cannot be used
//! (not root, a member that is not a subvolume): what `du` does (`FS_IOC_FIEMAP` on every
//! regular file) but keeping the physical ranges across arguments:
//!
//! - extents without `FIEMAP_EXTENT_SHARED` are referenced once: removing the set frees them
//!   for sure ([`SetMeasure::exclusive`]);
//! - the shared extents of the set are merged into one range set `A`; the shared extents of
//!   the kept members are clipped to `A` and merged into `K`; what the set frees is
//!   `exclusive + |A| - |A ∩ K|`.
//!
//! Its over-estimates: data shared outside the kept list, as above, and extents of which the
//! set and the kept members reference different, non-overlapping parts (btrfs frees an extent
//! only when no part of it is referenced). Its lengths are logical: compressed extents count
//! their uncompressed size, as in `du`. Nested subvolumes are not entered (another `st_dev`),
//! symbolic links are never followed, files that cannot be opened are counted in
//! [`Walk::unreadable`].

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
    /// More shared ranges (or distinct extents) than the memory budget allows.
    TooLarge,
    /// The tree search cannot be used here (not root, not a subvolume, old kernel): the
    /// caller may walk the files instead.
    Unsupported(String),
    Io(String),
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkError::Stopped => f.write_str("stopped"),
            WalkError::TooLarge => f.write_str("too many shared extents to measure"),
            WalkError::Unsupported(e) => write!(f, "tree search not usable: {e}"),
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
    /// File walk: most ranges kept for the set and, separately, for the kept members (16
    /// bytes each; a vector may hold twice that while it grows, so the peak is about
    /// 4 × 16 × this).
    pub max_ranges: usize,
    /// Tree search: most distinct extents of the set (8 bytes each; with the batch not yet
    /// deduplicated the vector holds at most about twice that, plus one bit each for the
    /// kept members' marks).
    pub max_extents: usize,
    /// Asked every few hundred files or every search batch: `true` stops the walk.
    pub stop: &'a dyn Fn() -> bool,
    /// One line per subvolume searched (task log).
    pub progress: &'a dyn Fn(&str),
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
    /// Bytes only one file of the set references: given back for sure (file walk only; the
    /// tree search does not know reference counts).
    pub exclusive: Option<u64>,
    /// Distinct data extents of the set (tree search only).
    pub extents: u64,
    pub files: u64,
    pub unreadable: u64,
}

/// Measures what removing `set` frees, given the members that stay (`keep`: the other
/// snapshots of the same source and the live subvolume).
pub fn measure_set(set: &[PathBuf], keep: &[PathBuf], lim: &Limits) -> Result<SetMeasure, WalkError> {
    let mut shared = Ranges::default();
    let mut m = SetMeasure::default();
    let mut exclusive = 0u64;
    for p in set {
        let w = walk(p, lim.stop, &mut |ex| {
            for e in ex {
                if e.shared {
                    shared.push(e.physical, e.length);
                } else {
                    exclusive += e.length;
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
    m.freed = exclusive + shared_bytes - shared.overlap(&kept);
    m.exclusive = Some(exclusive);
    Ok(m)
}

/// `BTRFS_IOC_TREE_SEARCH_V2` = `_IOWR(0x94, 17, struct btrfs_ioctl_search_args_v2)`
/// (104-byte search key + 8-byte buffer size).
const IOC_TREE_SEARCH_V2: u64 = (3 << 30) | (112 << 16) | (0x94 << 8) | 17;
/// `BTRFS_EXTENT_DATA_KEY`: the item that maps a file range to a data extent.
const EXTENT_DATA_KEY: u32 = 108;
/// `BTRFS_FIRST_FREE_OBJECTID`: the first inode number (the subvolume's top directory).
const FIRST_FREE_OBJECTID: u64 = 256;
/// `BTRFS_FILE_EXTENT_INLINE`: data stored in the item itself (in metadata).
const FILE_EXTENT_INLINE: u8 = 0;
/// Result buffer of one search (the kernel caps it at 16 MiB; one item is at most a leaf).
const SEARCH_BUF: usize = 1 << 20;
/// `struct btrfs_ioctl_search_header`.
const SEARCH_HEADER: usize = 32;
/// Size of a `btrfs_file_extent_item` that points to an extent (packed, little endian).
const FILE_EXTENT_ITEM: usize = 53;

/// `struct btrfs_ioctl_search_key`.
#[repr(C)]
#[derive(Clone, Copy)]
struct SearchKey {
    tree_id: u64,
    min_objectid: u64,
    max_objectid: u64,
    min_offset: u64,
    max_offset: u64,
    min_transid: u64,
    max_transid: u64,
    min_type: u32,
    max_type: u32,
    nr_items: u32,
    unused: u32,
    unused1: [u64; 4],
}

const KEY_WORDS: usize = std::mem::size_of::<SearchKey>() / 8;

/// What a `btrfs_file_extent_item` holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileExtent {
    /// Data inside the item (metadata, no data extent).
    Inline,
    /// A hole (`disk_bytenr == 0`).
    Hole,
    /// A regular or preallocated extent: its start and on-disk size (the whole extent,
    /// whatever part of it this file range uses), and the length of the file range.
    Data { bytenr: u64, len: u64, num_bytes: u64 },
}

/// One file range that points to a data extent, as a tree search finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRef {
    pub ino: u64,
    /// Start of the file range (the item's key offset).
    pub offset: u64,
    pub bytenr: u64,
    /// On-disk size of the whole extent.
    pub len: u64,
    /// Length of the file range.
    pub num_bytes: u64,
}

/// A btrfs key `(objectid, type, offset)`.
pub type Key = (u64, u32, u64);

/// Reads a `btrfs_file_extent_item`; `None`: too short or an unknown type.
pub fn parse_file_extent(item: &[u8]) -> Option<FileExtent> {
    let kind = *item.get(20)?;
    if kind == FILE_EXTENT_INLINE {
        return Some(FileExtent::Inline);
    }
    if kind > 2 || item.len() < FILE_EXTENT_ITEM {
        return None;
    }
    let le = |a: usize| u64::from_le_bytes(item[a..a + 8].try_into().unwrap_or([0; 8]));
    let (bytenr, len, num_bytes) = (le(21), le(29), le(45));
    Some(if bytenr == 0 { FileExtent::Hole } else { FileExtent::Data { bytenr, len, num_bytes } })
}

/// A data extent as one sortable number: `bytenr / 4096` in the high 48 bits,
/// `len / 4096` in the low 16 (data extents are at most 128 MiB). `None`: a layout this
/// does not expect (then nothing is measured rather than something wrong).
fn pack(bytenr: u64, len: u64) -> Option<u64> {
    if !bytenr.is_multiple_of(4096) || !len.is_multiple_of(4096) || len == 0 {
        return None;
    }
    let (b, l) = (bytenr >> 12, len >> 12);
    (b < 1 << 48 && l < 1 << 16).then_some((b << 16) | l)
}

fn packed_len(k: u64) -> u64 {
    (k & 0xffff) << 12
}

/// The key after `(objectid, type, offset)` in btrfs key order.
fn next_key((o, t, off): (u64, u32, u64)) -> Option<(u64, u32, u64)> {
    if off < u64::MAX {
        Some((o, t, off + 1))
    } else if t < 255 {
        Some((o, t + 1, 0))
    } else if o < u64::MAX {
        Some((o + 1, 0, 0))
    } else {
        None
    }
}

/// Walks the items of one search result: calls `each` for the data extent items, returns
/// the last key seen.
fn search_items(
    buf: &[u8],
    nr: usize,
    each: &mut dyn FnMut(DataRef) -> Result<(), WalkError>,
) -> Result<Option<Key>, WalkError> {
    let bad = || WalkError::Io("malformed tree search result".into());
    let (mut pos, mut last) = (0usize, None);
    for _ in 0..nr {
        let h = buf.get(pos..pos + SEARCH_HEADER).ok_or_else(bad)?;
        let u64_at = |a: usize| u64::from_ne_bytes(h[a..a + 8].try_into().unwrap_or([0; 8]));
        let u32_at = |a: usize| u32::from_ne_bytes(h[a..a + 4].try_into().unwrap_or([0; 4]));
        let (objectid, offset, kind, len) = (u64_at(8), u64_at(16), u32_at(24), u32_at(28) as usize);
        let item = buf.get(pos + SEARCH_HEADER..pos + SEARCH_HEADER + len).ok_or_else(bad)?;
        pos += SEARCH_HEADER + len;
        last = Some((objectid, kind, offset));
        if kind == EXTENT_DATA_KEY {
            match parse_file_extent(item) {
                Some(FileExtent::Data { bytenr, len, num_bytes }) => each(DataRef { ino: objectid, offset, bytenr, len, num_bytes })?,
                Some(_) => {}
                None => return Err(WalkError::Io(format!("unknown file extent item (inode {objectid})"))),
            }
        }
    }
    Ok(last)
}

/// Calls `each(disk_bytenr, disk_num_bytes)` for every data extent reference of the files
/// of subvolume `tree_id`, read from its tree with `BTRFS_IOC_TREE_SEARCH_V2` (metadata only,
/// no file opened; needs `CAP_SYS_ADMIN`). `fd`: any descriptor on the filesystem. Returns
/// the references seen. A first search that fails is [`WalkError::Unsupported`].
pub fn subvolume_extents(
    fd: &std::fs::File,
    tree_id: u64,
    stop: &dyn Fn() -> bool,
    each: &mut dyn FnMut(u64, u64) -> Result<(), WalkError>,
) -> Result<u64, WalkError> {
    search_extent_items(fd, tree_id, (FIRST_FREE_OBJECTID, 0, 0), (u64::MAX, 255, u64::MAX), 0, stop, &mut |r| each(r.bytenr, r.len))
}

/// Calls `each` for the data extent items of subvolume `tree_id` with keys from `min` to
/// `max` (btrfs key order), in tree blocks written at transaction `min_transid` or later:
/// the kernel skips every subtree older than that (what `btrfs subvolume find-new` uses).
/// Returns the items seen. A first search that fails is [`WalkError::Unsupported`].
pub fn search_extent_items(
    fd: &std::fs::File,
    tree_id: u64,
    min: Key,
    max: Key,
    min_transid: u64,
    stop: &dyn Fn() -> bool,
    each: &mut dyn FnMut(DataRef) -> Result<(), WalkError>,
) -> Result<u64, WalkError> {
    let mut words = vec![0u64; KEY_WORDS + 1 + SEARCH_BUF / 8];
    let mut min = min;
    let (mut n, mut first) = (0u64, true);
    loop {
        if stop() {
            return Err(WalkError::Stopped);
        }
        let key = SearchKey {
            tree_id,
            min_objectid: min.0,
            max_objectid: max.0,
            min_offset: min.2,
            max_offset: max.2,
            min_transid,
            max_transid: u64::MAX,
            min_type: min.1,
            max_type: max.1,
            nr_items: u32::MAX,
            unused: 0,
            unused1: [0; 4],
        };
        // SAFETY: the buffer is 8-aligned and longer than the key
        unsafe { std::ptr::write(words.as_mut_ptr() as *mut SearchKey, key) };
        words[KEY_WORDS] = SEARCH_BUF as u64;
        // SAFETY: the kernel writes the key back and at most `buf_size` bytes after it
        if unsafe { libc::ioctl(fd.as_raw_fd(), IOC_TREE_SEARCH_V2 as _, words.as_mut_ptr()) } < 0 {
            let e = io::Error::last_os_error().to_string();
            return Err(if first { WalkError::Unsupported(e) } else { WalkError::Io(e) });
        }
        first = false;
        // SAFETY: the key the kernel wrote back
        let nr = unsafe { std::ptr::read(words.as_ptr() as *const SearchKey) }.nr_items as usize;
        if nr == 0 {
            return Ok(n);
        }
        // SAFETY: the words after the key and the size, viewed as bytes
        let bytes = unsafe { std::slice::from_raw_parts(words.as_ptr().add(KEY_WORDS + 1) as *const u8, SEARCH_BUF) };
        let last = search_items(bytes, nr, &mut |r| {
            n += 1;
            each(r)
        })?;
        match last.filter(|&k| k < max).and_then(next_key) {
            Some(k) => min = k,
            None => return Ok(n),
        }
    }
}

/// `BTRFS_ROOT_TREE_OBJECTID`: the tree of the trees.
const ROOT_TREE_OBJECTID: u64 = 1;
/// `BTRFS_ROOT_ITEM_KEY`.
const ROOT_ITEM_KEY: u32 = 132;
/// Offset of `bytes_used` in a `btrfs_root_item`: after the 160-byte inode item,
/// `generation`, `root_dirid`, `bytenr` and `byte_limit`.
const ROOT_ITEM_BYTES_USED: usize = 192;

/// `bytes_used` of a `btrfs_root_item` (little endian); `None` when the item is too short.
fn root_item_bytes_used(item: &[u8]) -> Option<u64> {
    item.get(ROOT_ITEM_BYTES_USED..ROOT_ITEM_BYTES_USED + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])))
}

/// The size of subvolume `tree_id`'s own tree: `bytes_used` of its root item, the tree
/// blocks the kernel has allocated to it (metadata only). A full tree search
/// ([`subvolume_extents`]) reads all of them, so its cost follows this number. One small
/// search of the root tree (`CAP_SYS_ADMIN`); `fd`: any descriptor on the filesystem.
/// `Ok(None)`: no root item for that id.
pub fn subvolume_tree_bytes(fd: &std::fs::File, tree_id: u64) -> Result<Option<u64>, WalkError> {
    const BUF: usize = 4096;
    let mut words = vec![0u64; KEY_WORDS + 1 + BUF / 8];
    let key = SearchKey {
        tree_id: ROOT_TREE_OBJECTID,
        min_objectid: tree_id,
        max_objectid: tree_id,
        min_offset: 0,
        max_offset: u64::MAX,
        min_transid: 0,
        max_transid: u64::MAX,
        min_type: ROOT_ITEM_KEY,
        max_type: ROOT_ITEM_KEY,
        nr_items: 1,
        unused: 0,
        unused1: [0; 4],
    };
    // SAFETY: the buffer is 8-aligned and longer than the key
    unsafe { std::ptr::write(words.as_mut_ptr() as *mut SearchKey, key) };
    words[KEY_WORDS] = BUF as u64;
    // SAFETY: the kernel writes the key back and at most `buf_size` bytes after it
    if unsafe { libc::ioctl(fd.as_raw_fd(), IOC_TREE_SEARCH_V2 as _, words.as_mut_ptr()) } < 0 {
        return Err(WalkError::Unsupported(io::Error::last_os_error().to_string()));
    }
    // SAFETY: the key the kernel wrote back
    let nr = unsafe { std::ptr::read(words.as_ptr() as *const SearchKey) }.nr_items as usize;
    if nr == 0 {
        return Ok(None);
    }
    // SAFETY: the words after the key and the size, viewed as bytes
    let buf = unsafe { std::slice::from_raw_parts(words.as_ptr().add(KEY_WORDS + 1) as *const u8, BUF) };
    let bad = || WalkError::Io("malformed tree search result".into());
    let h = buf.get(..SEARCH_HEADER).ok_or_else(bad)?;
    let u64_at = |a: usize| u64::from_ne_bytes(h[a..a + 8].try_into().unwrap_or([0; 8]));
    let u32_at = |a: usize| u32::from_ne_bytes(h[a..a + 4].try_into().unwrap_or([0; 4]));
    let (objectid, kind, len) = (u64_at(8), u32_at(24), u32_at(28) as usize);
    if objectid != tree_id || kind != ROOT_ITEM_KEY {
        return Ok(None);
    }
    let item = buf.get(SEARCH_HEADER..SEARCH_HEADER + len).ok_or_else(bad)?;
    Ok(root_item_bytes_used(item))
}

/// Distinct extents (packed), deduplicated in batches with a bounded vector.
#[derive(Default)]
struct ExtentSet {
    v: Vec<u64>,
    merged: usize,
}

impl ExtentSet {
    fn push(&mut self, k: u64, max: usize) -> Result<(), WalkError> {
        if self.v.len() == self.v.capacity() {
            self.v.reserve_exact(self.merged.max(1 << 16));
        }
        self.v.push(k);
        if self.v.len() >= 2 * self.merged + (1 << 16) {
            self.merge();
            if self.v.len() > max {
                return Err(WalkError::TooLarge);
            }
        }
        Ok(())
    }

    fn merge(&mut self) {
        self.v.sort_unstable();
        self.v.dedup();
        self.merged = self.v.len();
    }

    /// The last merge: sorted, distinct, within `max`.
    fn finish(&mut self, max: usize) -> Result<(), WalkError> {
        self.merge();
        if self.v.len() > max {
            return Err(WalkError::TooLarge);
        }
        self.v.shrink_to_fit();
        Ok(())
    }
}

/// The subvolumes to search: `(path, open top directory, tree id)`. Anything that is not a
/// btrfs subvolume top directory makes the whole tree search unusable.
fn open_trees(paths: &[PathBuf]) -> Result<Vec<(&PathBuf, std::fs::File, u64)>, WalkError> {
    paths
        .iter()
        .map(|p| {
            let unsupported = |e: String| WalkError::Unsupported(format!("{}: {e}", p.display()));
            let f = crate::ioctl::open_dir(p).map_err(|e| unsupported(e.to_string()))?;
            if !crate::ioctl::is_subvolume(&f) {
                return Err(unsupported("not a subvolume".into()));
            }
            let id = crate::ioctl::subvol_info(&f).map_err(|e| unsupported(e.to_string()))?.root_id;
            Ok((p, f, id))
        })
        .collect()
}

/// The core of [`measure_tree`] on extents already read: what `set_refs` frees while
/// `keep_refs` stay (each a list of `(disk_bytenr, disk_num_bytes)` per member).
pub fn freed_by_extents(set_refs: &[Vec<(u64, u64)>], keep_refs: &[Vec<(u64, u64)>], max_extents: usize) -> Result<(u64, u64), WalkError> {
    let mut set = ExtentSet::default();
    for r in set_refs {
        for &(b, l) in r {
            set.push(pack(b, l).ok_or_else(|| WalkError::Io(format!("unexpected extent {b}+{l}")))?, max_extents)?;
        }
    }
    set.finish(max_extents)?;
    let mut kept = vec![0u64; set.v.len().div_ceil(64)];
    for r in keep_refs {
        for &(b, l) in r {
            mark(&set.v, &mut kept, b, l);
        }
    }
    Ok((unmarked_bytes(&set.v, &kept), set.v.len() as u64))
}

fn mark(keys: &[u64], kept: &mut [u64], b: u64, l: u64) {
    // an extent that does not pack cannot be one of the set's
    if let Some(i) = pack(b, l).and_then(|k| keys.binary_search(&k).ok()) {
        kept[i / 64] |= 1 << (i % 64);
    }
}

fn unmarked_bytes(keys: &[u64], kept: &[u64]) -> u64 {
    keys.iter().enumerate().filter(|(i, _)| kept[i / 64] & (1 << (i % 64)) == 0).map(|(_, &k)| packed_len(k)).sum()
}

/// What removing `set` frees by whole data extents, read from the subvolume trees (see the
/// module text): the bytes of the distinct extents the set's files point to that no member
/// of `keep` points to. All paths must be subvolume top directories.
pub fn measure_tree(set: &[PathBuf], keep: &[PathBuf], lim: &Limits) -> Result<SetMeasure, WalkError> {
    let set_trees = open_trees(set)?;
    let keep_trees = open_trees(keep)?;
    let mut ext = ExtentSet::default();
    for (p, f, id) in &set_trees {
        let n = subvolume_extents(f, *id, lim.stop, &mut |b, l| match pack(b, l) {
            Some(k) => ext.push(k, lim.max_extents),
            None => Err(WalkError::Io(format!("unexpected extent {b}+{l}"))),
        })?;
        (lim.progress)(&format!("{}: {n} extent references", p.display()));
    }
    ext.finish(lim.max_extents)?;
    let mut m = SetMeasure { extents: ext.v.len() as u64, ..SetMeasure::default() };
    let mut kept = vec![0u64; ext.v.len().div_ceil(64)];
    if !ext.v.is_empty() {
        for (p, f, id) in &keep_trees {
            let n = subvolume_extents(f, *id, lim.stop, &mut |b, l| {
                mark(&ext.v, &mut kept, b, l);
                Ok(())
            })?;
            (lim.progress)(&format!("{} (kept): {n} extent references", p.display()));
        }
    }
    m.freed = unmarked_bytes(&ext.v, &kept);
    Ok(m)
}

/// The distinct data extents subvolume `tree_id` references (one full tree search), sorted,
/// each as `(disk_bytenr, disk_num_bytes)` packed into one number ([`unpack`] reads it back).
pub fn distinct_extents(fd: &std::fs::File, tree_id: u64, lim: &Limits) -> Result<Vec<u64>, WalkError> {
    let mut ext = ExtentSet::default();
    subvolume_extents(fd, tree_id, lim.stop, &mut |b, l| match pack(b, l) {
        Some(k) => ext.push(k, lim.max_extents),
        None => Err(WalkError::Io(format!("unexpected extent {b}+{l}"))),
    })?;
    ext.finish(lim.max_extents)?;
    Ok(ext.v)
}

/// `(disk_bytenr, disk_num_bytes)` of a packed extent of [`distinct_extents`].
pub fn unpack(k: u64) -> (u64, u64) {
    ((k >> 16) << 12, packed_len(k))
}

/// What removing `set` frees: the tree search ([`measure_tree`]), or the file walk
/// ([`measure_set`]) when the tree search cannot be used at all.
pub fn measure(set: &[PathBuf], keep: &[PathBuf], lim: &Limits) -> Result<SetMeasure, WalkError> {
    match measure_tree(set, keep, lim) {
        Err(WalkError::Unsupported(e)) => {
            (lim.progress)(&format!("{e}; walking the files instead"));
            measure_set(set, keep, lim)
        }
        r => r,
    }
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

    /// A `btrfs_file_extent_item` as it is on disk.
    fn item(kind: u8, bytenr: u64, len: u64) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&7u64.to_le_bytes()); // generation
        v.extend_from_slice(&len.to_le_bytes()); // ram_bytes
        v.extend_from_slice(&[0, 0, 0, 0]); // compression, encryption, other_encoding
        v.push(kind);
        if kind == FILE_EXTENT_INLINE {
            v.extend_from_slice(b"inline data");
            return v;
        }
        v.extend_from_slice(&bytenr.to_le_bytes());
        v.extend_from_slice(&len.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes()); // offset
        v.extend_from_slice(&len.to_le_bytes()); // num_bytes
        assert_eq!(v.len(), FILE_EXTENT_ITEM);
        v
    }

    #[test]
    fn root_items_give_their_tree_size() {
        // a btrfs_root_item: inode item (160), generation, root_dirid, bytenr, byte_limit,
        // then bytes_used
        let mut item = vec![0u8; 439];
        item[192..200].copy_from_slice(&3_222_749_184u64.to_le_bytes());
        assert_eq!(root_item_bytes_used(&item), Some(3_222_749_184));
        assert_eq!(root_item_bytes_used(&item[..199]), None);
    }

    #[test]
    fn file_extent_items_are_read() {
        assert_eq!(parse_file_extent(&item(0, 0, 0)), Some(FileExtent::Inline));
        assert_eq!(parse_file_extent(&item(1, 0, 8192)), Some(FileExtent::Hole));
        assert_eq!(parse_file_extent(&item(1, 1 << 30, 8192)), Some(FileExtent::Data { bytenr: 1 << 30, len: 8192, num_bytes: 8192 }));
        // preallocated extents take space too
        assert_eq!(parse_file_extent(&item(2, 4096, 4096)), Some(FileExtent::Data { bytenr: 4096, len: 4096, num_bytes: 4096 }));
        // short or unknown: nothing rather than a guess
        assert_eq!(parse_file_extent(&item(1, 4096, 4096)[..40]), None);
        let mut odd = item(1, 4096, 4096);
        odd[20] = 9;
        assert_eq!(parse_file_extent(&odd), None);
        assert_eq!(parse_file_extent(&[]), None);
    }

    #[test]
    fn packing_keeps_order_and_size() {
        let a = pack(4096, 128 << 20).unwrap();
        let b = pack(8192, 4096).unwrap();
        assert!(a < b);
        assert_eq!(packed_len(a), 128 << 20);
        assert_eq!(packed_len(b), 4096);
        assert_eq!(pack(4097, 4096), None);
        assert_eq!(pack(4096, 0), None);
        assert_eq!(pack(4096, 256 << 20), None);
        assert_eq!(pack(1 << 60, 4096), None);
        assert_eq!(next_key((5, 108, 7)), Some((5, 108, 8)));
        assert_eq!(next_key((5, 108, u64::MAX)), Some((5, 109, 0)));
        assert_eq!(next_key((5, 255, u64::MAX)), Some((6, 0, 0)));
        assert_eq!(next_key((u64::MAX, 255, u64::MAX)), None);
    }

    fn header(objectid: u64, kind: u32, offset: u64, len: usize) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&1u64.to_ne_bytes());
        v.extend_from_slice(&objectid.to_ne_bytes());
        v.extend_from_slice(&offset.to_ne_bytes());
        v.extend_from_slice(&kind.to_ne_bytes());
        v.extend_from_slice(&(len as u32).to_ne_bytes());
        v
    }

    #[test]
    fn search_results_give_the_data_extents_only() {
        let mut buf = Vec::new();
        let inode = vec![0u8; 160];
        buf.extend(header(257, 1, 0, inode.len())); // INODE_ITEM
        buf.extend(&inode);
        for (off, it) in [(0u64, item(1, 1 << 20, 8192)), (8192, item(1, 0, 4096)), (12288, item(0, 0, 0))] {
            buf.extend(header(257, EXTENT_DATA_KEY, off, it.len()));
            buf.extend(it);
        }
        buf.extend(header(258, 12, 256, 14)); // INODE_REF
        buf.extend([0u8; 14]);
        let mut seen = Vec::new();
        let last = search_items(&buf, 5, &mut |r| {
            seen.push(r);
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, vec![DataRef { ino: 257, offset: 0, bytenr: 1 << 20, len: 8192, num_bytes: 8192 }]);
        assert_eq!(last, Some((258, 12, 256)));
        // a result cut short is an error, never a smaller count
        assert!(search_items(&buf[..buf.len() - 4], 5, &mut |_| Ok(())).is_err());
    }

    #[test]
    fn freed_counts_whole_extents_once_and_only_without_kept_references() {
        const M: u64 = 1 << 20;
        // two snapshots of the set share extent A; B only in the set; C also kept (another
        // file range of it); D only kept
        let set = vec![vec![(10 * M, M), (20 * M, 2 * M), (30 * M, 4 * M)], vec![(10 * M, M), (30 * M, 4 * M)]];
        let keep = vec![vec![(30 * M, 4 * M), (40 * M, M)]];
        let (freed, n) = freed_by_extents(&set, &keep, 100).unwrap();
        assert_eq!(n, 3);
        assert_eq!(freed, M + 2 * M);
        // nothing kept: everything distinct
        assert_eq!(freed_by_extents(&set, &[], 100).unwrap().0, 7 * M);
        // more distinct extents than allowed: not measured
        let many: Vec<(u64, u64)> = (1..=200_000u64).map(|i| (i * 4096, 4096)).collect();
        assert_eq!(freed_by_extents(&[many], &[], 1000).unwrap_err(), WalkError::TooLarge);
        // an extent that does not fit the expected layout: not measured
        assert!(freed_by_extents(&[vec![(4097, 4096)]], &[], 100).is_err());
    }

    #[test]
    fn a_folder_that_is_not_a_subvolume_falls_back_to_the_file_walk() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f"), b"x").unwrap();
        let lines = std::sync::Mutex::new(Vec::new());
        let progress = |l: &str| lines.lock().unwrap().push(l.to_string());
        let lim = Limits { max_ranges: 1000, max_extents: 1000, stop: &|| false, progress: &progress };
        assert!(matches!(measure_tree(&[d.path().to_path_buf()], &[], &lim), Err(WalkError::Unsupported(_))));
        let m = measure(&[d.path().to_path_buf()], &[], &lim).unwrap();
        assert_eq!(m.files, 1);
        assert!(lines.lock().unwrap().iter().any(|l| l.contains("walking the files")));
    }
}
