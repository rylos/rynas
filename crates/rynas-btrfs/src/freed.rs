//! What deleting a selection of snapshots gives back: the data extents that only the
//! selection references (Space preview, `docs/phase2/space.md` §5.2).
//!
//! Reading whole snapshots is the wrong shape on a small NAS (one pass over the tree of a
//! PC backup snapshot with 3.3 million files takes about 6 minutes on a J4125 with disks), so
//! the work is a diff, then an exact check of what the diff found:
//!
//! 1. **Candidates.** The snapshots of a line (`@`, `@home`, a share job…) are ordered by
//!    time. For each run of selected snapshots, every member is compared with its adjacent
//!    snapshot on one side, the same side for the whole run: the older one when the run has
//!    an older neighbour, else the newer one. The comparison gives the file ranges of the
//!    member whose data extent differs from what the neighbour has at the same place:
//!    - [`Diff::Since`]: a tree search of the member restricted to tree blocks written at or
//!      after a transaction (the kernel skips every older subtree). Valid when every older
//!      block of the member is also a block of the older neighbour: the member is a snapshot
//!      of the neighbour (an incremental receive) and the neighbour has not changed since, or
//!      both are snapshots of the same subvolume and the older one has not changed since it
//!      was taken. Then the member's items in older blocks are the neighbour's items. Items
//!      of the changed blocks that the neighbour has at the same place are left out.
//!    - [`Diff::Send`]: `BTRFS_IOC_SEND` with `BTRFS_SEND_FLAG_NO_FILE_DATA` and the
//!      neighbour as parent (both read-only): the kernel's tree compare. `UPDATE_EXTENT`,
//!      `WRITE` and `CLONE` give `(path, offset, length)`; renames later in the stream are
//!      applied to the recorded paths, which are then opened in the member (no symbolic link
//!      followed) for their inode, and the member's tree gives the extents of those ranges.
//!
//!    Why this finds every extent only the selection references: take such an extent `E`
//!    and a member that references it at some place (inode, file offset). Walk the run
//!    towards the compared side while the next snapshot references `E` at the same place:
//!    every snapshot on the way references `E`, so it is a member (otherwise `E` is kept),
//!    and the walk ends at a member whose compared neighbour does not reference `E` there,
//!    which its diff then reports. The walk cannot leave the run: the neighbour beyond the
//!    run's end is not a member. A run with no neighbour on either side (or whose
//!    comparisons are not possible) makes the whole answer [`WalkError::Unsupported`]: the
//!    caller then uses the full walk ([`crate::extents::measure`]).
//! 2. **Extents.** Candidates are deduplicated by their start (`disk_bytenr`); a removed
//!    extent is freed whole.
//! 3. **Exact check.** `BTRFS_IOC_LOGICAL_INO_V2` with `IGNORE_OFFSET` lists every
//!    `(inode, offset, root)` that references a candidate, across the filesystem, including
//!    reflinks and deduplicated copies in other shares. A candidate is freed when every root
//!    is a selected snapshot. A list that does not fit 16 MiB counts as kept.
//!
//! The result is the on-disk size (compressed extents count what they take) of the extents
//! freed. It leaves out the metadata only the selection holds (tree blocks, inline data of
//! small files), so it can only be low by that.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::extents::{DataRef, Limits, SetMeasure, WalkError};

/// `BTRFS_IOC_SEND` = `_IOW(0x94, 38, struct btrfs_ioctl_send_args)` (72 bytes).
const IOC_SEND: u64 = (1 << 30) | (72 << 16) | (0x94 << 8) | 38;
const SEND_FLAG_NO_FILE_DATA: u64 = 1;
/// `BTRFS_IOC_LOGICAL_INO_V2` = `_IOWR(0x94, 59, struct btrfs_ioctl_logical_ino_args)`.
const IOC_LOGICAL_INO_V2: u64 = (3 << 30) | (56 << 16) | (0x94 << 8) | 59;
const LOGICAL_INO_IGNORE_OFFSET: u64 = 1;
/// First and largest reference list asked of `LOGICAL_INO_V2`.
const REFS_FIRST: usize = 64 << 10;
const REFS_MAX: usize = 16 << 20;
/// Largest no-data send stream read for one comparison.
const MAX_STREAM: usize = 512 << 20;
/// `EXTENT_DATA` key type.
const EXTENT_DATA_KEY: u32 = 108;

