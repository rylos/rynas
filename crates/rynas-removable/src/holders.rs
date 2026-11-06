//! Which processes keep a mounted filesystem busy, the `fuser -m` way through `/proc`: a
//! process holds the filesystem when its working directory, root, executable, an open file
//! or a mapped file lives on it (the device number of the target, `stat` through the
//! `/proc/<pid>/…` links, so no path has to be resolved and no file is opened).

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rynas_api_types::removable::VolumeHolder;

/// At most this many holders are reported.
pub const MAX_HOLDERS: usize = 50;

/// The device number (`st_dev`) of the filesystem mounted at `mount_point`.
pub fn mount_dev(mount_point: &Path) -> Option<u64> {
    std::fs::metadata(mount_point).ok().map(|m| m.dev())
}

fn on_dev(path: &Path, dev: u64) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.dev() == dev)
}

/// `major:minor` of a `/proc/<pid>/maps` line (5th field).
fn maps_dev(line: &str) -> Option<u64> {
    let mm = line.split_whitespace().nth(3)?;
    let (ma, mi) = mm.split_once(':')?;
    let (ma, mi) = (
        u64::from_str_radix(ma, 16).ok()?,
        u64::from_str_radix(mi, 16).ok()?,
    );
    Some(libc::makedev(ma as u32, mi as u32))
}

fn user_names(proc_root: &Path) -> HashMap<u32, String> {
    let passwd = proc_root.parent().map(|r| r.join("etc/passwd"));
    passwd
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            Some((f.get(2)?.parse().ok()?, f.first()?.to_string()))
        })
        .collect()
}

/// Processes holding the filesystem `dev` (from [`mount_dev`]), read from `proc_root`
/// (`/proc`). Our own process is left out (it only looks).
pub fn holders(proc_root: &Path, dev: u64) -> Vec<VolumeHolder> {
    let me = std::process::id();
    let names = user_names(proc_root);
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir(proc_root) else {
        return out;
    };
    let mut pids: Vec<u32> = dir
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .filter(|p| *p != me)
        .collect();
    pids.sort_unstable();
    for pid in pids {
        let base = proc_root.join(pid.to_string());
        let held = ["cwd", "root", "exe"]
            .iter()
            .any(|l| on_dev(&base.join(l), dev))
            || std::fs::read_dir(base.join("fd"))
                .map(|d| d.flatten().any(|e| on_dev(&e.path(), dev)))
                .unwrap_or(false)
            || std::fs::read_to_string(base.join("maps"))
                .map(|m| m.lines().any(|l| maps_dev(l) == Some(dev)))
                .unwrap_or(false);
        if !held {
            continue;
        }
        let command = std::fs::read_to_string(base.join("comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let uid = std::fs::read_to_string(base.join("status"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("Uid:"))
                    .and_then(|v| v.split_whitespace().next()?.parse::<u32>().ok())
            });
        let user = uid
            .map(|u| names.get(&u).cloned().unwrap_or_else(|| u.to_string()))
            .unwrap_or_default();
        out.push(VolumeHolder { pid, command, user });
        if out.len() >= MAX_HOLDERS {
            break;
        }
    }
    out
}

/// "smbd (pid 812, alice), bash (pid 4410, root)".
pub fn describe(h: &[VolumeHolder]) -> String {
    h.iter()
        .map(|h| {
            if h.user.is_empty() {
                format!("{} (pid {})", h.command, h.pid)
            } else {
                format!("{} (pid {}, {})", h.command, h.pid, h.user)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn finds_processes_by_device() {
        // the "filesystem" is the temp directory's: every file below it has its st_dev
        let data = tempfile::tempdir().unwrap();
        let file = data.path().join("open.txt");
        std::fs::write(&file, "x").unwrap();
        let root = tempfile::tempdir().unwrap();
        let proc_root = root.path().join("proc");
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        std::fs::write(
            root.path().join("etc/passwd"),
            "alice:x:1000:100::/home/alice:/bin/bash\n",
        )
        .unwrap();
        // pid 10: an open file on it; pid 11: nothing; pid 12: working directory on it
        for (pid, comm) in [(10, "smbd"), (11, "sshd"), (12, "bash")] {
            let b = proc_root.join(pid.to_string());
            std::fs::create_dir_all(b.join("fd")).unwrap();
            std::fs::write(b.join("comm"), format!("{comm}\n")).unwrap();
            std::fs::write(b.join("status"), "Name:\tx\nUid:\t1000\t1000\t1000\t1000\n").unwrap();
        }
        symlink(&file, proc_root.join("10/fd/3")).unwrap();
        symlink("/proc", proc_root.join("11/cwd")).unwrap();
        symlink(data.path(), proc_root.join("12/cwd")).unwrap();
        let dev = mount_dev(data.path()).unwrap();
        let proc_dev = mount_dev(Path::new("/proc")).unwrap();
        // only meaningful when the temp directory is not on procfs (always)
        assert_ne!(dev, proc_dev);
        let h = holders(&proc_root, dev);
        let got: Vec<(u32, &str, &str)> = h
            .iter()
            .map(|h| (h.pid, h.command.as_str(), h.user.as_str()))
            .collect();
        assert_eq!(got, [(10, "smbd", "alice"), (12, "bash", "alice")]);
        assert_eq!(describe(&h), "smbd (pid 10, alice), bash (pid 12, alice)");
    }

    #[test]
    fn maps_lines() {
        let l = "7f00-7f01 r-xp 00000000 08:21 1234 /media/S/lib.so";
        assert_eq!(maps_dev(l), Some(libc::makedev(8, 0x21)));
        assert_eq!(maps_dev("garbage"), None);
    }
}
