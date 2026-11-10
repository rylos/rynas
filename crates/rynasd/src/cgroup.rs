//! rynasd and its file manager workers in cgroups of their own.
//!
//! `rynasd.service` delegates its cgroup (`Delegate=memory pids`, `ProtectControlGroups=private`).
//! At start rynasd splits it in two leaves: it moves itself into `daemon/` (its other children,
//! mdadm, smartctl, useradd…, start there too) and opens `workers/`, where every
//! `rynas-files-worker` moves itself between fork and exec
//! (`rynas_files::jobs::manager::set_worker_cgroup`): one write of "0" to `cgroup.procs`, no
//! D-Bus round trip (a `systemd-run --scope` per request costs tens of milliseconds, see
//! `docs/plugins/files.md`). Each leaf has its own `memory.high` and `memory.max`, so a big
//! job (a `.tar.zst`, a copy filling the page cache) is reclaimed and throttled in `workers/`
//! while rynasd keeps answering; the unit's own `MemoryHigh`/`MemoryMax` bound the sum.
//!
//! Limits: `RYNAS_CGROUP_DAEMON` and `RYNAS_CGROUP_WORKERS`, each `<memory.high>:<memory.max>`
//! (`96M:256M` and `192M:384M` by default, set in the unit). Anything missing — no delegation, a
//! read-only cgroup tree, not root, `--dry-system`, the tests — leaves everything in one cgroup,
//! as before: the file manager works the same, only without the separation.

use std::fs;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The cgroup tree (cgroup v2).
pub const CGROUP_FS: &str = "/sys/fs/cgroup";
/// The host's whole cgroup tree, read only. rynasd's cgroup namespace
/// (`ProtectControlGroups=private`) shows only its own subtree at [`CGROUP_FS`], so the unit
/// binds the host tree here (`BindReadOnlyPaths=`) for what reads other cgroups: the
/// containers' CPU and memory.
pub const HOST_CGROUP_FS: &str = "/run/rynas-host-cgroup";

/// Where the host's cgroups are read: the bound tree, or [`CGROUP_FS`] when the unit has no
/// private namespace (older systemd, a run by hand).
pub fn host_tree() -> &'static str {
    if Path::new(HOST_CGROUP_FS).join("cgroup.controllers").exists() { HOST_CGROUP_FS } else { CGROUP_FS }
}
/// rynasd's own leaf.
pub const DAEMON: &str = "daemon";
/// The leaf of the file manager workers.
pub const WORKERS: &str = "workers";

/// `memory.high` and `memory.max` of a leaf, as the kernel takes them ("96M", "max").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub high: String,
    pub max: String,
}

impl Limits {
    /// "96M:256M": a number with an optional K, M or G, or "max", for each.
    pub fn parse(text: &str) -> Option<Limits> {
        let (high, max) = text.trim().split_once(':')?;
        let ok = |v: &str| {
            v == "max" || {
                let digits = v.strip_suffix(['K', 'M', 'G']).unwrap_or(v);
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            }
        };
        (ok(high) && ok(max)).then(|| Limits { high: high.into(), max: max.into() })
    }

    fn from_env(name: &str, default: &str) -> Limits {
        std::env::var(name).ok().and_then(|v| Limits::parse(&v)).unwrap_or_else(|| Limits::parse(default).expect("a valid default"))
    }
}

/// The process's own cgroup below the tree, from `/proc/self/cgroup` ("0::/system.slice/…";
/// "0::/" inside a cgroup namespace).
pub fn own_cgroup(proc_self_cgroup: &str) -> Option<PathBuf> {
    let path = proc_self_cgroup.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(PathBuf::from(path.trim().trim_start_matches('/')))
}

fn write(path: &Path, value: &str) -> io::Result<()> {
    // one write(2) per value: cgroup files take them whole
    let mut f = fs::OpenOptions::new().write(true).open(path)?;
    f.write_all(value.as_bytes())
}

fn mkdir(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

/// Splits the cgroup `base` (rynasd's, delegated) into `daemon/`, holding every process that
/// is in it now, and `workers/`, with their limits; returns `workers/cgroup.procs`, opened for
/// the workers to move themselves into.
pub fn split(base: &Path, daemon: &Limits, workers: &Limits) -> io::Result<OwnedFd> {
    // started again inside the leaf (a re-exec): the split is there already
    let base = if base.ends_with(DAEMON) { base.parent().unwrap_or(base) } else { base };
    let controllers = fs::read_to_string(base.join("cgroup.controllers"))?;
    let have: Vec<&str> = controllers.split_whitespace().collect();
    if !have.contains(&"memory") {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "the cgroup is not delegated (no memory controller)"));
    }
    let (d, w) = (base.join(DAEMON), base.join(WORKERS));
    mkdir(&d)?;
    mkdir(&w)?;
    // the workers read their memory limit to size what they keep unwritten (the daemon's umask
    // is 077 and they do not run as root)
    fs::set_permissions(&w, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    // a cgroup with controllers for its children holds no process itself
    for pid in fs::read_to_string(base.join("cgroup.procs"))?.split_whitespace() {
        match write(&d.join("cgroup.procs"), pid) {
            // gone meanwhile
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
            other => other?,
        }
    }
    let enable: Vec<String> = ["memory", "pids"].iter().filter(|c| have.contains(c)).map(|c| format!("+{c}")).collect();
    write(&base.join("cgroup.subtree_control"), &enable.join(" "))?;
    for (leaf, limits) in [(&d, daemon), (&w, workers)] {
        write(&leaf.join("memory.high"), &limits.high)?;
        write(&leaf.join("memory.max"), &limits.max)?;
    }
    let procs = fs::OpenOptions::new().write(true).custom_flags(libc::O_CLOEXEC).open(w.join("cgroup.procs"))?;
    Ok(OwnedFd::from(procs))
}