const STREAM_MAGIC: &[u8; 13] = b"btrfs-stream\0";
const CMD_RENAME: u16 = 9;
const CMD_UNLINK: u16 = 11;
const CMD_RMDIR: u16 = 12;
const CMD_WRITE: u16 = 15;
const CMD_CLONE: u16 = 16;
const CMD_END: u16 = 21;
const CMD_UPDATE_EXTENT: u16 = 22;
const ATTR_SIZE: u16 = 4;
const ATTR_PATH: u16 = 15;
const ATTR_PATH_TO: u16 = 16;
const ATTR_FILE_OFFSET: u16 = 18;
const ATTR_DATA: u16 = 19;
const ATTR_CLONE_LEN: u16 = 24;

#[repr(C)]
struct SendArgs {
    send_fd: i64,
    clone_sources_count: u64,
    clone_sources: u64,
    parent_root: u64,
    flags: u64,
    version: u32,
    reserved: [u8; 28],
}

#[repr(C)]
struct LogicalInoArgs {
    logical: u64,
    size: u64,
    reserved: [u64; 3],
    flags: u64,
    inodes: u64,
}

/// What the plan needs to know about one snapshot of a line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Member {
    pub in_set: bool,
    pub readonly: bool,
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    /// Transaction that created it.
    pub otransid: u64,
    /// Last transaction that changed it.
    pub generation: u64,
}

/// One comparison of the plan (indexes into the line, oldest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diff {
    /// The member's tree blocks written at `min_transid` or later.
    Since { member: usize, min_transid: u64 },
    /// A no-data send of the member against `parent`.
    Send { member: usize, parent: usize },
}

fn send_diff(line: &[Member], m: usize, p: usize) -> Option<Diff> {
    (line[m].readonly && line[p].readonly).then_some(Diff::Send { member: m, parent: p })
}

/// The comparison of member `m` with its older neighbour `m - 1`.
fn older_diff(line: &[Member], m: usize) -> Option<Diff> {
    let (s, o) = (&line[m], &line[m - 1]);
    if s.parent_uuid.is_some() {
        // a snapshot of the older one (incremental receive), which has not changed since
        if s.parent_uuid == o.uuid && o.generation <= s.otransid {
            return Some(Diff::Since { member: m, min_transid: s.otransid });
        }
        // both snapshots of one subvolume, the older one unchanged since it was taken
        if s.parent_uuid == o.parent_uuid && o.otransid < s.otransid && o.generation <= o.otransid {
            return Some(Diff::Since { member: m, min_transid: o.otransid });
        }
    }
    send_diff(line, m, m - 1)
}

/// The comparisons that find every extent only the selected members of `line` (oldest
/// first) reference; `None`: a run of selected members can be compared on neither side.
pub fn plan_line(line: &[Member]) -> Option<Vec<Diff>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < line.len() {
        if !line[i].in_set {
            i += 1;
            continue;
        }
        let a = i;
        while i < line.len() && line[i].in_set {
            i += 1;
        }
        let older = if a > 0 { (a..i).map(|m| older_diff(line, m)).collect::<Option<Vec<_>>>() } else { None };
        let run = match older {
            Some(d) => d,
            None if i < line.len() => (a..i).map(|m| send_diff(line, m, m + 1)).collect::<Option<Vec<_>>>()?,
            None => return None,
        };
        out.extend(run);
    }
    Some(out)
}

