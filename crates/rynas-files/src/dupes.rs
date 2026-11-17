//! Duplicate files under a folder (route `duplicates`). Files of the same size are
//! candidates (and of the same name, by name); by content their first and last 64 KiB are
//! hashed first, and only those still alike are read whole (xxh3-128). Hard links to one
//! file are one file, not duplicates. Everything is read as the user, inside the jail of the
//! request ([`crate::confine`]): folders with [`crate::confine::read_dir`], files with
//! [`crate::confine::open`], links never followed. The answer streams JSON lines like the
//! checksum's: progress, then the groups.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::wire::{self as cgi, ApiError};

/// What is hashed at each end of a file before the whole of it.
const EDGE: u64 = 64 * 1024;
/// Groups answered at most (the ones with the most room wasted).
const MAX_GROUPS: usize = 5000;
/// Files looked at at most: past this the search stops and says so (`more`). The worker
/// keeps one small entry per size (a first pass) and the candidates only (a second).
const MAX_FILES: u64 = 500_000;
const TICK: Duration = Duration::from_millis(500);

/// A candidate: its path below the root and what it was when seen.
struct Found {
    rel: Box<str>,
    size: u64,
    mtime: i64,
}

/// What a walk of the tree hands its visitor: a file's path below the root, its name, its
/// metadata.
struct Seen<'a> {
    rel: &'a Path,
    name: &'a str,
    meta: &'a std::fs::Metadata,
}

/// The walk of both passes: regular files below `root`, never through a link, the NAS's
/// bookkeeping and the file manager's own names skipped, at most [`MAX_FILES`]. A name that
/// is not UTF-8 cannot be named back to the interface: skipped and counted. Answers
/// (files seen, names skipped, whether it stopped at the limit).
fn walk(root: &Path, mut visit: impl FnMut(Seen) -> io::Result<()>) -> io::Result<(u64, u64, bool)> {
    let (mut files, mut skipped) = (0u64, 0u64);
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(rd) = crate::confine::read_dir(&root.join(&rel)) else { continue };
        for item in rd.flatten() {
            let raw = item.file_name();
            let Some(name) = raw.to_str() else {
                skipped += 1;
                continue;
            };
            if crate::fsops::is_system_name(name) || name == crate::shares::RECYCLE || name == "#snapshot" {
                continue;
            }
            let Ok(kind) = item.file_type() else { continue };
            if kind.is_dir() {
                stack.push(rel.join(name));
                continue;
            }
            // a link is not followed, a device or a pipe is not read
            if !kind.is_file() {
                continue;
            }
            let Ok(meta) = item.metadata() else { continue };
            if files >= MAX_FILES {
                return Ok((files, skipped, true));
            }
            files += 1;
            visit(Seen { rel: &rel, name, meta: &meta })?;
        }
    }
    Ok((files, skipped, false))
}

#[derive(Serialize)]
pub struct Item {
    pub path: String,
    pub mtime: i64,
}

#[derive(Serialize)]
pub struct Group {
    pub size: u64,
    pub items: Vec<Item>,
}

/// Throttled progress lines; an error writing them means the browser went away.
struct Report<'a, W: Write> {
    out: &'a mut W,
    at: Option<Instant>,
}

impl<W: Write> Report<'_, W> {
    fn tick(&mut self, line: impl FnOnce() -> serde_json::Value) -> io::Result<()> {
        if self.at.is_some_and(|at| at.elapsed() < TICK) {
            return Ok(());
        }
        self.at = Some(Instant::now());
        writeln!(self.out, "{}", line())?;
        self.out.flush()
    }
}

/// Answers `duplicates` for the folder `root` (already placed inside the user's share).
/// `by`: "content" or "name" (same name and size, nothing read). `min`: the smallest size
/// looked at.
pub fn send(out: &mut impl Write, root: &Path, by: &str, min: u64) -> Result<(), ApiError> {
    let by_name = match by {
        "content" => false,
        "name" => true,
        _ => return Err(ApiError::invalid("by", t!("compare by content or by name"))),
    };
    if !std::fs::metadata(root)?.is_dir() {
        return Err(ApiError::invalid("path", t!("not a folder")));
    }
    // the folder itself, checked to lie in the jail before anything is said
    drop(crate::confine::read_dir(root)?);
    cgi::head(out, 200, &[("Content-Type", "application/x-ndjson"), ("X-Accel-Buffering", "no")])?;
    out.flush()?;
    let mut report = Report { out, at: None };
    let result = find(root, by_name, min.max(1), &mut report);
    let out = report.out;
    let line = match result {
        // skipped: names that are not UTF-8, never acted on
        Ok((groups, scanned, skipped, more)) => json!({ "groups": groups, "scanned": scanned, "skipped": skipped, "more": more }),
        // the client went away
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
        Err(e) => {
            let err = ApiError::from(e);
            json!({ "error": err.message, "code": err.code })
        }
    };
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
    Ok(())
}