/// [`split`] of this process's own cgroup, with the limits of the environment; the workers'
/// `cgroup.procs` on success.
pub fn split_own() -> io::Result<(PathBuf, OwnedFd)> {
    let own = own_cgroup(&fs::read_to_string("/proc/self/cgroup")?).ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "no cgroup v2"))?;
    let base = Path::new(CGROUP_FS).join(own);
    let daemon = Limits::from_env("RYNAS_CGROUP_DAEMON", "96M:256M");
    let workers = Limits::from_env("RYNAS_CGROUP_WORKERS", "192M:384M");
    let fd = split(&base, &daemon, &workers)?;
    Ok((base, fd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_paths() {
        assert_eq!(Limits::parse("96M:256M"), Some(Limits { high: "96M".into(), max: "256M".into() }));
        assert_eq!(Limits::parse(" max:1073741824 ").unwrap().max, "1073741824");
        for bad in ["96M", "96M:", ":1G", "96MB:1G", "-1:1G", "1G:1G\nx", "1T:2T"] {
            assert!(Limits::parse(bad).is_none(), "{bad}");
        }
        assert_eq!(own_cgroup("0::/system.slice/rynasd.service\n"), Some(PathBuf::from("system.slice/rynasd.service")));
        assert_eq!(own_cgroup("0::/\n"), Some(PathBuf::new()));
        assert_eq!(own_cgroup("1:name=systemd:/x\n"), None);
    }

    #[test]
    fn splits_a_delegated_tree() {
        // plain files standing for the cgroup tree
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("rynasd.service");
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("cgroup.controllers"), "cpu io memory pids\n").unwrap();
        fs::write(base.join("cgroup.procs"), "4242\n").unwrap();
        fs::write(base.join("cgroup.subtree_control"), "").unwrap();
        for leaf in [DAEMON, WORKERS] {
            fs::create_dir_all(base.join(leaf)).unwrap();
            for f in ["cgroup.procs", "memory.high", "memory.max"] {
                fs::write(base.join(leaf).join(f), "").unwrap();
            }
        }
        let daemon = Limits::parse("96M:256M").unwrap();
        let workers = Limits::parse("192M:384M").unwrap();
        let _fd = split(&base, &daemon, &workers).unwrap();
        let read = |p: &str| fs::read_to_string(base.join(p)).unwrap();
        assert_eq!(read("daemon/cgroup.procs"), "4242");
        assert_eq!(read("cgroup.subtree_control"), "+memory +pids");
        assert_eq!((read("daemon/memory.high"), read("daemon/memory.max")), ("96M".into(), "256M".into()));
        assert_eq!((read("workers/memory.high"), read("workers/memory.max")), ("192M".into(), "384M".into()));
        // started again from inside the leaf: the same tree
        assert!(split(&base.join(DAEMON), &daemon, &workers).is_ok());
        // not delegated: nothing done
        fs::write(base.join("cgroup.controllers"), "").unwrap();
        assert_eq!(split(&base, &daemon, &workers).unwrap_err().kind(), io::ErrorKind::Unsupported);
    }

    /// The cost of the move per worker, in a delegated cgroup:
    /// `systemd-run --user --scope -p Delegate=yes cargo test -p rynasd --lib cgroup::tests::spawn_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn spawn_cost() {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let (base, fd) = split_own().expect("a delegated cgroup");
        let raw = fd.as_raw_fd();
        let run = |moved: bool| {
            let start = std::time::Instant::now();
            for _ in 0..300 {
                let mut cmd = std::process::Command::new("/bin/true");
                if moved {
                    // SAFETY: one write(2) between fork and exec
                    unsafe {
                        cmd.pre_exec(move || {
                            libc::write(raw, b"0".as_ptr().cast(), 1);
                            Ok(())
                        });
                    }
                }
                assert!(cmd.status().unwrap().success());
            }
            start.elapsed().as_micros() as f64 / 300.0
        };
        let (plain, moved) = (run(false), run(true));
        let mut cmd = std::process::Command::new("/bin/sleep");
        cmd.arg("0.3");
        // SAFETY: one write(2) between fork and exec
        unsafe {
            cmd.pre_exec(move || {
                libc::write(raw, b"0".as_ptr().cast(), 1);
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let procs = fs::read_to_string(base.join(WORKERS).join("cgroup.procs")).unwrap();
        let _ = child.wait();
        assert!(procs.split_whitespace().any(|p| p == child.id().to_string()), "the child is in workers/: {procs}");
        println!("spawn + wait of /bin/true: {plain:.0} µs plain, {moved:.0} µs moved into workers/");
    }
}
