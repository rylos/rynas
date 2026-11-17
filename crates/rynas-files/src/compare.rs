//! Two folder trees side by side, for synchronizing them (route `compare`): what is only on
//! one side (a folder there is reported whole, not walked), and the files on both sides
//! that differ, by size or by date (two seconds of slack, as FAT and SMB keep). Read as the
//! user inside the request's jail ([`crate::confine::read_dir`]), links compared as
//! themselves and never followed. The answer streams JSON lines like the checksum's.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::wire::{self as cgi, ApiError};

/// Differences answered at most.
const MAX: usize = 20000;
const TICK: Duration = Duration::from_millis(500);

#[derive(Serialize, Debug, PartialEq)]
pub struct Diff {
    /// relative to both roots
    pub path: String,
    /// a: only on the left, b: only on the right, newer_a / newer_b: on both, one newer;
    /// size: on both, same date, another size; kind: a file on one side, a folder on the
    /// other; unreadable: a folder or an item that could not be read on a side (nothing is
    /// done about it, nor about its counterpart)
    pub what: &'static str,
    pub dir: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a: Option<Side>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b: Option<Side>,
}

#[derive(Serialize, Debug, PartialEq, Clone, Copy)]
pub struct Side {
    pub size: u64,
    pub mtime: i64,
}

struct Node {
    dir: bool,
    side: Side,
}

/// A folder's items by name, the file manager's own and the NAS's bookkeeping left out; an
/// item whose metadata cannot be read is there as `None` (unreadable: nothing is decided
/// about it nor its counterpart). A name that is not UTF-8 cannot be named back to the
/// interface: skipped and counted. An error when the folder itself cannot be read.
fn children(dir: &Path, skipped: &mut u64) -> io::Result<BTreeMap<String, Option<Node>>> {
    let mut out = BTreeMap::new();
    for item in crate::confine::read_dir(dir)? {
        let item = item?;
        let raw = item.file_name();
        let Some(name) = raw.to_str() else {
            *skipped += 1;
            continue;
        };
        if crate::fsops::is_system_name(name) || name == crate::shares::RECYCLE || name == "#snapshot" {
            continue;
        }
        // a symbolic link is compared as itself, never followed
        let node = item.metadata().ok().map(|meta| {
            let dir = meta.is_dir();
            Node { dir, side: Side { size: if dir { 0 } else { meta.len() }, mtime: meta.mtime() } }
        });
        out.insert(name.to_string(), node);
    }
    Ok(out)
}

/// Whether `path` is on a filesystem that keeps local times (FAT, exFAT, and FUSE ones such
/// as exfat-fuse): there a whole hour of difference is the daylight saving time or the time
/// zone of whoever wrote it, not a newer file.
fn local_time_fs(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    const MSDOS: i64 = 0x4d44;
    const EXFAT: i64 = 0x2011_bab0;
    const FUSE: i64 = 0x6573_5546;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    // SAFETY: zeroed out-parameter, valid C string
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    #[allow(clippy::unnecessary_cast)]
    let kind = st.f_type as i64;
    matches!(kind, MSDOS | EXFAT | FUSE)
}

/// How two files of both sides differ: `None` when alike (two seconds of slack; on a
/// local-time filesystem an hour ± 2 s too, as the same file — or undecided, `size`, when the
/// sizes differ).
fn verdict(l: Side, r: Side, local_time: bool) -> Option<&'static str> {
    let d = l.mtime - r.mtime;
    if local_time && (d.abs() - 3600).abs() <= 2 {
        return (l.size != r.size).then_some("size");
    }
    if d > 2 {
        Some("newer_a")
    } else if d < -2 {
        Some("newer_b")
    } else if l.size != r.size {
        Some("size")
    } else {
        None
    }
}

