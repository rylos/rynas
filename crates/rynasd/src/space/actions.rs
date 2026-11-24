//! The safe one-click actions and the review list of Space: what each would remove (dry
//! run, pure where it can be), and how it is done.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rynas_tasks::Cmd;

/// `journalctl --vacuum-size` frees what is above the cap.
pub fn journal_excess(usage: u64, cap: Option<u64>) -> u64 {
    cap.map_or(0, |c| usage.saturating_sub(c))
}

/// Catalogue serial folders that are neither the current nor the previous one (the
/// targets of the `current` and `previous` links); never a link itself.
pub fn stale_serials(dir: &Path) -> Vec<PathBuf> {
    let target = |l: &str| std::fs::read_link(dir.join(l)).ok().map(|t| if t.is_absolute() { t } else { dir.join(t) });
    let keep: BTreeSet<PathBuf> = ["current", "previous"].iter().filter_map(|l| target(l)).collect();
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| e.file_name().to_str().is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())))
        .map(|e| e.path())
        .filter(|p| !keep.contains(p))
        .collect();
    v.sort();
    v
}

/// One installed kernel image package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kernel {
    pub package: String,
    /// `uname -r` of the image (the package name without `linux-image-`).
    pub release: String,
    pub bytes: u64,
}

/// Installed versioned kernel images from `dpkg-query -W -f '${Package}\t${db:Status-Abbrev}\t${Installed-Size}\n' 'linux-image-*'`
/// (metapackages without a version left out).
pub fn parse_kernels(out: &str) -> Vec<Kernel> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (pkg, status, size) = (f.next()?, f.next()?, f.next().unwrap_or("0"));
            let release = pkg.strip_prefix("linux-image-")?;
            if !status.starts_with("ii") || !release.starts_with(|c: char| c.is_ascii_digit()) || release.ends_with("-dbg") {
                return None;
            }
            Some(Kernel { package: pkg.into(), release: release.into(), bytes: size.trim().parse::<u64>().unwrap_or(0) * 1024 })
        })
        .collect()
}

/// The series of a release: its `major.minor` and its flavour (`6.12.48+deb13-amd64` →
/// `6.12/amd64`), so each line of kernels keeps its newest.
pub fn series(release: &str) -> String {
    let flavour = release.rsplit('-').next().filter(|f| f.chars().any(|c| c.is_ascii_alphabetic())).unwrap_or("");
    let mm: Vec<&str> = release.split(['.', '-', '+']).take(2).collect();
    format!("{}/{flavour}", mm.join("."))
}

/// Compares versions by their numeric and text runs (`6.12.9` < `6.12.48`).
pub fn cmp_version(a: &str, b: &str) -> std::cmp::Ordering {
    fn runs(s: &str) -> Vec<(bool, String)> {
        let mut v: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let d = c.is_ascii_digit();
            match v.last_mut() {
                Some((kd, t)) if *kd == d => t.push(c),
                _ => v.push((d, c.to_string())),
            }
        }
        v
    }
    let (ra, rb) = (runs(a), runs(b));
    for (x, y) in ra.iter().zip(rb.iter()) {
        let o = match (x.0, y.0) {
            (true, true) => x.1.trim_start_matches('0').len().cmp(&y.1.trim_start_matches('0').len()).then_with(|| x.1.trim_start_matches('0').cmp(y.1.trim_start_matches('0'))),
            _ => x.1.cmp(&y.1),
        };
        if o != std::cmp::Ordering::Equal {
            return o;
        }
    }
    ra.len().cmp(&rb.len())
}

/// Kernels that may go: not the running one, not the newest of its series, not held.
pub fn old_kernels(installed: &[Kernel], running: &str, held: &BTreeSet<String>) -> Vec<Kernel> {
    let mut newest: std::collections::BTreeMap<String, &Kernel> = Default::default();
    for k in installed {
        let s = series(&k.release);
        match newest.get(&s) {
            Some(n) if cmp_version(&n.release, &k.release) != std::cmp::Ordering::Less => {}
            _ => {
                newest.insert(s, k);
            }
        }
    }
    let keep: BTreeSet<&str> = newest.values().map(|k| k.package.as_str()).collect();
    installed
        .iter()
        .filter(|k| k.release != running && !keep.contains(k.package.as_str()) && !held.contains(&k.package))
        .cloned()
        .collect()
}