/// A command of a stream: its type and its attributes by type.
type Command<'a> = (u16, BTreeMap<u16, &'a [u8]>);
/// File ranges `(offset, length)` by path.
pub type PathRanges = BTreeMap<String, Vec<(u64, u64)>>;
/// References `(inode, offset, root)` and how many did not fit.
pub type Refs = (Vec<(u64, u64, u64)>, u32);
/// One line of snapshots, oldest first: `(path, selected)`.
pub type Line = Vec<(PathBuf, bool)>;

/// The commands of a stream.
fn commands(data: &[u8]) -> Result<Vec<Command<'_>>, String> {
    let rest = data.strip_prefix(STREAM_MAGIC.as_slice()).ok_or("not a send stream")?;
    let version = rest.get(..4).map(|v| u32::from_le_bytes([v[0], v[1], v[2], v[3]])).ok_or("short stream")?;
    if version != 1 {
        return Err(format!("send stream version {version}"));
    }
    let (mut pos, rest) = (0usize, &rest[4..]);
    let mut out = Vec::new();
    while pos < rest.len() {
        let h = rest.get(pos..pos + 10).ok_or("truncated command")?;
        let len = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
        let cmd = u16::from_le_bytes([h[4], h[5]]);
        let crc = u32::from_le_bytes([h[6], h[7], h[8], h[9]]);
        let body = rest.get(pos + 10..pos + 10 + len).ok_or("truncated command")?;
        let c = crate::send_stream::crc32c(crate::send_stream::crc32c(crate::send_stream::crc32c(0, &h[..6]), &[0; 4]), body);
        if c != crc {
            return Err(format!("command {cmd}: bad checksum"));
        }
        let mut attrs = BTreeMap::new();
        let mut i = 0;
        while i < body.len() {
            let a = body.get(i..i + 4).ok_or("truncated attribute")?;
            let (t, l) = (u16::from_le_bytes([a[0], a[1]]), u16::from_le_bytes([a[2], a[3]]) as usize);
            attrs.insert(t, body.get(i + 4..i + 4 + l).ok_or("truncated attribute")?);
            i += 4 + l;
        }
        pos += 10 + len;
        let end = cmd == CMD_END;
        out.push((cmd, attrs));
        if end {
            return Ok(out);
        }
    }
    Err("stream without END".into())
}