/// Answers `compare` for the folders `a` and `b` (already placed inside the user's shares).
pub fn send(out: &mut impl Write, a: &Path, b: &Path) -> Result<(), ApiError> {
    if !std::fs::metadata(a)?.is_dir() || !std::fs::metadata(b)?.is_dir() {
        return Err(ApiError::invalid("path", t!("two folders are compared")));
    }
    // both checked to lie in the jail before anything is said
    drop(crate::confine::read_dir(a)?);
    drop(crate::confine::read_dir(b)?);
    cgi::head(out, 200, &[("Content-Type", "application/x-ndjson"), ("X-Accel-Buffering", "no")])?;
    out.flush()?;
    let mut at: Option<Instant> = None;
    let mut tick = |n: u64| -> io::Result<()> {
        if at.is_some_and(|at| at.elapsed() < TICK) {
            return Ok(());
        }
        at = Some(Instant::now());
        writeln!(out, "{}", json!({ "folders": n }))?;
        out.flush()
    };
    let result = compare(a, b, &mut tick);
    let line = match result {
        // skipped: names that are not UTF-8, never acted on
        Ok((diffs, folders, skipped, more)) => json!({ "diffs": diffs, "folders": folders, "skipped": skipped, "more": more }),
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

/// The differences between the trees `a` and `b`, sorted by path. A folder that cannot be
/// read on either side is one `unreadable` difference, its counterpart left alone (never
/// "only on the other side"). Answers (differences, folders walked, names skipped, whether
/// it stopped at [`MAX`]); an error when a root itself cannot be read.
pub fn compare(a: &Path, b: &Path, tick: &mut impl FnMut(u64) -> io::Result<()>) -> io::Result<(Vec<Diff>, u64, u64, bool)> {
    let local_time = local_time_fs(a) || local_time_fs(b);
    let mut diffs = Vec::new();
    let mut folders = 0u64;
    let mut skipped = 0u64;
    let mut more = false;
    let mut stack = vec![PathBuf::new()];
    'walk: while let Some(rel) = stack.pop() {
        folders += 1;
        tick(folders)?;
        let (left, right) = match (children(&a.join(&rel), &mut skipped), children(&b.join(&rel), &mut skipped)) {
            (Ok(l), Ok(r)) => (l, r),
            (Err(e), _) | (_, Err(e)) if rel.as_os_str().is_empty() => return Err(e),
            _ => {
                if diffs.len() >= MAX {
                    more = true;
                    break 'walk;
                }
                diffs.push(Diff { path: rel.to_string_lossy().into_owned(), what: "unreadable", dir: true, a: None, b: None });
                continue;
            }
        };
        let names: BTreeSet<&String> = left.keys().chain(right.keys()).collect();
        for name in names {
            let path = rel.join(name).to_string_lossy().into_owned();
            let diff = match (left.get(name), right.get(name)) {
                // what cannot be read on one side decides nothing about either side
                (Some(None), _) | (_, Some(None)) => Some(Diff { path, what: "unreadable", dir: false, a: None, b: None }),
                (Some(Some(l)), None) => Some(Diff { path, what: "a", dir: l.dir, a: Some(l.side), b: None }),
                (None, Some(Some(r))) => Some(Diff { path, what: "b", dir: r.dir, a: None, b: Some(r.side) }),
                (Some(Some(l)), Some(Some(r))) if l.dir != r.dir => Some(Diff { path, what: "kind", dir: l.dir, a: Some(l.side), b: Some(r.side) }),
                (Some(Some(l)), Some(Some(_))) if l.dir => {
                    stack.push(rel.join(name));
                    None
                }
                (Some(Some(l)), Some(Some(r))) => verdict(l.side, r.side, local_time).map(|what| Diff { path, what, dir: false, a: Some(l.side), b: Some(r.side) }),
                (None, None) => None,
            };
            if let Some(d) = diff {
                if diffs.len() >= MAX {
                    more = true;
                    break 'walk;
                }
                diffs.push(d);
            }
        }
    }
    diffs.sort_by(|x, y| x.path.cmp(&y.path));
    Ok((diffs, folders, skipped, more))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn compares_two_trees() {
        let base = tempfile::tempdir().unwrap();
        let (a, b) = (base.path().join("a"), base.path().join("b"));
        for d in [&a, &b] {
            fs::create_dir_all(d.join("both")).unwrap();
            fs::create_dir_all(d.join("#recycle")).unwrap();
            fs::write(d.join("same.txt"), "x").unwrap();
        }
        fs::write(a.join("#recycle/gone.txt"), "x").unwrap();
        fs::create_dir_all(a.join("onlya/deep")).unwrap();
        fs::write(a.join("onlya/deep/f"), "x").unwrap();
        fs::write(a.join("both/new.txt"), "new").unwrap();
        fs::write(b.join("both/new.txt"), "old").unwrap();
        fs::write(a.join("both/size.txt"), "1").unwrap();
        fs::write(b.join("both/size.txt"), "22").unwrap();
        fs::write(b.join("onlyb.txt"), "b").unwrap();
        fs::write(a.join("kind"), "f").unwrap();
        fs::create_dir_all(b.join("kind")).unwrap();
        let set = |p: &Path, t: u64| {
            let f = fs::File::options().write(true).open(p).unwrap();
            f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(t)).unwrap();
        };
        // one second apart: the same date; new.txt newer on the left; size.txt same date
        set(&a.join("same.txt"), 1_000_000);
        set(&b.join("same.txt"), 1_000_001);
        set(&a.join("both/new.txt"), 2_000_000);
        set(&b.join("both/new.txt"), 1_000_000);
        set(&a.join("both/size.txt"), 1_000_000);
        set(&b.join("both/size.txt"), 1_000_002);
        let (diffs, _, skipped, more) = compare(&a, &b, &mut |_| Ok(())).unwrap();
        assert!(!more);
        assert_eq!(skipped, 0);
        let got: Vec<(&str, &str)> = diffs.iter().map(|d| (d.path.as_str(), d.what)).collect();
        assert_eq!(got, vec![("both/new.txt", "newer_a"), ("both/size.txt", "size"), ("kind", "kind"), ("onlya", "a"), ("onlyb.txt", "b")]);
        assert!(diffs.iter().find(|d| d.path == "onlya").unwrap().dir, "a folder on one side is reported whole");
    }

    #[test]
    fn an_unreadable_folder_decides_nothing() {
        use std::os::unix::fs::PermissionsExt;
        // root reads everything: nothing to see then
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let base = tempfile::tempdir().unwrap();
        let (a, b) = (base.path().join("a"), base.path().join("b"));
        for d in [&a, &b] {
            fs::create_dir_all(d.join("sub")).unwrap();
            fs::write(d.join("sub/f"), "x").unwrap();
        }
        fs::write(b.join("sub/only_b"), "b").unwrap();
        fs::set_permissions(a.join("sub"), fs::Permissions::from_mode(0o000)).unwrap();
        let (diffs, _, _, _) = compare(&a, &b, &mut |_| Ok(())).unwrap();
        fs::set_permissions(a.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
        let got: Vec<(&str, &str)> = diffs.iter().map(|d| (d.path.as_str(), d.what)).collect();
        // never "only_b" as only on the right (a mirror would delete it)
        assert_eq!(got, [("sub", "unreadable")]);
        // a root that cannot be read is an error, not "everything is only on the other side"
        assert!(compare(&a.join("missing"), &b, &mut |_| Ok(())).is_err());
    }

    #[test]
    fn local_times_an_hour_apart_are_alike() {
        let s = |size, mtime| Side { size, mtime };
        assert_eq!(verdict(s(1, 10_000), s(1, 10_000 - 3600), false), Some("newer_a"));
        assert_eq!(verdict(s(1, 10_000), s(1, 10_000 - 3601), true), None);
        assert_eq!(verdict(s(1, 10_000), s(2, 10_000 + 3599), true), Some("size"));
        assert_eq!(verdict(s(1, 10_000), s(1, 10_001), true), None);
        assert_eq!(verdict(s(1, 10_000), s(1, 9_000), true), Some("newer_a"));
    }

    #[test]
    fn names_that_are_not_utf8_are_skipped_and_counted() {
        use std::os::unix::ffi::OsStrExt;
        let base = tempfile::tempdir().unwrap();
        let (a, b) = (base.path().join("a"), base.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join(std::ffi::OsStr::from_bytes(b"caf\xe9")), "x").unwrap();
        let (diffs, _, skipped, _) = compare(&a, &b, &mut |_| Ok(())).unwrap();
        assert!(diffs.is_empty(), "a lossy path is never offered for deletion");
        assert_eq!(skipped, 1);
    }

    #[test]
    fn streams_progress_then_the_differences() {
        let base = tempfile::tempdir().unwrap();
        let (a, b) = (base.path().join("a"), base.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("x"), "x").unwrap();
        let mut out = Vec::new();
        send(&mut out, &a, &b).unwrap();
        let text = String::from_utf8(out).unwrap();
        let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last["diffs"][0]["path"], "x");
        assert_eq!(last["diffs"][0]["what"], "a");
        assert_eq!(send(&mut Vec::new(), &a, &a.join("x")).unwrap_err().status, 400);
    }
}