/// The groups, with what [`Found`] needs: (groups, files looked at, names skipped, more).
fn find<W: Write>(root: &Path, by_name: bool, min: u64, report: &mut Report<W>) -> io::Result<(Vec<Group>, u64, u64, bool)> {
    use std::hash::{Hash, Hasher};
    // the key of a file: its size, and by name a hash of its name in lower case
    let key = |s: &Seen| -> (u64, u64) {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        if by_name {
            s.name.to_lowercase().hash(&mut h);
        }
        (s.meta.len(), if by_name { h.finish() } else { 0 })
    };
    // 1. how many files share each key, once per inode (a hard link is the same file)
    let mut counts: HashMap<(u64, u64), u32> = HashMap::new();
    let mut inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut scanned = 0u64;
    let (_, skipped, more_files) = walk(root, |s| {
        scanned += 1;
        report.tick(|| json!({ "phase": "scan", "files": scanned }))?;
        if s.meta.len() >= min && inodes.insert((s.meta.dev(), s.meta.ino())) {
            *counts.entry(key(&s)).or_default() += 1;
        }
        Ok(())
    })?;
    counts.retain(|_, n| *n > 1);
    inodes.clear();
    inodes.shrink_to_fit();
    // 2. the candidates only: the files whose key another file has
    let mut by_key: HashMap<(u64, u64, Box<str>), Vec<Found>> = HashMap::new();
    if !counts.is_empty() {
        walk(root, |s| {
            let k = key(&s);
            if s.meta.len() >= min && counts.contains_key(&k) && inodes.insert((s.meta.dev(), s.meta.ino())) {
                // by name the group is the exact name in lower case (the hash only narrowed it)
                let name: Box<str> = if by_name { s.name.to_lowercase().into() } else { "".into() };
                by_key.entry((k.0, k.1, name)).or_default().push(Found { rel: s.rel.join(s.name).to_string_lossy().into(), size: s.meta.len(), mtime: s.meta.mtime() });
            }
            Ok(())
        })?;
    }
    drop(counts);
    let mut candidates: Vec<Vec<Found>> = by_key.into_values().filter(|v| v.len() > 1).collect();
    if !by_name {
        // 3. the edges, then 4. the whole of what is still alike
        let total: u64 = candidates.iter().flatten().map(|f| f.size).sum();
        let mut done = 0u64;
        let mut next = Vec::new();
        for group in candidates {
            for g in split(group, |f| edges(&root.join(&*f.rel), f.size))? {
                let bytes = g.iter().map(|f| f.size).sum::<u64>();
                if g.len() < 2 || g[0].size <= 2 * EDGE {
                    // alone, or the edges were the whole file
                    done += bytes;
                    if g.len() > 1 {
                        next.push(g);
                    }
                    continue;
                }
                let mut whole = |f: &Found| {
                    full(&root.join(&*f.rel), &mut |n| {
                        done += n;
                        report.tick(|| json!({ "phase": "hash", "done": done, "total": total }))
                    })
                };
                next.extend(split(g, &mut whole)?.into_iter().filter(|g| g.len() > 1));
            }
            report.tick(|| json!({ "phase": "hash", "done": done, "total": total }))?;
        }
        candidates = next;
    }
    // the most room wasted first
    candidates.sort_by_key(|g| std::cmp::Reverse(g[0].size.saturating_mul(g.len() as u64 - 1)));
    let more = more_files || candidates.len() > MAX_GROUPS;
    let groups = candidates
        .into_iter()
        .take(MAX_GROUPS)
        .map(|mut g| {
            g.sort_by(|a, b| a.rel.cmp(&b.rel));
            Group { size: g[0].size, items: g.into_iter().map(|f| Item { path: f.rel.into(), mtime: f.mtime }).collect() }
        })
        .collect();
    Ok((groups, scanned, skipped, more))
}

/// Splits a group by a key; a file that cannot be read drops out.
fn split<K: std::hash::Hash + Eq>(group: Vec<Found>, mut key: impl FnMut(&Found) -> io::Result<Option<K>>) -> io::Result<Vec<Vec<Found>>> {
    let mut parts: HashMap<K, Vec<Found>> = HashMap::new();
    for f in group {
        if let Some(k) = key(&f)? {
            parts.entry(k).or_default().push(f);
        }
    }
    Ok(parts.into_values().collect())
}