/// The file ranges `(offset, length)` a no-data send stream reports as changed in the sent
/// subvolume, by the path each file has at the end of the stream (later renames applied).
/// A recorded path that is unlinked later is an error: its file cannot be found again.
pub fn stream_ranges(data: &[u8]) -> Result<PathRanges, String> {
    let mut out = PathRanges::new();
    let text = |a: &BTreeMap<u16, &[u8]>, t: u16| a.get(&t).map(|b| String::from_utf8_lossy(b).into_owned()).ok_or_else(|| format!("attribute {t} missing"));
    let num = |a: &BTreeMap<u16, &[u8]>, t: u16| {
        a.get(&t).and_then(|b| <[u8; 8]>::try_from(*b).ok()).map(u64::from_le_bytes).ok_or_else(|| format!("attribute {t} missing"))
    };
    for (cmd, a) in commands(data)? {
        match cmd {
            CMD_UPDATE_EXTENT | CMD_WRITE | CMD_CLONE => {
                let len = match cmd {
                    CMD_UPDATE_EXTENT => num(&a, ATTR_SIZE)?,
                    CMD_WRITE => a.get(&ATTR_DATA).map_or(0, |d| d.len() as u64),
                    _ => num(&a, ATTR_CLONE_LEN)?,
                };
                if len > 0 {
                    out.entry(text(&a, ATTR_PATH)?).or_default().push((num(&a, ATTR_FILE_OFFSET)?, len));
                }
            }
            CMD_RENAME => {
                let (from, to) = (text(&a, ATTR_PATH)?, text(&a, ATTR_PATH_TO)?);
                for (k, v) in take_under(&mut out, &from) {
                    out.entry(format!("{to}{}", &k[from.len()..])).or_default().extend(v);
                }
            }
            CMD_UNLINK | CMD_RMDIR => {
                let p = text(&a, ATTR_PATH)?;
                if !take_under(&mut out, &p).is_empty() {
                    return Err(format!("{p}: changed, then removed from the stream's tree"));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Removes and returns the entries for `p` and everything below it.
fn take_under(m: &mut PathRanges, p: &str) -> Vec<(String, Vec<(u64, u64)>)> {
    let below = format!("{p}/");
    let keys: Vec<String> = m.range(p.to_string()..).take_while(|(k, _)| k.starts_with(p)).filter(|(k, _)| *k == p || k.starts_with(&below)).map(|(k, _)| k.clone()).collect();
    keys.into_iter().filter_map(|k| m.remove(&k).map(|v| (k, v))).collect()
}

/// The roots that reference an extent: the triples `(inode, offset, root)` of a
/// `btrfs_data_container`, and how many did not fit.
pub fn parse_refs(buf: &[u8]) -> Option<Refs> {
    let u32_at = |a: usize| buf.get(a..a + 4).map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]));
    let (count, missed) = (u32_at(8)? as usize, u32_at(12)?);
    let vals: Vec<u64> = (0..count).map(|i| buf.get(16 + 8 * i..24 + 8 * i).map(|b| u64::from_ne_bytes(b.try_into().unwrap_or([0; 8])))).collect::<Option<_>>()?;
    Some((vals.as_chunks::<3>().0.iter().map(|c| (c[0], c[1], c[2])).collect(), missed))
}

/// The roots referencing the extent at `bytenr`; `None`: the list does not fit 16 MiB.
fn extent_roots(fd: &File, bytenr: u64) -> Result<Option<BTreeSet<u64>>, WalkError> {
    let mut size = REFS_FIRST;
    loop {
        let mut buf = vec![0u64; size / 8];
        let mut a = LogicalInoArgs { logical: bytenr, size: size as u64, reserved: [0; 3], flags: LOGICAL_INO_IGNORE_OFFSET, inodes: buf.as_mut_ptr() as u64 };
        // SAFETY: the kernel writes at most `size` bytes into `buf`
        if unsafe { libc::ioctl(fd.as_raw_fd(), IOC_LOGICAL_INO_V2 as _, &mut a) } < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENOENT) {
                // no longer an extent: nothing to free
                return Ok(Some(BTreeSet::new()));
            }
            return Err(WalkError::Unsupported(format!("LOGICAL_INO: {e}")));
        }
        // SAFETY: the words as bytes
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, size) };
        let (refs, missed) = parse_refs(bytes).ok_or_else(|| WalkError::Io("malformed LOGICAL_INO result".into()))?;
        if missed == 0 {
            return Ok(Some(refs.into_iter().map(|r| r.2).collect()));
        }
        if size >= REFS_MAX {
            return Ok(None);
        }
        size = (size * 4).min(REFS_MAX);
    }
}

/// The no-data send stream of `sub` against the subvolume `parent_root`.
fn send_stream(sub: &File, parent_root: u64, stop: &dyn Fn() -> bool) -> Result<Vec<u8>, WalkError> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe2 fills two descriptors
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(WalkError::Io(io::Error::last_os_error().to_string()));
    }
    // SAFETY: descriptors just made, owned here
    let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let sub = sub.try_clone().map_err(|e| WalkError::Io(e.to_string()))?;
    let sender = std::thread::spawn(move || {
        let a = SendArgs { send_fd: wr.as_raw_fd() as i64, clone_sources_count: 0, clone_sources: 0, parent_root, flags: SEND_FLAG_NO_FILE_DATA, version: 0, reserved: [0; 28] };
        // SAFETY: the kernel reads the arguments and writes the stream to `wr`
        let r = unsafe { libc::ioctl(sub.as_raw_fd(), IOC_SEND as _, &a) };
        let e = (r < 0).then(io::Error::last_os_error);
        drop(wr);
        e
    });
    let mut f = File::from(rd);
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 1 << 16];
    let mut fail = None;
    loop {
        if stop() {
            fail = Some(WalkError::Stopped);
            break;
        }
        let mut p = libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd
        if unsafe { libc::poll(&mut p, 1, 1000) } == 0 {
            continue;
        }
        match f.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) if out.len() + n > MAX_STREAM => {
                fail = Some(WalkError::TooLarge);
                break;
            }
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                fail = Some(WalkError::Io(e.to_string()));
                break;
            }
        }
    }
    // closing the read end makes a send still writing fail (EPIPE)
    drop(f);
    let sent = sender.join().map_err(|_| WalkError::Io("send thread".into()))?;
    if let Some(e) = fail {
        return Err(e);
    }
    match sent {
        Some(e) => Err(WalkError::Unsupported(format!("send: {e}"))),
        None => Ok(out),
    }
}

