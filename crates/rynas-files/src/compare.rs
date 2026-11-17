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
    /// other
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

/// A folder's items by name, the file manager's own and the NAS's bookkeeping left out.
fn children(dir: &Path) -> BTreeMap<String, Node> {
    let mut out = BTreeMap::new();
    let Ok(rd) = crate::confine::read_dir(dir) else { return out };
    for item in rd.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if crate::fsops::is_system_name(&name) || name == crate::shares::RECYCLE || name == "#snapshot" {
            continue;
        }
        // a symbolic link is compared as itself, never followed
        let Ok(meta) = item.metadata() else { continue };
        let dir = meta.is_dir();
        out.insert(name, Node { dir, side: Side { size: if dir { 0 } else { meta.len() }, mtime: meta.mtime() } });
    }
    out
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
        Ok((diffs, folders, more)) => json!({ "diffs": diffs, "folders": folders, "more": more }),
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

/// The differences between the trees `a` and `b`, sorted by path; the number of folders
/// walked; whether it stopped at [`MAX`].
pub fn compare(a: &Path, b: &Path, tick: &mut impl FnMut(u64) -> io::Result<()>) -> io::Result<(Vec<Diff>, u64, bool)> {
    let mut diffs = Vec::new();
    let mut folders = 0u64;
    let mut more = false;
    let mut stack = vec![PathBuf::new()];
    'walk: while let Some(rel) = stack.pop() {
        folders += 1;
        tick(folders)?;
        let left = children(&a.join(&rel));
        let right = children(&b.join(&rel));
        let names: BTreeSet<&String> = left.keys().chain(right.keys()).collect();
        for name in names {
            let path = rel.join(name).to_string_lossy().into_owned();
            let diff = match (left.get(name), right.get(name)) {
                (Some(l), None) => Some(Diff { path, what: "a", dir: l.dir, a: Some(l.side), b: None }),
                (None, Some(r)) => Some(Diff { path, what: "b", dir: r.dir, a: None, b: Some(r.side) }),
                (Some(l), Some(r)) if l.dir != r.dir => Some(Diff { path, what: "kind", dir: l.dir, a: Some(l.side), b: Some(r.side) }),
                (Some(l), Some(_)) if l.dir => {
                    stack.push(rel.join(name));
                    None
                }
                (Some(l), Some(r)) => {
                    let what = if l.side.mtime > r.side.mtime + 2 {
                        Some("newer_a")
                    } else if r.side.mtime > l.side.mtime + 2 {
                        Some("newer_b")
                    } else if l.side.size != r.side.size {
                        Some("size")
                    } else {
                        None
                    };
                    what.map(|what| Diff { path, what, dir: false, a: Some(l.side), b: Some(r.side) })
                }
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
    Ok((diffs, folders, more))
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
        let (diffs, _, more) = compare(&a, &b, &mut |_| Ok(())).unwrap();
        assert!(!more);
        let got: Vec<(&str, &str)> = diffs.iter().map(|d| (d.path.as_str(), d.what)).collect();
        assert_eq!(got, vec![("both/new.txt", "newer_a"), ("both/size.txt", "size"), ("kind", "kind"), ("onlya", "a"), ("onlyb.txt", "b")]);
        assert!(diffs.iter().find(|d| d.path == "onlya").unwrap().dir, "a folder on one side is reported whole");
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