/// The first and last 64 KiB; None when the file cannot be read (or changed size).
fn edges(path: &Path, size: u64) -> io::Result<Option<u128>> {
    let Ok(mut f) = crate::confine::open(path) else { return Ok(None) };
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; EDGE.min(size) as usize];
    if f.read_exact(&mut buf).is_err() {
        return Ok(None);
    }
    h.update(&buf);
    if size > EDGE {
        let tail = EDGE.min(size - EDGE);
        let mut buf = vec![0u8; tail as usize];
        if f.seek(SeekFrom::Start(size - tail)).is_err() || f.read_exact(&mut buf).is_err() {
            return Ok(None);
        }
        h.update(&buf);
    }
    Ok(Some(h.digest128()))
}

/// The whole file; `tick` hears of every chunk and stops all when the browser is gone.
fn full(path: &Path, tick: &mut impl FnMut(u64) -> io::Result<()>) -> io::Result<Option<u128>> {
    let Ok(mut f) = crate::confine::open(path) else { return Ok(None) };
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(None),
        };
        h.update(&buf[..n]);
        tick(n as u64)?;
    }
    Ok(Some(h.digest128()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run(dir: &Path, by_name: bool) -> (Vec<Vec<String>>, u64) {
        let mut sink = Vec::new();
        let mut report = Report { out: &mut sink, at: None };
        let (groups, scanned, skipped, more) = find(dir, by_name, 1, &mut report).unwrap();
        assert!(!more);
        assert_eq!(skipped, 0);
        (groups.iter().map(|g| g.items.iter().map(|i| i.path.clone()).collect()).collect(), scanned)
    }

    #[test]
    fn names_that_are_not_utf8_are_skipped_and_counted() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let odd = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
        fs::write(dir.path().join(odd), "same").unwrap();
        fs::write(dir.path().join("cafe.txt"), "same").unwrap();
        let mut sink = Vec::new();
        let mut report = Report { out: &mut sink, at: None };
        let (groups, scanned, skipped, _) = find(dir.path(), false, 1, &mut report).unwrap();
        assert!(groups.is_empty(), "a lossy path is never offered for deletion");
        assert_eq!((scanned, skipped), (1, 1));
    }

    #[test]
    fn finds_duplicates_by_content_not_links() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::create_dir_all(dir.join("#recycle")).unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let mut other = big.clone();
        other[150_000] ^= 1; // same size and edges, another middle
        fs::write(dir.join("a.bin"), &big).unwrap();
        fs::write(dir.join("sub/b.bin"), &big).unwrap();
        fs::write(dir.join("c.bin"), &other).unwrap();
        fs::write(dir.join("#recycle/old.bin"), &big).unwrap();
        fs::hard_link(dir.join("a.bin"), dir.join("link.bin")).unwrap();
        std::os::unix::fs::symlink(dir.join("a.bin"), dir.join("sym.bin")).unwrap();
        fs::write(dir.join("x.txt"), "same").unwrap();
        fs::write(dir.join("sub/x.txt"), "same").unwrap();
        fs::write(dir.join("sub/y.txt"), "diff").unwrap();
        let (groups, scanned) = run(dir, false);
        // a, b, c, link, x, sub/x, sub/y: the bin and the symbolic link are not looked at
        assert_eq!(scanned, 7);
        assert_eq!(groups.len(), 2, "{groups:?}");
        // the biggest waste first: the .bin pair (the hard link counted once)
        assert_eq!(groups[0].len(), 2);
        assert!(groups[0].contains(&"sub/b.bin".to_string()) && groups[0].iter().all(|p| p.ends_with(".bin")));
        assert_eq!(groups[1], ["sub/x.txt", "x.txt"]);
        // by name: same name and size, nothing read
        let (groups, _) = run(dir, true);
        assert_eq!(groups, [["sub/x.txt", "x.txt"]]);
    }

    #[test]
    fn streams_progress_then_the_groups() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), "1").unwrap();
        fs::write(dir.path().join("b"), "1").unwrap();
        let mut out = Vec::new();
        send(&mut out, dir.path(), "content", 1).unwrap();
        let text = String::from_utf8(out).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(head.contains("application/x-ndjson"));
        let last: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
        assert_eq!(last["groups"][0]["items"].as_array().unwrap().len(), 2);
        assert_eq!(send(&mut Vec::new(), dir.path(), "md5", 1).unwrap_err().status, 400);
    }
}