/// The inode of `path` inside the subvolume `root` (no symbolic link followed, a regular
/// file on the subvolume's own device).
fn inode_at(root: &File, path: &str) -> Option<u64> {
    let dev = root.metadata().ok()?.dev();
    let mut parts: Vec<&str> = path.split('/').collect();
    let last = parts.pop()?;
    if last.is_empty() || parts.iter().chain([&last]).any(|c| c.is_empty() || *c == "." || *c == "..") {
        return None;
    }
    let mut dir = root.try_clone().ok()?;
    for c in parts {
        dir = crate::ioctl::open_dir_at(&dir, c).ok()?;
    }
    let name = CString::new(last).ok()?;
    // SAFETY: fstatat writes into the struct
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return None;
    }
    (st.st_mode & libc::S_IFMT == libc::S_IFREG && st.st_dev == dev).then_some(st.st_ino)
}

/// Whether a file range `[off, off + len)` meets any of `ranges` (sorted by start).
fn meets(ranges: &[(u64, u64)], off: u64, len: u64) -> bool {
    let end = off.saturating_add(len);
    ranges.iter().any(|&(o, l)| o < end && off < o.saturating_add(l))
}

/// One opened snapshot of a line.
struct Opened {
    path: PathBuf,
    file: File,
    root: u64,
    facts: Member,
}

fn open(path: &Path, in_set: bool) -> Result<Opened, WalkError> {
    let unsupported = |e: String| WalkError::Unsupported(format!("{}: {e}", path.display()));
    let file = crate::ioctl::open_dir(path).map_err(|e| unsupported(e.to_string()))?;
    if !crate::ioctl::is_subvolume(&file) {
        return Err(unsupported("not a subvolume".into()));
    }
    let i = crate::ioctl::subvol_info(&file).map_err(|e| unsupported(e.to_string()))?;
    let facts = Member { in_set, readonly: i.readonly, uuid: i.uuid, parent_uuid: i.parent_uuid, otransid: i.otransid, generation: i.generation };
    Ok(Opened { path: path.to_path_buf(), file, root: i.root_id, facts })
}