/// Files of the thumbnail cache, least recently used first: `(path, bytes, used at s)`.
pub fn cache_files(root: &Path) -> Vec<(PathBuf, u64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let mut v = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else if m.is_file() {
                // the later of read and change: a thumbnail served is "used"
                v.push((e.path(), m.blocks() * 512, m.atime().max(m.mtime())));
            }
        }
    }
    v.sort_by_key(|x| x.2);
    v
}

/// What to evict from the cache to bring it under `cap` bytes: the oldest-used first.
pub fn evictions(files: &[(PathBuf, u64, i64)], cap: u64) -> Vec<PathBuf> {
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    let mut out = Vec::new();
    for (p, b, _) in files {
        if total <= cap {
            break;
        }
        out.push(p.clone());
        total = total.saturating_sub(*b);
    }
    out
}

/// Downloaded package files and apt's package caches (what `apt-get clean` removes).
pub fn apt_cache_bytes(root: &Path) -> (u64, u32) {
    use std::os::unix::fs::MetadataExt;
    let mut bytes = 0;
    let mut n = 0;
    for d in ["var/cache/apt/archives", "var/cache/apt/archives/partial"] {
        let Ok(rd) = std::fs::read_dir(root.join(d)) else { continue };
        for e in rd.flatten() {
            if e.file_name().to_str().is_some_and(|x| x.ends_with(".deb")) {
                bytes += e.metadata().map(|m| m.blocks() * 512).unwrap_or(0);
                n += 1;
            }
        }
    }
    for f in ["var/cache/apt/pkgcache.bin", "var/cache/apt/srcpkgcache.bin"] {
        if let Ok(m) = std::fs::metadata(root.join(f)) {
            bytes += m.blocks() * 512;
        }
    }
    (bytes, n)
}

/// The home folders of regular accounts (uid 1000–59999) from a passwd text.
pub fn homes_of(passwd: &str) -> Vec<String> {
    passwd
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            let uid: u32 = f.get(2)?.parse().ok()?;
            let home = f.get(5)?;
            ((1000..60000).contains(&uid) && home.starts_with('/') && *home != "/").then(|| home.to_string())
        })
        .collect()
}

/// A command run to completion (logged error text on failure).
pub fn run(c: Cmd) -> Result<String, String> {
    let out = c.output_blocking().map_err(|e| e.to_string())?;
    if out.success() { Ok(out.stdout) } else { Err(format!("{} {}", out.stderr.trim(), out.stdout.trim()).trim().to_string()) }
}

/// `btrfs subvolume sync <mount>`: waits until the filesystem has freed what deleted
/// subvolumes held (the cleaner works in the background).
pub fn subvolume_sync(mount: &Path) -> Result<(), String> {
    run(Cmd::new("btrfs").args(["subvolume", "sync"]).arg(mount).timeout(Duration::from_secs(3600))).map(|_| ())
}