/// Candidate extents (start → on-disk size) of one comparison.
fn diff_extents(line: &[Opened], d: Diff, lim: &Limits, cands: &mut BTreeMap<u64, u64>) -> Result<(), WalkError> {
    let push = |r: DataRef, cands: &mut BTreeMap<u64, u64>| {
        cands.insert(r.bytenr, r.len);
        if cands.len() > lim.max_extents { Err(WalkError::TooLarge) } else { Ok(()) }
    };
    match d {
        Diff::Since { member, min_transid } => {
            let (m, o) = (&line[member], &line[member - 1]);
            let before = cands.len();
            // the items of the changed blocks, one inode at a time; those the older neighbour
            // has at the same place (same file offset, same extent) are left out
            let mut cur: Vec<DataRef> = Vec::new();
            let flush = |cur: &mut Vec<DataRef>, cands: &mut BTreeMap<u64, u64>| -> Result<(), WalkError> {
                let Some(first) = cur.first().copied() else { return Ok(()) };
                let hi = cur.iter().map(|r| r.offset).max().unwrap_or(first.offset);
                let lo = cur.iter().map(|r| r.offset).min().unwrap_or(first.offset);
                let mut same: HashSet<(u64, u64)> = HashSet::new();
                crate::extents::search_extent_items(&o.file, o.root, (first.ino, EXTENT_DATA_KEY, lo), (first.ino, EXTENT_DATA_KEY, hi), 0, lim.stop, &mut |x| {
                    same.insert((x.offset, x.bytenr));
                    Ok(())
                })?;
                for r in cur.drain(..) {
                    if !same.contains(&(r.offset, r.bytenr)) {
                        push(r, cands)?;
                    }
                }
                Ok(())
            };
            crate::extents::search_extent_items(&m.file, m.root, (256, 0, 0), (u64::MAX, 255, u64::MAX), min_transid, lim.stop, &mut |r| {
                if cur.first().is_some_and(|c| c.ino != r.ino) {
                    flush(&mut cur, cands)?;
                }
                cur.push(r);
                Ok(())
            })?;
            flush(&mut cur, cands)?;
            (lim.progress)(&format!("{}: {} changed extents (blocks since transaction {min_transid})", m.path.display(), cands.len() - before));
        }
        Diff::Send { member, parent } => {
            let (m, p) = (&line[member], &line[parent]);
            let stream = send_stream(&m.file, p.root, lim.stop)?;
            let ranges = stream_ranges(&stream).map_err(|e| WalkError::Unsupported(format!("{}: {e}", m.path.display())))?;
            let mut by_ino: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
            for (path, r) in ranges {
                let ino = inode_at(&m.file, &path).ok_or_else(|| WalkError::Unsupported(format!("{}: {path} not found after the send", m.path.display())))?;
                by_ino.entry(ino).or_default().extend(r);
            }
            let before = cands.len();
            for (ino, mut r) in by_ino {
                r.sort_unstable();
                let end = r.iter().map(|&(o, l)| o.saturating_add(l)).max().unwrap_or(0);
                crate::extents::search_extent_items(&m.file, m.root, (ino, EXTENT_DATA_KEY, 0), (ino, EXTENT_DATA_KEY, end.saturating_sub(1)), 0, lim.stop, &mut |x| {
                    if meets(&r, x.offset, x.num_bytes) { push(x, cands) } else { Ok(()) }
                })?;
            }
            (lim.progress)(&format!("{}: {} changed extents (against {})", m.path.display(), cands.len() - before, p.path.display()));
        }
    }
    Ok(())
}

/// What removing the selected snapshots of `lines` frees (each line oldest first, `true`
/// for a selected snapshot; all on one filesystem), by the method of the module text.
/// [`WalkError::Unsupported`]: this method cannot answer, the caller walks instead.
pub fn measure_lines(lines: &[Line], lim: &Limits) -> Result<SetMeasure, WalkError> {
    let mut opened: Vec<Vec<Opened>> = Vec::new();
    let mut plans = Vec::new();
    for line in lines {
        let o: Vec<Opened> = line.iter().map(|(p, s)| open(p, *s)).collect::<Result<_, _>>()?;
        let facts: Vec<Member> = o.iter().map(|x| x.facts.clone()).collect();
        let plan = plan_line(&facts).ok_or_else(|| {
            WalkError::Unsupported(format!("{}: no read-only neighbour to compare with", o.iter().find(|x| x.facts.in_set).map(|x| x.path.display().to_string()).unwrap_or_default()))
        })?;
        opened.push(o);
        plans.push(plan);
    }
    let set_roots: HashSet<u64> = opened.iter().flatten().filter(|x| x.facts.in_set).map(|x| x.root).collect();
    let Some(any) = opened.iter().flatten().next() else { return Ok(SetMeasure::default()) };
    let mut cands: BTreeMap<u64, u64> = BTreeMap::new();
    for (line, plan) in opened.iter().zip(&plans) {
        for d in plan {
            diff_extents(line, *d, lim, &mut cands)?;
        }
    }
    let mut m = SetMeasure { extents: cands.len() as u64, ..SetMeasure::default() };
    let (mut only, mut truncated) = (0u64, 0u64);
    for (i, (&bytenr, &len)) in cands.iter().enumerate() {
        if i % 256 == 0 && (lim.stop)() {
            return Err(WalkError::Stopped);
        }
        match extent_roots(&any.file, bytenr)? {
            Some(roots) if !roots.is_empty() && roots.iter().all(|r| set_roots.contains(r)) => {
                m.freed += len;
                only += 1;
            }
            Some(_) => {}
            None => truncated += 1,
        }
    }
    (lim.progress)(&format!("{} candidate extents, {only} referenced only by the selection", cands.len()));
    if truncated > 0 {
        (lim.progress)(&format!("{truncated} extents with too many references to list: counted as kept"));
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::send_stream::fake::{self, cmd};

    fn m(in_set: bool, uuid: &str, parent: &str, otransid: u64, generation: u64) -> Member {
        let o = |s: &str| (!s.is_empty()).then(|| s.to_string());
        Member { in_set, readonly: true, uuid: o(uuid), parent_uuid: o(parent), otransid, generation }
    }

    /// A line of incremental receives: each a snapshot of the one before, unchanged after.
    fn received(sel: &[bool]) -> Vec<Member> {
        sel.iter()
            .enumerate()
            .map(|(i, &s)| {
                let parent = if i == 0 { String::new() } else { format!("u{}", i - 1) };
                m(s, &format!("u{i}"), &parent, 100 * (i as u64 + 1), 100 * (i as u64 + 1) + 50)
            })
            .collect()
    }

    #[test]
    fn runs_compare_with_the_older_neighbour_when_there_is_one() {
        // kept, set, set, kept: both members against the one before, by tree blocks
        let line = received(&[false, true, true, false]);
        assert_eq!(plan_line(&line), Some(vec![Diff::Since { member: 1, min_transid: 200 }, Diff::Since { member: 2, min_transid: 300 }]));
        // the oldest end: no older neighbour, so the whole run against the newer side
        let line = received(&[true, true, false]);
        assert_eq!(plan_line(&line), Some(vec![Diff::Send { member: 0, parent: 1 }, Diff::Send { member: 1, parent: 2 }]));
        // two runs
        let line = received(&[true, false, true, false]);
        assert_eq!(plan_line(&line), Some(vec![Diff::Send { member: 0, parent: 1 }, Diff::Since { member: 2, min_transid: 300 }]));
        // a whole line selected: no neighbour on either side
        assert_eq!(plan_line(&received(&[true, true])), None);
        assert_eq!(plan_line(&received(&[false, false])), Some(vec![]));
    }

    #[test]
    fn tree_blocks_are_used_only_when_the_neighbour_holds_every_older_block() {
        // the older one changed after the newer was made from it: send instead
        let mut line = received(&[false, true]);
        line[0].generation = 250;
        assert_eq!(plan_line(&line), Some(vec![Diff::Send { member: 1, parent: 0 }]));
        // made from another snapshot: send
        let mut line = received(&[false, true]);
        line[1].parent_uuid = Some("elsewhere".into());
        assert_eq!(plan_line(&line), Some(vec![Diff::Send { member: 1, parent: 0 }]));
        // snapshots of one live subvolume, the older unchanged since: since the older's creation
        let line = vec![m(false, "a", "live", 100, 100), m(true, "b", "live", 200, 200)];
        assert_eq!(plan_line(&line), Some(vec![Diff::Since { member: 1, min_transid: 100 }]));
        // the older one written to after it was taken: send
        let line = vec![m(false, "a", "live", 100, 150), m(true, "b", "live", 200, 200)];
        assert_eq!(plan_line(&line), Some(vec![Diff::Send { member: 1, parent: 0 }]));
        // a writable neighbour cannot be sent against: the other side
        let mut line = vec![m(false, "a", "", 100, 150), m(true, "b", "", 200, 200), m(false, "c", "", 300, 300)];
        line[0].readonly = false;
        assert_eq!(plan_line(&line), Some(vec![Diff::Send { member: 1, parent: 2 }]));
        line[2].readonly = false;
        assert_eq!(plan_line(&line), None);
    }

    fn stream(body: &[Vec<u8>]) -> Vec<u8> {
        let mut s = fake::header(1);
        s.extend(fake::first("snap", "0d7dc2d7-878a-5446-8596-6b5b6b021473", Some("4704ff2c-f735-d741-b649-cc1f58aea7f6")));
        for c in body {
            s.extend_from_slice(c);
        }
        s.extend(cmd(fake::END, &[]));
        s
    }

    fn ext(path: &str, off: u64, len: u64) -> Vec<u8> {
        cmd(fake::UPDATE_EXTENT, &[(fake::PATH, path.as_bytes()), (fake::FILE_OFFSET, &off.to_le_bytes()), (fake::SIZE, &len.to_le_bytes())])
    }

    fn rename(from: &str, to: &str) -> Vec<u8> {
        cmd(fake::RENAME, &[(fake::PATH, from.as_bytes()), (fake::PATH_TO, to.as_bytes())])
    }

    #[test]
    fn stream_ranges_follow_later_renames() {
        let s = stream(&[
            cmd(fake::MKFILE, &[(fake::PATH, b"o257-7-0")]),
            ext("o257-7-0", 0, 4096),
            ext("o258-7-0/new.txt", 8192, 100),
            cmd(fake::CLONE, &[(fake::PATH, b"docs/a"), (fake::FILE_OFFSET, &0u64.to_le_bytes()), (fake::CLONE_LEN, &65536u64.to_le_bytes())]),
            ext("docs/a", 1 << 20, 4096),
            rename("o257-7-0", "docs/b"),
            rename("o258-7-0", "docs/sub"),
            // a sibling with a common prefix is not below the renamed folder
            ext("docs/sub2", 0, 1),
            rename("docs/sub", "docs/sub_final"),
        ]);
        let r = stream_ranges(&s).unwrap();
        let want: BTreeMap<String, Vec<(u64, u64)>> = [
            ("docs/a".to_string(), vec![(0, 65536), (1 << 20, 4096)]),
            ("docs/b".to_string(), vec![(0, 4096)]),
            ("docs/sub2".to_string(), vec![(0, 1)]),
            ("docs/sub_final/new.txt".to_string(), vec![(8192, 100)]),
        ]
        .into_iter()
        .collect();
        assert_eq!(r, want);
        // a changed file removed later cannot be found again: no answer rather than a low one
        let s = stream(&[ext("x", 0, 4096), cmd(CMD_UNLINK, &[(fake::PATH, b"x")])]);
        assert!(stream_ranges(&s).is_err());
        // a damaged stream
        let mut s = stream(&[ext("x", 0, 4096)]);
        let n = s.len();
        s[n - 12] ^= 1;
        assert!(stream_ranges(&s).is_err());
        assert!(stream_ranges(&s[..n - 10]).is_err());
    }

    #[test]
    fn reference_lists_are_read_with_what_did_not_fit() {
        let mut b = Vec::new();
        for v in [0u32, 0, 6, 2] {
            b.extend_from_slice(&v.to_ne_bytes());
        }
        for v in [257u64, 0, 300, 258, 4096, 301] {
            b.extend_from_slice(&v.to_ne_bytes());
        }
        assert_eq!(parse_refs(&b), Some((vec![(257, 0, 300), (258, 4096, 301)], 2)));
        assert_eq!(parse_refs(&b[..20]), None);
        assert!(meets(&[(0, 10), (100, 10)], 5, 1));
        assert!(meets(&[(0, 10), (100, 10)], 50, 51));
        assert!(!meets(&[(0, 10), (100, 10)], 10, 90));
    }
}