/// Top-level subvolumes (parent 5) of a filesystem from `btrfs subvolume list <mount>`:
/// `(id, path)`.
pub fn top_level_subvolumes(list: &str) -> Vec<(u64, String)> {
    list.lines()
        .filter_map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            let id = w.get(1)?.parse().ok()?;
            let at = w.iter().position(|x| *x == "level")?;
            let level: u64 = w.get(at + 1)?.parse().ok()?;
            let p = w.iter().position(|x| *x == "path")?;
            let path = w[p + 1..].join(" ");
            (level == 5 && !path.contains('/')).then_some((id, path))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernels_keep_running_newest_and_held() {
        let out = "linux-image-amd64\tii \t1\n\
                   linux-image-6.12.9+deb13-amd64\tii \t400000\n\
                   linux-image-6.12.48+deb13-amd64\tii \t410000\n\
                   linux-image-6.12.30+deb13-amd64\tii \t405000\n\
                   linux-image-6.1.0-30-amd64\tii \t300000\n\
                   linux-image-6.1.0-28-amd64\trc \t0\n\
                   linux-image-6.17.1-rt-amd64\tii \t1000\n";
        let k = parse_kernels(out);
        assert_eq!(k.len(), 5, "metapackage and removed package left out");
        assert_eq!(series("6.12.48+deb13-amd64"), "6.12/amd64");
        assert_eq!(series("6.1.0-30-amd64"), "6.1/amd64");
        let held: BTreeSet<String> = ["linux-image-6.12.9+deb13-amd64".to_string()].into();
        let old = old_kernels(&k, "6.12.30+deb13-amd64", &held);
        // 6.12.48 newest of 6.12, 6.12.30 running, 6.12.9 held, 6.1 and 6.17 alone in their series
        assert!(old.is_empty(), "{old:?}");
        let old = old_kernels(&k, "6.12.48+deb13-amd64", &BTreeSet::new());
        let names: Vec<&str> = old.iter().map(|k| k.release.as_str()).collect();
        assert_eq!(names, ["6.12.9+deb13-amd64", "6.12.30+deb13-amd64"]);
        assert_eq!(old[0].bytes, 400000 * 1024);
    }

    #[test]
    fn versions_compare_by_numbers() {
        use std::cmp::Ordering::*;
        assert_eq!(cmp_version("6.12.9", "6.12.48"), Less);
        assert_eq!(cmp_version("6.12.48", "6.12.48"), Equal);
        assert_eq!(cmp_version("6.10", "6.9"), Greater);
    }

    #[test]
    fn evictions_take_the_least_recently_used_until_under_the_cap() {
        let f = vec![(PathBuf::from("a"), 10, 1), (PathBuf::from("b"), 10, 2), (PathBuf::from("c"), 10, 3)];
        assert_eq!(evictions(&f, 15), [PathBuf::from("a"), PathBuf::from("b")]);
        assert!(evictions(&f, 30).is_empty());
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("1000")).unwrap();
        std::fs::write(d.path().join("1000/x.jpg"), vec![0u8; 5000]).unwrap();
        assert_eq!(cache_files(d.path()).len(), 1);
    }

    #[test]
    fn serials_keep_current_and_previous() {
        let d = tempfile::tempdir().unwrap();
        for s in ["100", "101", "102", "103"] {
            std::fs::create_dir(d.path().join(s)).unwrap();
        }
        std::fs::create_dir(d.path().join("tmp")).unwrap();
        std::os::unix::fs::symlink("103", d.path().join("current")).unwrap();
        std::os::unix::fs::symlink(d.path().join("102"), d.path().join("previous")).unwrap();
        let v = stale_serials(d.path());
        assert_eq!(v, [d.path().join("100"), d.path().join("101")]);
    }

    #[test]
    fn small_parsers() {
        assert_eq!(journal_excess(3 << 30, Some(2 << 30)), 1 << 30);
        assert_eq!(journal_excess(1 << 30, Some(2 << 30)), 0);
        assert_eq!(journal_excess(1 << 30, None), 0);
        let pw = "root:x:0:0:root:/root:/bin/bash\nann:x:1000:1000::/home/ann:/bin/bash\nnobody:x:65534:65534::/nonexistent:/usr/sbin/nologin\nsvc:x:1001:1001::/:/bin/false\n";
        assert_eq!(homes_of(pw), ["/home/ann"]);
        let list = "ID 256 gen 900 top level 5 path @rootfs\nID 257 gen 10 top level 5 path @rootfs.rynas-rollback-20261101T101010\nID 300 gen 5 top level 256 path .snapshots/rootfs/rootfs.20261101T0100\n";
        assert_eq!(top_level_subvolumes(list), [(256, "@rootfs".to_string()), (257, "@rootfs.rynas-rollback-20261101T101010".to_string())]);
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("var/cache/apt/archives/partial")).unwrap();
        std::fs::write(d.path().join("var/cache/apt/archives/a.deb"), vec![1u8; 9000]).unwrap();
        std::fs::write(d.path().join("var/cache/apt/archives/lock"), b"").unwrap();
        assert_eq!(apt_cache_bytes(d.path()).1, 1);
    }
}
