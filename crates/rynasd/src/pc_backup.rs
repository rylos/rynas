//! PC backup, receive side (D-PCBACKUP phase 2, `docs/research/pc-backup.md` §10): folders
//! where Linux PCs push btrbk snapshots over SSH. Administrators only.
//!
//! - `GET /v1/pc-backups/setup`: account, filter, host name, sshd port, btrfs volumes and
//!   what stops PCs from sending (no sshd, `AllowUsers`/`AllowGroups`, no sudo or btrfs, no
//!   filter).
//! - `GET /v1/pc-backups/targets`: every device with its freshness and snapshot counts;
//!   `POST` (202, task `pcbackup.create`): a new subvolume folder, or `adopt` an existing folder
//!   of received snapshots (nothing inside is touched; only the folder's owner and mode).
//! - `POST /v1/pc-backups/plan/adopt`: what a folder holds (read only).
//! - `GET|PUT|DELETE /v1/pc-backups/targets/{device}`: detail with every snapshot; settings
//!   (key, alert, delete rules: task `pcbackup.update`); remove (task `pcbackup.remove`: the
//!   key goes, the folder and its snapshots stay).
//! - `POST /v1/pc-backups/targets/{device}/usage` (task `pcbackup.usage`, `btrfs filesystem du`).
//! - `GET /v1/pc-backups/targets/{device}/browse?snapshot=&path=`: a folder inside a received
//!   snapshot (descriptor walk, symlinks shown never followed);
//!   `POST …/copy-out` (task `pcbackup.copy_out`): a file or folder copied into a NAS folder.
//!
//! The system side is behind [`ReceiveHost`]: the machine ([`HostReceive`]: `useradd`, the
//! account's `authorized_keys`, `/etc/sudoers.d/rynas-pcbackup` checked by `visudo -c`,
//! `btrfs subvolume create`, ioctls) or memory ([`MemoryReceive`], `--dry-system` and tests).
//! Each key line is `restrict,command="rynas-btrbk-filter --device … --root … [rules]"`: the
//! filter ([`rynas_btrfs::receive_filter`]) is the only thing a PC can run. This module never
//! edits sshd's configuration: the SSH service ([`crate::ssh`]) owns it, and when only listed
//! accounts may log in its `AllowGroups` always has the account's group
//! ([`rynas_api_types::ssh::SSH_BASE_GROUPS`]), so the `AllowUsers`/`AllowGroups` check below
//! passes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
use rynas_api_types::pcbackup::*;
use rynas_api_types::snapshots::{Retention, SnapshotEntry, SnapshotListing};
use rynas_api_types::{TaskStarted, error_codes};
use rynas_btrfs::receive::{self, Entry};
use rynas_notify::Notifier;
use rynas_notify::event::{code, field, firing, resolved};
use rynas_tasks::Cmd;
use serde::{Deserialize, Serialize};

use crate::ops::{Error, Ops, Res, user};
use crate::roles::AdminPolicy;

const DAY_MS: u64 = 86_400_000;
const SUDOERS: &str = "/etc/sudoers.d/rynas-pcbackup";
/// The account's home: root-owned, so the account cannot change its own keys.
const HOME: &str = "/var/lib/rynas-pcbackup";

// ------------------------------------------------------------------ the system side

/// Facts about this machine for the setup card.
#[derive(Debug, Clone, Default)]
pub struct HostFacts {
    pub sshd: bool,
    /// `sshd -T` output (or sshd_config) when readable.
    pub sshd_config: String,
    pub sudo: bool,
    /// Path of btrfs, when installed.
    pub btrfs: Option<String>,
    pub filter: bool,
    /// btrfs mount points that can hold folders.
    pub volumes: Vec<String>,
}

/// What a folder holds: `None` when it does not exist.
pub type Scan = Option<(bool, Vec<Entry>, u32)>;

pub trait ReceiveHost: Send + Sync {
    fn facts(&self) -> HostFacts;
    /// The account, its home and the sudoers file with the lines of these device folders;
    /// idempotent. Returns log lines.
    fn ensure_account(&self, btrfs: &str, roots: &[String]) -> Result<Vec<String>, String>;
    /// The last device went: the sudoers file, the keys, the home and the account go too.
    fn remove_account(&self) -> Result<Vec<String>, String>;
    /// A removed device's folder: back to `root:root` when its group is the account's (the
    /// snapshots stay; no stale gid a later group could inherit).
    fn release_folder(&self, path: &Path) -> Result<(), String>;
    /// (uid, gid, mode) of the folder itself, not following a symlink.
    fn folder_meta(&self, path: &Path) -> Option<(u32, u32, u32)>;
    /// The account group's gid, when it exists.
    fn account_gid(&self) -> Option<u32>;
    /// User and group names of ids, when known.
    fn names(&self, uid: u32, gid: u32) -> (Option<String>, Option<String>);
    /// The home folders of the users of this machine.
    fn home_dirs(&self) -> Vec<String>;
    fn write_keys(&self, content: &str) -> Result<(), String>;
    /// A new subvolume folder `0750 root:<account>` (parents made as needed).
    fn create_folder(&self, path: &Path) -> Result<(), String>;
    /// An existing folder: only its own owner group and mode change.
    fn prepare_adopted(&self, path: &Path) -> Result<(), String>;
    /// (is a subvolume, snapshot entries, other entries).
    fn scan(&self, path: &Path) -> Result<Scan, String>;
    fn usage(&self, path: &Path) -> Result<(u64, u64, u64), String>;
    fn browse(&self, snapshot: &Path, rel: &str) -> Result<(Vec<SnapshotEntry>, bool), Error>;
    fn copy_out(
        &self,
        snapshot: &Path,
        rel: &str,
        dest: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<String, String>;
    /// Free and total bytes of the filesystem holding `path`.
    fn space(&self, path: &Path) -> Option<(u64, u64)>;
    /// (referenced, exclusive) bytes of each snapshot by name, live from the volume's quota
    /// groups; `None` when quotas are off, simple quotas, or marked inconsistent.
    fn quota_sizes(&self, root: &Path, entries: &[Entry]) -> Option<BTreeMap<String, (u64, u64)>>;
    /// (referenced, exclusive) bytes of one snapshot, measured at idle priority (slow: it
    /// reads every file's extents).
    fn measure(&self, snapshot: &Path) -> Result<(u64, u64), String>;
    /// `/.rynas/manifest.json` inside snapshot `name` of `root`: `Ok(None)` when it is not
    /// there; refused when bigger than [`MANIFEST_MAX`] or not a regular file.
    fn read_manifest(&self, root: &Path, name: &str) -> Result<Option<Vec<u8>>, String>;
    /// A receive holds the folder's lock now (the filter's one-receive-per-device lock).
    fn receiving(&self, root: &Path) -> bool;
    /// Takes the folder's lock (no receive can start meanwhile) after checking the folder
    /// from `/` without symlinks; `Ok(None)` when a receive holds it.
    fn lock(&self, root: &Path) -> Result<Option<FolderLock>, String>;
    /// Deletes snapshot `name` directly inside the locked folder.
    fn delete_snapshot(&self, lock: &FolderLock, name: &str) -> Result<(), String>;
}

/// Largest manifest read from a snapshot.
pub const MANIFEST_MAX: u64 = 1 << 20;

/// The device folder, opened and locked; the lock goes with it.
pub struct FolderLock {
    pub path: PathBuf,
    /// `None` in memory.
    pub dir: Option<std::fs::File>,
}

/// The machine.
pub struct HostReceive;

fn cmd_ok(c: Cmd) -> Result<String, String> {
    let shown = c.display();
    let o = c.run_blocking().map_err(|e| format!("{shown}: {e}"))?;
    if o.success() {
        Ok(o.stdout)
    } else {
        Err(format!("{shown}: {}", o.stderr.trim()))
    }
}

fn btrfs_mounts(mountinfo: &str) -> Vec<String> {
    let mut v: Vec<String> = mountinfo
        .lines()
        .filter_map(|l| {
            let (pre, post) = l.split_once(" - ")?;
            let fstype = post.split_whitespace().next()?;
            let mp = pre.split_whitespace().nth(4)?.replace("\\040", " ");
            (fstype == "btrfs"
                && mp != "/"
                && !["/boot", "/var", "/usr", "/home", "/root", "/srv/.snapshots", "/.snapshots"]
                    .iter()
                    .any(|s| mp == *s || mp.starts_with(&format!("{s}/")))
                && receive::valid_path(&format!("{mp}/x")).is_ok())
            .then_some(mp)
        })
        .collect();
    v.sort();
    v.dedup();
    // a btrfs root: the system volume's folder too, last (D-ROOTVOL)
    if crate::places::root_fstype(mountinfo).as_deref() == Some("btrfs") {
        v.push(rynas_api_types::storage::SYSTEM_VOLUME_DIR.into());
    }
    v
}

impl ReceiveHost for HostReceive {
    fn facts(&self) -> HostFacts {
        // installed and running (the SSH service page can turn it off)
        let sshd_bin = rynas_btrfs::receive_filter::locate("sshd").is_some()
            && Cmd::new("systemctl")
                .args(["is-active", "--quiet", rynas_services::ssh::SERVICE, rynas_services::ssh::SOCKET])
                .timeout(Duration::from_secs(5))
                .run_blocking()
                .is_ok_and(|o| o.success());
        let sshd_config = Cmd::new("sshd")
            .arg("-T")
            .env_clear()
            .timeout(Duration::from_secs(5))
            .run_blocking()
            .ok()
            .filter(|o| o.success())
            .map(|o| o.stdout)
            .or_else(|| std::fs::read_to_string("/etc/ssh/sshd_config").ok())
            .unwrap_or_default();
        HostFacts {
            sshd: sshd_bin,
            sshd_config,
            sudo: rynas_btrfs::receive_filter::locate("sudo").is_some(),
            btrfs: rynas_btrfs::receive_filter::locate("btrfs").map(|p| p.display().to_string()),
            filter: Path::new(FILTER).is_file(),
            volumes: btrfs_mounts(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default()),
        }
    }

    fn ensure_account(&self, btrfs: &str, roots: &[String]) -> Result<Vec<String>, String> {
        let mut log = Vec::new();
        let exists = Cmd::new("getent")
            .args(["passwd", ACCOUNT])
            .run_blocking()
            .is_ok_and(|o| o.success());
        if !exists {
            // a shell is needed: sshd runs the forced command through it
            cmd_ok(Cmd::new("useradd").args([
                "--system", "--user-group", "--home-dir", HOME, "--no-create-home",
                "--shell", "/bin/sh", "--comment", "rynas PC backup (btrbk receive)", ACCOUNT,
            ]))?;
            // no password login ever
            cmd_ok(Cmd::new("usermod").args(["--lock", ACCOUNT]))?;
            log.push(format!("account {ACCOUNT} created"));
        }
        let ssh = Path::new(HOME).join(".ssh");
        std::fs::create_dir_all(&ssh).map_err(|e| format!("{}: {e}", ssh.display()))?;
        for d in [Path::new(HOME), ssh.as_path()] {
            set_mode(d, 0o755)?;
            std::os::unix::fs::chown(d, Some(0), Some(0)).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        let roots: Vec<&str> = roots.iter().map(String::as_str).collect();
        let text = receive::sudoers(btrfs, &roots);
        let tmp = format!("{SUDOERS}.rynas-new");
        write_file(Path::new(&tmp), &text, 0o440)?;
        if let Err(e) = cmd_ok(Cmd::new("visudo").args(["-c", "-q", "-f", &tmp])) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("sudoers check failed: {e}"));
        }
        std::fs::rename(&tmp, SUDOERS).map_err(|e| format!("{SUDOERS}: {e}"))?;
        log.push(format!("{SUDOERS} written (btrfs at {btrfs}, {} folders)", roots.len()));
        Ok(log)
    }

    fn remove_account(&self) -> Result<Vec<String>, String> {
        let mut log = Vec::new();
        for f in [SUDOERS.to_string(), format!("{SUDOERS}.rynas-new")] {
            match std::fs::remove_file(&f) {
                Ok(()) => log.push(format!("{f} removed")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{f}: {e}")),
            }
        }
        if Path::new(HOME).exists() {
            std::fs::remove_dir_all(HOME).map_err(|e| format!("{HOME}: {e}"))?;
            log.push(format!("{HOME} removed (keys)"));
        }
        let exists = Cmd::new("getent")
            .args(["passwd", ACCOUNT])
            .run_blocking()
            .is_ok_and(|o| o.success());
        if exists {
            cmd_ok(Cmd::new("userdel").arg(ACCOUNT))?;
            log.push(format!("account {ACCOUNT} removed"));
        }
        if Cmd::new("getent").args(["group", ACCOUNT]).run_blocking().is_ok_and(|o| o.success()) {
            let _ = cmd_ok(Cmd::new("groupdel").arg(ACCOUNT));
        }
        Ok(log)
    }

    fn release_folder(&self, path: &Path) -> Result<(), String> {
        let (Some(gid), Some((_, fgid, _))) = (self.account_gid(), self.folder_meta(path)) else {
            return Ok(());
        };
        if fgid != gid {
            return Ok(());
        }
        let dir = rynas_btrfs::ioctl::open_dir(path).map_err(|e| format!("{}: {e}", path.display()))?;
        std::os::unix::fs::fchown(&dir, Some(0), Some(0)).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn folder_meta(&self, path: &Path) -> Option<(u32, u32, u32)> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path).ok().filter(|m| m.is_dir())?;
        Some((m.uid(), m.gid(), m.mode() & 0o7777))
    }

    fn account_gid(&self) -> Option<u32> {
        Cmd::new("getent")
            .args(["group", ACCOUNT])
            .run_blocking()
            .ok()
            .filter(|o| o.success())
            .and_then(|o| o.stdout.split(':').nth(2).and_then(|g| g.trim().parse::<u32>().ok()))
    }

    fn names(&self, uid: u32, gid: u32) -> (Option<String>, Option<String>) {
        let name = |db: &str, id: u32| {
            Cmd::new("getent")
                .args([db, &id.to_string()])
                .run_blocking()
                .ok()
                .filter(|o| o.success())
                .and_then(|o| o.stdout.split(':').next().map(|n| n.trim().to_string()))
                .filter(|n| !n.is_empty())
        };
        (name("passwd", uid), name("group", gid))
    }

    fn home_dirs(&self) -> Vec<String> {
        std::fs::read_to_string("/etc/passwd")
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split(':').nth(5).map(str::to_string))
            .filter(|h| h.len() > 1)
            .collect()
    }

    fn write_keys(&self, content: &str) -> Result<(), String> {
        write_file(&Path::new(HOME).join(".ssh/authorized_keys"), content, 0o644)
    }

    fn create_folder(&self, path: &Path) -> Result<(), String> {
        // in the system volume: its folder made ready and checked first (D-ROOTVOL)
        if crate::places::in_system_dir(&path.to_string_lossy()) {
            crate::places::ensure_system_dir(Path::new("/"), true, &|m| {
                tracing::info!("system volume: {m}")
            })?;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        cmd_ok(Cmd::new("btrfs").args(["subvolume", "create"]).arg(path))?;
        self.prepare_adopted(path)
    }

    fn prepare_adopted(&self, path: &Path) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        let gid = self.account_gid().ok_or_else(|| format!("group {ACCOUNT} not found"))?;
        // on the descriptor: a symlink swapped in for the folder is not followed
        let dir = rynas_btrfs::ioctl::open_dir(path).map_err(|e| format!("{}: {e}", path.display()))?;
        std::os::unix::fs::fchown(&dir, Some(0), Some(gid)).map_err(|e| format!("{}: {e}", path.display()))?;
        dir.set_permissions(std::fs::Permissions::from_mode(0o750))
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    fn scan(&self, path: &Path) -> Result<Scan, String> {
        let dir = match rynas_btrfs::ioctl::open_dir(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        if !rynas_btrfs::ioctl::is_btrfs(&dir) {
            return Err(format!("{}: not on a btrfs volume", path.display()));
        }
        let sub = rynas_btrfs::ioctl::is_subvolume(&dir);
        let (entries, other) = receive::scan_dir(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Some((sub, entries, other)))
    }

    fn usage(&self, path: &Path) -> Result<(u64, u64, u64), String> {
        let out = cmd_ok(
            Cmd::new("btrfs")
                .args(["filesystem", "du", "-s", "--raw"])
                .arg(path)
                .timeout(Duration::from_secs(3600)),
        )?;
        receive::parse_du(&out).ok_or_else(|| "btrfs filesystem du: unexpected output".into())
    }

    fn browse(&self, snapshot: &Path, rel: &str) -> Result<(Vec<SnapshotEntry>, bool), Error> {
        let item = rynas_btrfs::browse::resolve(snapshot, rel)?;
        Ok(rynas_btrfs::browse::list(&item)?)
    }

    fn copy_out(
        &self,
        snapshot: &Path,
        rel: &str,
        dest: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        let now = (rynas_tasks::now_ms() / 1000) as i64;
        let r = rynas_btrfs::restore::restore_foreign(snapshot, dest, rel, now, cancelled)
            .map_err(|e| e.to_string())?;
        Ok(format!(
            "copied to {}/{}: {} files, {} folders, {} bytes",
            dest.display(),
            r.restored_to,
            r.files,
            r.dirs,
            r.bytes
        ))
    }

    fn space(&self, path: &Path) -> Option<(u64, u64)> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: statvfs(3) fills the struct; the path is a valid C string
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        let f = s.f_frsize as u64;
        Some((s.f_bavail as u64 * f, s.f_blocks as u64 * f))
    }

    fn quota_sizes(&self, root: &Path, entries: &[Entry]) -> Option<BTreeMap<String, (u64, u64)>> {
        // the quota tree shows up in sysfs while quotas are on: two small reads per snapshot,
        // no walk and no btrfs command
        let dir = rynas_btrfs::ioctl::open_dir(root).ok()?;
        let base = Path::new("/sys/fs/btrfs").join(rynas_btrfs::ioctl::fs_uuid(&dir).ok()?).join("qgroups");
        let read = |p: PathBuf| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string());
        // simple quotas charge extents to their first owner: not what deleting frees
        if !base.is_dir() || read(base.join("mode")).as_deref() == Some("squota") {
            return None;
        }
        if read(base.join("inconsistent")).as_deref() == Some("1") {
            return None;
        }
        let mut out = BTreeMap::new();
        for e in entries {
            let Some(info) = e.info.as_ref().filter(|i| i.root_id > 0) else { continue };
            let q = base.join(format!("0_{}", info.root_id));
            let num = |f: &str| read(q.join(f)).and_then(|s| s.parse::<u64>().ok());
            if let (Some(r), Some(x)) = (num("referenced"), num("exclusive")) {
                out.insert(e.name.clone(), (r, x));
            }
        }
        Some(out)
    }

    fn measure(&self, snapshot: &Path) -> Result<(u64, u64), String> {
        let btrfs = rynas_btrfs::receive_filter::locate("btrfs").ok_or("btrfs-progs is not installed")?;
        // idle I/O class and lowest CPU priority: a measure never slows the shares down
        let du = |c: Cmd| c.args(["filesystem", "du", "-s", "--raw"]).arg(snapshot).timeout(Duration::from_secs(3600));
        let out = match rynas_btrfs::receive_filter::locate("ionice") {
            Some(ionice) => cmd_ok(du(Cmd::new(ionice).args(["-c", "3", "nice", "-n", "19"]).arg(&btrfs)))?,
            None => cmd_ok(du(Cmd::new(&btrfs)))?,
        };
        receive::parse_du(&out)
            .map(|(total, excl, _)| (total, excl))
            .ok_or_else(|| "btrfs filesystem du: unexpected output".into())
    }

    fn read_manifest(&self, root: &Path, name: &str) -> Result<Option<Vec<u8>>, String> {
        use std::io::Read;
        use std::os::fd::{AsRawFd, FromRawFd};
        use rynas_btrfs::ioctl::{open_dir, open_dir_at};
        let gone = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
        let snap = open_dir_at(&open_dir(root).map_err(|e| e.to_string())?, name).map_err(|e| format!("{name}: {e}"))?;
        let dot = match open_dir_at(&snap, ".rynas") {
            Ok(d) => d,
            Err(e) if gone(&e) => return Ok(None),
            Err(e) => return Err(format!("{name}/.rynas: {e}")),
        };
        // SAFETY: openat(2) on a directory descriptor with a constant C name; never follows a
        // symlink, never blocks on a FIFO
        let fd = unsafe {
            libc::openat(
                dot.as_raw_fd(),
                c"manifest.json".as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY,
            )
        };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return if gone(&e) { Ok(None) } else { Err(format!("{name}/.rynas/manifest.json: {e}")) };
        }
        // SAFETY: a descriptor we just opened and own
        let f = unsafe { std::fs::File::from_raw_fd(fd) };
        let m = f.metadata().map_err(|e| e.to_string())?;
        if !m.is_file() || m.len() > MANIFEST_MAX {
            return Err(format!("{name}/.rynas/manifest.json: not a regular file of at most 1 MiB"));
        }
        let mut buf = Vec::with_capacity(m.len() as usize);
        f.take(MANIFEST_MAX + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
        if buf.len() as u64 > MANIFEST_MAX {
            return Err(format!("{name}/.rynas/manifest.json: bigger than 1 MiB"));
        }
        Ok(Some(buf))
    }

    fn receiving(&self, root: &Path) -> bool {
        use std::os::fd::AsRawFd;
        let Ok(dir) = rynas_btrfs::ioctl::open_dir(root) else { return false };
        // SAFETY: flock(2) on our own descriptor; a shared lock is refused while the filter
        // holds its exclusive one, and goes when `dir` closes
        unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) != 0 }
    }

    fn lock(&self, root: &Path) -> Result<Option<FolderLock>, String> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let dir = rynas_btrfs::receive_filter::verify_root(&root.to_string_lossy())?;
        // the path still names the folder that was checked (nothing swapped in since)
        let (a, b) = (dir.metadata().map_err(|e| e.to_string())?, std::fs::symlink_metadata(root).map_err(|e| e.to_string())?);
        if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
            return Err(format!("{}: changed while it was checked", root.display()));
        }
        // SAFETY: flock(2) on our own descriptor, the same lock the filter takes per receive
        if unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Ok(None);
        }
        Ok(Some(FolderLock { path: root.to_path_buf(), dir: Some(dir) }))
    }

    fn delete_snapshot(&self, lock: &FolderLock, name: &str) -> Result<(), String> {
        let dir = lock.dir.as_ref().ok_or("folder not open")?;
        if name.contains('/') || rynas_btrfs::name::split_snapshot_name(name).is_none() {
            return Err(format!("{name}: not a snapshot name"));
        }
        // only a subvolume directly inside the checked folder
        let sub = rynas_btrfs::ioctl::open_dir_at(dir, name).map_err(|e| format!("{name}: {e}"))?;
        if !rynas_btrfs::ioctl::is_subvolume(&sub) {
            return Err(format!("{name}: not a subvolume"));
        }
        drop(sub);
        rynas_btrfs::ioctl::destroy_subvolume(dir, name).map_err(|e| format!("deleting {name}: {e}"))
    }
}

fn set_mode(p: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("{}: {e}", p.display()))
}

/// Written next to the file, then renamed over it.
fn write_file(p: &Path, text: &str, mode: u32) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = p.with_extension("rynas-tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(text.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    set_mode(&tmp, mode)?;
    std::fs::rename(&tmp, p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Memory (`--dry-system`, tests): folders and their snapshots, the keys file, the account.
#[derive(Default)]
pub struct MemoryReceive {
    pub folders: Mutex<BTreeMap<PathBuf, (bool, Vec<Entry>)>>,
    pub keys: Mutex<String>,
    pub account: Mutex<bool>,
    pub facts: Mutex<HostFacts>,
    pub copies: Mutex<Vec<String>>,
    /// The folders of the last sudoers file written (`None`: no account).
    pub sudo_roots: Mutex<Option<Vec<String>>>,
    /// (uid, gid, mode) of folders; missing: already `root:<account> 0750`.
    pub meta: Mutex<BTreeMap<PathBuf, (u32, u32, u32)>>,
    pub homes: Mutex<Vec<String>>,
    pub released: Mutex<Vec<PathBuf>>,
    /// (free, total) of every folder (`None`: unknown).
    pub space: Mutex<Option<(u64, u64)>>,
    /// Quota sizes by snapshot name (`None`: quotas off).
    pub quota: Mutex<Option<BTreeMap<String, (u64, u64)>>>,
    /// Snapshots measured, in order.
    pub measured: Mutex<Vec<PathBuf>>,
    /// Manifests by snapshot path.
    pub manifests: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
    /// A receive holds the lock of these folders.
    pub busy: Mutex<Vec<PathBuf>>,
    /// Snapshots deleted by the NAS, `<folder>/<name>`.
    pub deleted: Mutex<Vec<String>>,
}

/// The account group's gid in memory.
const MEMORY_GID: u32 = 990;

impl MemoryReceive {
    pub fn new() -> Self {
        let m = Self::default();
        *m.facts.lock().unwrap() = HostFacts {
            sshd: true,
            sshd_config: String::new(),
            sudo: true,
            btrfs: Some("/usr/bin/btrfs".into()),
            filter: true,
            volumes: vec!["/volume1".into()],
        };
        m
    }

    /// A folder with received snapshots, as another NAS would have left it (dry system).
    pub fn demo() -> Self {
        let m = Self::new();
        let now_s = (rynas_tasks::now_ms() / 1000) as i64;
        // one run a day at 03:00 (both subvolumes share the run's time, as btrbk names them)
        let day_start = now_s - now_s.rem_euclid(86_400) + 3 * 3600;
        let mut entries = Vec::new();
        for (i, g) in ["home", "rootfs"].into_iter().enumerate() {
            for d in [1, 2, 3, 9, 16, 40] {
                let t = day_start - d * 86_400;
                entries.push(Entry {
                    name: format!("{g}.{}", rynas_btrfs::name::long_timestamp(t)),
                    info: Some(rynas_btrfs::ioctl::SubvolInfo {
                        root_id: 300 + (d as u64) * 2 + i as u64,
                        readonly: true,
                        uuid: Some(format!("0b7d9e4c-1a2b-4c3d-8e5f-{:012x}", t + i as i64)),
                        received_uuid: Some(format!("6f1c2a8e-0d3b-4c5e-9a7f-{:012x}", t + i as i64)),
                        otime: t + 60,
                        ..Default::default()
                    }),
                });
            }
        }
        entries.sort_by(|a, b| rynas_btrfs::name::compare_names(&b.name, &a.name));
        // the root subvolume's newest snapshot carries the layout manifest
        let root_newest = format!("rootfs.{}", rynas_btrfs::name::long_timestamp(day_start - 86_400));
        m.manifests.lock().unwrap().insert(
            Path::new("/volume1/backup/old-laptop").join(root_newest),
            serde_json::to_vec(&serde_json::json!({
                "format": 1,
                "written_at_ms": (day_start - 86_400) as u64 * 1000,
                "boot": { "mode": "uefi", "loader": "systemd-boot" },
                "btrbk": {
                    "snapshot_dir": "btrbk_snapshots",
                    "subvolumes": [
                        { "name": "rootfs", "path": "@", "mount": "/" },
                        { "name": "home", "path": "@home", "mount": "/home" }
                    ]
                },
                "subvolumes": [ { "path": "@log", "mount": "/var/log", "excluded": true } ]
            }))
            .unwrap_or_default(),
        );
        m.folders
            .lock()
            .unwrap()
            .insert("/volume1/backup/old-laptop".into(), (true, entries));
        *m.space.lock().unwrap() = Some((1_200 << 30, 3_600 << 30));
        m
    }
}

impl ReceiveHost for MemoryReceive {
    fn facts(&self) -> HostFacts {
        self.facts.lock().unwrap().clone()
    }
    fn ensure_account(&self, _: &str, roots: &[String]) -> Result<Vec<String>, String> {
        let mut a = self.account.lock().unwrap();
        let first = !*a;
        *a = true;
        *self.sudo_roots.lock().unwrap() = Some(roots.to_vec());
        Ok(if first { vec![format!("account {ACCOUNT} created (memory)")] } else { Vec::new() })
    }
    fn remove_account(&self) -> Result<Vec<String>, String> {
        *self.account.lock().unwrap() = false;
        *self.sudo_roots.lock().unwrap() = None;
        self.keys.lock().unwrap().clear();
        Ok(vec![format!("account {ACCOUNT} removed (memory)")])
    }
    fn release_folder(&self, path: &Path) -> Result<(), String> {
        self.released.lock().unwrap().push(path.to_path_buf());
        Ok(())
    }
    fn folder_meta(&self, path: &Path) -> Option<(u32, u32, u32)> {
        if !self.folders.lock().unwrap().contains_key(path) {
            return None;
        }
        Some(self.meta.lock().unwrap().get(path).copied().unwrap_or((0, MEMORY_GID, 0o750)))
    }
    fn account_gid(&self) -> Option<u32> {
        Some(MEMORY_GID)
    }
    fn names(&self, uid: u32, gid: u32) -> (Option<String>, Option<String>) {
        let n = |id: u32| match id {
            0 => Some("root".to_string()),
            1000 => Some("marco".to_string()),
            MEMORY_GID => Some(ACCOUNT.to_string()),
            _ => None,
        };
        (n(uid), n(gid))
    }
    fn home_dirs(&self) -> Vec<String> {
        self.homes.lock().unwrap().clone()
    }
    fn write_keys(&self, content: &str) -> Result<(), String> {
        *self.keys.lock().unwrap() = content.to_string();
        Ok(())
    }
    fn create_folder(&self, path: &Path) -> Result<(), String> {
        self.folders
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), (true, Vec::new()));
        Ok(())
    }
    fn prepare_adopted(&self, path: &Path) -> Result<(), String> {
        self.meta.lock().unwrap().remove(path);
        Ok(())
    }
    fn scan(&self, path: &Path) -> Result<Scan, String> {
        Ok(self
            .folders
            .lock()
            .unwrap()
            .get(path)
            .map(|(s, e)| (*s, e.clone(), 0)))
    }
    fn usage(&self, path: &Path) -> Result<(u64, u64, u64), String> {
        let n = self.folders.lock().unwrap().get(path).map(|f| f.1.len()).unwrap_or(0) as u64;
        let (total, excl) = ((n * 12) << 30, (n * 300) << 20);
        Ok((total, excl, total - excl))
    }
    fn browse(&self, _: &Path, rel: &str) -> Result<(Vec<SnapshotEntry>, bool), Error> {
        rynas_btrfs::browse::components(rel)?;
        let e = |name: &str, kind, size| SnapshotEntry {
            name: name.into(),
            kind,
            size_bytes: size,
            mtime_ms: 1_762_500_000_000,
            mode: 0o755,
            uid: 1000,
            gid: 1000,
            link: None,
        };
        use rynas_api_types::snapshots::EntryKind::*;
        Ok((
            if rel.is_empty() {
                vec![e("Documents", Dir, 0), e("Pictures", Dir, 0), e("notes.txt", File, 2048)]
            } else {
                vec![e("report.odt", File, 48_213), e("budget.ods", File, 19_002)]
            },
            false,
        ))
    }
    fn copy_out(&self, s: &Path, rel: &str, dest: &Path, _: &dyn Fn() -> bool) -> Result<String, String> {
        let line = format!("{}:{rel} -> {}", s.display(), dest.display());
        self.copies.lock().unwrap().push(line.clone());
        Ok(format!("copied (memory): {line}"))
    }
    fn space(&self, path: &Path) -> Option<(u64, u64)> {
        if !self.folders.lock().unwrap().contains_key(path) {
            return None;
        }
        *self.space.lock().unwrap()
    }
    fn quota_sizes(&self, _: &Path, entries: &[Entry]) -> Option<BTreeMap<String, (u64, u64)>> {
        let q = self.quota.lock().unwrap();
        let q = q.as_ref()?;
        Some(entries.iter().filter_map(|e| q.get(&e.name).map(|v| (e.name.clone(), *v))).collect())
    }
    fn measure(&self, snapshot: &Path) -> Result<(u64, u64), String> {
        self.measured.lock().unwrap().push(snapshot.to_path_buf());
        // a stable made-up size per snapshot: about 12 GiB, a few hundred MiB its own
        let n = snapshot.to_string_lossy().bytes().fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(b)));
        Ok(((12 << 30) + ((n % 512) << 20), (n % 700 + 20) << 20))
    }
    fn read_manifest(&self, root: &Path, name: &str) -> Result<Option<Vec<u8>>, String> {
        let m = self.manifests.lock().unwrap().get(&root.join(name)).cloned();
        match m {
            Some(b) if b.len() as u64 > MANIFEST_MAX => Err("bigger than 1 MiB".into()),
            m => Ok(m),
        }
    }
    fn receiving(&self, root: &Path) -> bool {
        self.busy.lock().unwrap().iter().any(|p| p == root)
    }
    fn lock(&self, root: &Path) -> Result<Option<FolderLock>, String> {
        if !self.folders.lock().unwrap().contains_key(root) {
            return Err(format!("{}: no such folder", root.display()));
        }
        Ok((!self.receiving(root)).then(|| FolderLock { path: root.to_path_buf(), dir: None }))
    }
    fn delete_snapshot(&self, lock: &FolderLock, name: &str) -> Result<(), String> {
        let mut f = self.folders.lock().unwrap();
        let (_, entries) = f.get_mut(&lock.path).ok_or("no such folder")?;
        let before = entries.len();
        entries.retain(|e| e.name != name || e.info.is_none());
        if entries.len() == before {
            return Err(format!("{name}: no such subvolume"));
        }
        self.deleted.lock().unwrap().push(format!("{}/{name}", lock.path.display()));
        Ok(())
    }
}

// ------------------------------------------------------------------ retention, runs, health

const HOUR_MS: u64 = 3_600_000;
/// An interrupted receive older than this with no receive running has failed.
const FAILED_AFTER_MS: u64 = 10 * 60_000;
/// When the NAS owns retention it removes failed receives older than this.
const CLEAN_INTERRUPTED_AFTER_MS: u64 = HOUR_MS;
/// A receive still running after this long is stuck (a first full send over a slow link
/// can take many hours).
const STUCK_AFTER_MS: u64 = 24 * HOUR_MS;
/// At most this many deletes per retention pass (the next pass goes on): bounded I/O.
const PRUNE_MAX: usize = 50;
/// A manifest written this long before the snapshot that carries it is stale.
const MANIFEST_STALE_DAYS: u64 = 7;
/// Low space without known sizes: less than this share of the volume free.
const LOW_SPACE_PCT: u64 = 5;

/// Calendar buckets of a time in the NAS's local time: hour, day, week (from Sunday), month,
/// year (the counts of a [`Retention`], in that order).
fn buckets(ms: u64) -> Option<[i64; 5]> {
    use rynas_btrfs::name::{days_from_civil, long_timestamp, parse_timestamp};
    let s = parse_timestamp(&long_timestamp((ms / 1000) as i64))?;
    let day = days_from_civil(s.year, s.month, s.day);
    Some([
        day * 24 + i64::from(s.hour),
        day,
        // 1970-01-04 (day 3) was a Sunday
        (day - 3).div_euclid(7),
        i64::from(s.year) * 12 + i64::from(s.month) - 1,
        i64::from(s.year),
    ])
}

/// `Retention::min`: everything, nothing more than the newest, or within N units of a level.
enum MinKeep {
    All,
    Nothing,
    Within(usize, i64),
}

fn min_keep(m: &str) -> MinKeep {
    match m {
        "all" => MinKeep::All,
        "latest" | "no" | "" => MinKeep::Nothing,
        _ => {
            let unit = m.chars().last().unwrap_or('d');
            let n = m[..m.len() - unit.len_utf8()].parse::<i64>().unwrap_or(0);
            MinKeep::Within("hdwmy".find(unit).unwrap_or(1), n)
        }
    }
}

/// The time a snapshot counts at for retention: the time in its name (the PC's clock, as
/// btrbk on the PC counts), its NAS-side creation time when the name has none.
fn snap_ms(e: &Entry) -> Option<u64> {
    receive::name_ms(&e.name).or_else(|| receive::otime_ms(e))
}

/// What the NAS keeps of a device folder now, newest first (`entries` as scanned).
///
/// Per subvolume (btrbk name), the GFS counts keep the first snapshot of each hour, day,
/// week, month and year within that many of them, everything within `min`; on top, and
/// whatever the counts say:
/// - the newest complete snapshot (the parent of the PC's next incremental send);
/// - the NAS floor (`floor.keep_last` newest by NAS-side time, at least one, and nothing
///   younger than `floor.min_age_days`), as for deletes the PC asks for;
/// - names more than a day in the future (never deleted on the NAS's own);
/// - interrupted receives while a receive runs or for an hour after they stopped.
///
/// Folders with a snapshot name that are not subvolumes never appear.
pub fn retention_plan(
    entries: &[Entry],
    gfs: &Retention,
    floor: &DeletePolicy,
    receiving: bool,
    now_ms: u64,
) -> Vec<RetentionItem> {
    let parents = receive::parents(entries, now_ms);
    let floor = DeletePolicy { allowed: true, keep_last: floor.keep_last.max(1), min_age_days: floor.min_age_days };
    let group_of = |e: &Entry| rynas_btrfs::name::split_snapshot_name(&e.name).map(|(g, _)| g.to_string());
    let counted = |e: &Entry| e.info.is_some() && !e.incomplete() && !receive::future_dated(&e.name, now_ms);
    // the first (oldest) snapshot of every bucket, per subvolume and level
    let mut oldest_first: Vec<(&Entry, u64, [i64; 5])> = entries
        .iter()
        .filter(|e| counted(e))
        .filter_map(|e| snap_ms(e).and_then(|ms| buckets(ms).map(|b| (e, ms, b))))
        .collect();
    oldest_first.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| rynas_btrfs::name::compare_names(&a.0.name, &b.0.name)));
    let mut firsts: BTreeMap<(String, usize, i64), &str> = BTreeMap::new();
    let mut bucket_of: BTreeMap<&str, [i64; 5]> = BTreeMap::new();
    for (e, _, b) in &oldest_first {
        let g = group_of(e).unwrap_or_default();
        for (level, key) in b.iter().enumerate() {
            firsts.entry((g.clone(), level, *key)).or_insert(e.name.as_str());
        }
        bucket_of.insert(e.name.as_str(), *b);
    }
    let now_b = buckets(now_ms);
    let counts = [gfs.hourly, gfs.daily, gfs.weekly, gfs.monthly, gfs.yearly];
    let reasons = [
        RetentionReason::Hourly,
        RetentionReason::Daily,
        RetentionReason::Weekly,
        RetentionReason::Monthly,
        RetentionReason::Yearly,
    ];
    let min = min_keep(&gfs.min);
    let mut out = Vec::new();
    for e in entries.iter().filter(|e| e.info.is_some()) {
        let group = group_of(e).unwrap_or_default();
        let (keep, reason) = if e.incomplete() {
            let stale = receive::otime_ms(e).is_none_or(|t| now_ms >= t.saturating_add(CLEAN_INTERRUPTED_AFTER_MS));
            (receiving || !stale, RetentionReason::Interrupted)
        } else if parents.contains(&e.name) {
            (true, RetentionReason::Parent)
        } else if receive::delete_refusal(&e.name, entries, &floor, now_ms).is_some() {
            (true, RetentionReason::Floor)
        } else if let (Some(b), Some(nb)) = (bucket_of.get(e.name.as_str()), now_b) {
            let delta = |l: usize| nb[l] - b[l];
            let recent = match min {
                MinKeep::All => true,
                MinKeep::Nothing => false,
                MinKeep::Within(l, n) => delta(l) < n,
            };
            if recent || (0..5).any(|l| delta(l) < 0) {
                (true, RetentionReason::Recent)
            } else if let Some(l) = (0..5).find(|&l| {
                counts[l] > 0 && delta(l) < i64::from(counts[l]) && firsts.get(&(group.clone(), l, b[l])) == Some(&e.name.as_str())
            }) {
                (true, reasons[l])
            } else {
                (false, RetentionReason::Expired)
            }
        } else {
            // named in the future (or no time at all): never deleted on the NAS's own
            (true, RetentionReason::Future)
        };
        out.push(RetentionItem { name: e.name.clone(), group, keep, reason });
    }
    out
}

/// The time part of a snapshot name without btrbk's `_N` suffix: the run it belongs to.
fn run_key(name: &str) -> Option<&str> {
    let (_, ts) = rynas_btrfs::name::split_snapshot_name(name)?;
    Some(ts.split_once('_').filter(|(a, _)| a.len() != 8).map_or(ts, |(a, _)| a))
}

/// The snapshots grouped by run (same timestamp), newest first. `expected`: the subvolumes
/// a full set has.
fn runs(list: &[ReceivedSnapshot], expected: &BTreeSet<String>) -> Vec<PcRun> {
    let mut by: BTreeMap<&str, Vec<&ReceivedSnapshot>> = BTreeMap::new();
    for s in list.iter().filter(|s| !s.not_subvolume) {
        if let Some(k) = run_key(&s.name) {
            by.entry(k).or_default().push(s);
        }
    }
    let mut out: Vec<PcRun> = by
        .into_iter()
        .map(|(k, snaps)| {
            let complete: BTreeSet<&str> = snaps.iter().filter(|s| !s.incomplete).map(|s| s.group.as_str()).collect();
            let sum = |f: fn(&SnapshotSize) -> u64| {
                snaps.iter().filter(|s| !s.incomplete).map(|s| s.size.as_ref().map(f)).sum::<Option<u64>>()
            };
            PcRun {
                timestamp: k.to_string(),
                created_ms: snaps.iter().filter_map(|s| s.created_ms).min(),
                snapshots: snaps.iter().map(|s| s.name.clone()).collect(),
                lacks: expected.iter().filter(|g| !complete.contains(g.as_str())).cloned().collect(),
                incomplete: snaps.iter().any(|s| s.incomplete),
                referenced_bytes: sum(|z| z.referenced_bytes),
                exclusive_bytes: sum(|z| z.exclusive_bytes),
            }
        })
        .collect();
    out.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then_with(|| b.timestamp.cmp(&a.timestamp)));
    out
}

/// What B1 reads of the layout manifest (`/.rynas/manifest.json` inside the root
/// subvolume, written by `rynas-agent manifest`; `docs/research/pc-backup.md` §11.3).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Manifest {
    written_at_ms: Option<u64>,
    loader: Option<String>,
    /// btrbk names of the subvolumes a restore needs (`btrbk.subvolumes[].name`).
    subvolumes: Vec<String>,
    /// One of them is mounted at `/`.
    root: bool,
    /// For display only (§11.2.4): kept when they parse, dropped otherwise.
    disks: Vec<rynas_api_types::pcagent::Disk>,
    layout: Vec<rynas_api_types::pcagent::BtrbkSubvolume>,
    boot_copies: Vec<rynas_api_types::pcagent::BootCopy>,
}

/// A manifest found in a snapshot, or why it cannot be used.
type ManifestRead = Result<Manifest, (BareMetalCode, Option<String>)>;

/// Parses a manifest (untrusted input from the PC: size capped before, shape checked here).
fn parse_manifest(b: &[u8]) -> ManifestRead {
    let invalid = || (BareMetalCode::ManifestInvalid, None);
    let v: serde_json::Value = serde_json::from_slice(b).map_err(|_| invalid())?;
    let format = v.get("format").and_then(serde_json::Value::as_u64).ok_or_else(invalid)?;
    if format != 1 {
        return Err((BareMetalCode::ManifestUnsupported, Some(format.to_string())));
    }
    let short = |s: &str| (!s.is_empty() && s.len() <= 128 && !s.contains(['/', '\n', '\0'])).then(|| s.to_string());
    let subs = v
        .pointer("/btrbk/subvolumes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(invalid)?;
    let mut subvolumes = Vec::new();
    let mut root = false;
    for s in subs.iter().take(256) {
        let name = s.get("name").and_then(serde_json::Value::as_str).and_then(short).ok_or_else(invalid)?;
        root |= s.get("mount").and_then(serde_json::Value::as_str) == Some("/");
        if !subvolumes.contains(&name) {
            subvolumes.push(name);
        }
    }
    // the display parts: typed, bounded, and never a reason to refuse the manifest
    fn shown<T: serde::de::DeserializeOwned>(v: Option<&serde_json::Value>, max: usize) -> Vec<T> {
        let Some(a) = v.and_then(serde_json::Value::as_array) else { return Vec::new() };
        a.iter().take(max).filter_map(|x| serde_json::from_value(x.clone()).ok()).collect()
    }
    let mut disks: Vec<rynas_api_types::pcagent::Disk> = shown(v.get("disks"), 16);
    for d in &mut disks {
        if let Some(t) = &mut d.table {
            t.partitions.truncate(128);
        }
    }
    Ok(Manifest {
        written_at_ms: v.get("written_at_ms").and_then(serde_json::Value::as_u64),
        loader: v.pointer("/boot/loader").and_then(serde_json::Value::as_str).and_then(short),
        subvolumes,
        root,
        disks,
        layout: shown(v.pointer("/btrbk/subvolumes"), 256),
        boot_copies: shown(v.pointer("/btrbk/boot_copies"), 8),
    })
}

/// "Ready for bare metal": a manifest in a newest snapshot, every subvolume it needs
/// received, a root and a known boot loader. `manifest`: (snapshot, its NAS time, what was
/// read).
fn bare_metal(
    complete_groups: &BTreeSet<String>,
    manifest: Option<(String, Option<u64>, ManifestRead)>,
    newest_run: Option<&PcRun>,
) -> BareMetal {
    let gap = |code, subject: Option<String>| BareMetalGap { code, subject };
    let mut b = BareMetal::default();
    if complete_groups.is_empty() {
        b.missing.push(gap(BareMetalCode::NoSnapshots, None));
        return b;
    }
    match manifest {
        None => b.missing.push(gap(BareMetalCode::NoManifest, None)),
        Some((snap, _, Err((code, subject)))) => {
            b.manifest_snapshot = Some(snap);
            b.missing.push(gap(code, subject));
        }
        Some((snap, snap_ms, Ok(m))) => {
            b.manifest_snapshot = Some(snap);
            if !m.root {
                b.missing.push(gap(BareMetalCode::NoRoot, None));
            }
            for s in m.subvolumes.iter().filter(|s| !complete_groups.contains(*s)) {
                b.missing.push(gap(BareMetalCode::SubvolumeMissing, Some(s.clone())));
            }
            if m.loader.is_none() {
                b.missing.push(gap(BareMetalCode::LoaderUnknown, None));
            }
            if let (Some(w), Some(s)) = (m.written_at_ms, snap_ms)
                && s > w + MANIFEST_STALE_DAYS * DAY_MS
            {
                b.warnings.push(gap(BareMetalCode::ManifestStale, Some(((s - w) / DAY_MS).to_string())));
            }
            if let Some(r) = newest_run {
                for g in r.lacks.iter().filter(|g| m.subvolumes.contains(g)) {
                    b.warnings.push(gap(BareMetalCode::LatestRunIncomplete, Some(g.clone())));
                }
            }
            b.subvolumes = m.subvolumes;
            b.loader = m.loader;
            b.disks = m.disks;
            b.layout = m.layout;
            b.boot_copies = m.boot_copies;
        }
    }
    b.ready = b.missing.is_empty();
    b
}

/// Facts the alerts of one device are worked out from.
struct HealthFacts<'a> {
    list: &'a [ReceivedSnapshot],
    expected: &'a BTreeSet<String>,
    alert_hours: u32,
    receiving: bool,
    space: Option<(u64, u64)>,
    /// One more full set of snapshots, when every size is known.
    full_set_bytes: Option<u64>,
    now_ms: u64,
}

/// What is wrong now: late PC, a subvolume that stopped, a failed or stuck receive, low
/// space.
fn issues(f: &HealthFacts) -> Vec<PcIssue> {
    let issue = |code, subject: Option<String>, since_ms: Option<u64>| PcIssue {
        code,
        subject,
        since_ms,
        free_bytes: None,
        needed_bytes: None,
    };
    let mut out = Vec::new();
    let mut newest: BTreeMap<&str, u64> = BTreeMap::new();
    for s in f.list.iter().filter(|s| !s.incomplete && !s.not_subvolume) {
        if let Some(t) = s.received_at_ms.or(s.created_ms) {
            let n = newest.entry(s.group.as_str()).or_insert(t);
            *n = (*n).max(t);
        }
    }
    let limit = u64::from(f.alert_hours) * HOUR_MS;
    if f.alert_hours > 0 && let Some(&last) = newest.values().max() {
        if f.now_ms > last + limit {
            out.push(issue(PcIssueCode::Late, None, Some(last)));
        } else {
            for g in f.expected {
                match newest.get(g.as_str()) {
                    Some(&t) if f.now_ms > t + limit => out.push(issue(PcIssueCode::SubvolumeLate, Some(g.clone()), Some(t))),
                    None => out.push(issue(PcIssueCode::SubvolumeLate, Some(g.clone()), None)),
                    _ => {}
                }
            }
        }
    }
    // while receiving, the newest interrupted entry is the receive running now; otherwise
    // the oldest tells how long a failed one has been there
    let incomplete = f.list.iter().filter(|s| s.incomplete);
    let pick = if f.receiving {
        incomplete.max_by_key(|s| s.received_at_ms.unwrap_or(0))
    } else {
        incomplete.min_by_key(|s| s.received_at_ms.unwrap_or(0))
    };
    if let Some(s) = pick {
        let since = s.received_at_ms.unwrap_or(0);
        if f.receiving && f.now_ms > since + STUCK_AFTER_MS {
            out.push(issue(PcIssueCode::ReceiveStuck, Some(s.name.clone()), Some(since)));
        } else if !f.receiving && f.now_ms > since + FAILED_AFTER_MS {
            out.push(issue(PcIssueCode::ReceiveFailed, Some(s.name.clone()), Some(since)));
        }
    }
    if let Some((free, total)) = f.space {
        let needed = f.full_set_bytes.unwrap_or(total / 100 * LOW_SPACE_PCT);
        if free < needed {
            out.push(PcIssue {
                free_bytes: Some(free),
                needed_bytes: Some(needed),
                ..issue(PcIssueCode::LowSpace, None, None)
            });
        }
    }
    out
}

// ------------------------------------------------------------------ records

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Stored {
    description: String,
    path: String,
    public_key: String,
    alert_days: u32,
    delete: DeletePolicy,
    adopted: bool,
    created_at_ms: u64,
    usage: Option<ReceiveUsage>,
    /// The late notification is open.
    #[serde(default)]
    alerted: bool,
    #[serde(default)]
    alert_hours: u32,
    #[serde(default)]
    retention: PcRetention,
    #[serde(default)]
    ignored_groups: Vec<String>,
    /// Measured sizes by the snapshot's subvolume UUID (read-only snapshots: the referenced
    /// size never changes; the exclusive one grows as neighbours go).
    #[serde(default)]
    sizes: BTreeMap<String, SnapshotSize>,
    /// The other open notifications: key → event code.
    #[serde(default)]
    open_alerts: BTreeMap<String, String>,
    #[serde(default)]
    last_prune: Option<PruneRun>,
}

impl Stored {
    /// The rules the NAS applies to deletes: the PC's (through the filter) or, when the NAS
    /// owns retention, the floor of its own deletes (always at least the newest one).
    fn rules(&self) -> DeletePolicy {
        match self.retention.owner {
            RetentionOwner::Pc => self.delete,
            RetentionOwner::Nas => DeletePolicy { allowed: true, keep_last: self.delete.keep_last.max(1), ..self.delete },
        }
    }

    /// The policy written in the key line: when the NAS owns retention the PC can only add
    /// (and clear its own interrupted receives).
    fn key_policy(&self) -> DeletePolicy {
        match self.retention.owner {
            RetentionOwner::Pc => self.delete,
            RetentionOwner::Nas => DeletePolicy { allowed: false, ..self.delete },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Registry {
    targets: BTreeMap<String, Stored>,
}

pub struct PcBackup {
    pub ops: Arc<Ops>,
    pub host: Arc<dyn ReceiveHost>,
    pub notifier: Arc<dyn Notifier>,
    pub state_dir: PathBuf,
    pub admins: AdminPolicy,
    pub hostname: String,
    reg: Mutex<Registry>,
    /// One change at a time (create, update, remove rewrite the keys file; retention passes
    /// and snapshot deletes).
    change: tokio::sync::Mutex<()>,
    /// Manifests read from snapshots, by subvolume UUID (received snapshots are read-only:
    /// what a snapshot holds never changes). `None`: the snapshot has none.
    manifests: Mutex<BTreeMap<String, Option<ManifestRead>>>,
}

/// Most manifests remembered (a few per device).
const MANIFEST_CACHE: usize = 256;

/// One device as the API shows it.
struct View {
    target: PcTarget,
    list: Vec<ReceivedSnapshot>,
    runs: Vec<PcRun>,
    quota: bool,
}

pub type PbState = Option<Arc<PcBackup>>;

impl PcBackup {
    pub fn new(
        ops: Arc<Ops>,
        host: Arc<dyn ReceiveHost>,
        notifier: Arc<dyn Notifier>,
        state_dir: PathBuf,
        admins: AdminPolicy,
        hostname: String,
    ) -> Arc<Self> {
        let reg = std::fs::read(Self::reg_path(&state_dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Arc::new(Self {
            ops,
            host,
            notifier,
            state_dir,
            admins,
            hostname,
            reg: Mutex::new(reg),
            change: tokio::sync::Mutex::new(()),
            manifests: Mutex::new(BTreeMap::new()),
        })
    }

    fn reg_path(state_dir: &Path) -> PathBuf {
        state_dir.join("pcbackup/targets.json")
    }

    /// Changes the records and saves them; undone when saving fails.
    fn edit(&self, f: impl FnOnce(&mut Registry)) -> Result<(), String> {
        let mut g = self.reg.lock().unwrap();
        let before = g.clone();
        f(&mut g);
        let p = Self::reg_path(&self.state_dir);
        let r = p
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .map_err(|e| e.to_string())
            .and_then(|()| write_file(&p, &serde_json::to_string_pretty(&*g).unwrap_or_default(), 0o600));
        if r.is_err() {
            *g = before;
        }
        r
    }

    fn stored(&self, device: &str) -> Res<Stored> {
        self.reg
            .lock()
            .unwrap()
            .targets
            .get(device)
            .cloned()
            .ok_or_else(|| Error::not_found(format!("no PC backup target {device}")))
    }

    /// The keys file from the records.
    fn keys_text(&self) -> String {
        let reg = self.reg.lock().unwrap();
        let lines: Vec<String> = reg
            .targets
            .iter()
            .filter_map(|(d, t)| {
                let (k, b) = receive::parse_public_key(&t.public_key).ok()?;
                Some(receive::key_line(d, &t.path, &t.key_policy(), &k, &b))
            })
            .collect();
        receive::authorized_keys(&lines)
    }

    /// The manifest in the newest snapshot of some subvolume (the root one first), read once
    /// per snapshot.
    fn find_manifest(&self, root: &Path, entries: &[Entry], parents: &BTreeSet<String>) -> Option<(String, Option<u64>, ManifestRead)> {
        let mut names: Vec<&Entry> = entries.iter().filter(|e| parents.contains(&e.name)).collect();
        let rootish = |e: &Entry| {
            let g = rynas_btrfs::name::split_snapshot_name(&e.name).map(|x| x.0).unwrap_or_default();
            !matches!(g, "@" | "root" | "rootfs" | "@root")
        };
        names.sort_by_key(|e| rootish(e));
        for e in names {
            let key = e.info.as_ref().and_then(|i| i.uuid.clone()).unwrap_or_else(|| format!("{}/{}", root.display(), e.name));
            let cached = self.manifests.lock().unwrap().get(&key).cloned();
            let read = match cached {
                Some(r) => r,
                None => {
                    let r = match self.host.read_manifest(root, &e.name) {
                        Ok(Some(b)) => Some(parse_manifest(&b)),
                        Ok(None) => None,
                        Err(m) => {
                            tracing::info!("pc backup: manifest of {}/{}: {m}", root.display(), e.name);
                            Some(Err((BareMetalCode::ManifestInvalid, None)))
                        }
                    };
                    let mut c = self.manifests.lock().unwrap();
                    if c.len() >= MANIFEST_CACHE {
                        c.clear();
                    }
                    c.insert(key, r.clone());
                    r
                }
            };
            if let Some(r) = read {
                return Some((e.name.clone(), receive::otime_ms(e), r));
            }
        }
        None
    }

    fn view(&self, device: &str, t: &Stored, now_ms: u64) -> View {
        let root = Path::new(&t.path);
        let scan = self.host.scan(root);
        let present = matches!(scan, Ok(Some(_)));
        let entries = match scan {
            Ok(Some((_, e, _))) => e,
            _ => Vec::new(),
        };
        let mut s = receive::summarize(&entries, &t.rules(), now_ms);
        let quota = if present { self.host.quota_sizes(root, &entries) } else { None };
        for (snap, e) in s.list.iter_mut().zip(&entries) {
            snap.size = match (&quota, e.info.as_ref().and_then(|i| i.uuid.as_ref())) {
                (Some(q), _) => q.get(&e.name).map(|&(r, x)| SnapshotSize {
                    referenced_bytes: r,
                    exclusive_bytes: x,
                    read_at_ms: now_ms,
                    from_quota: true,
                }),
                (None, Some(uuid)) => t.sizes.get(uuid).copied(),
                (None, None) => None,
            };
        }
        let complete: BTreeSet<String> = s
            .list
            .iter()
            .filter(|x| !x.incomplete && !x.not_subvolume)
            .map(|x| x.group.clone())
            .collect();
        let parents = receive::parents(&entries, now_ms);
        let manifest = if present { self.find_manifest(root, &entries, &parents) } else { None };
        let ignored = |g: &String| !t.ignored_groups.contains(g);
        let expected: BTreeSet<String> = match &manifest {
            Some((_, _, Ok(m))) => m.subvolumes.iter().filter(|g| ignored(g)).cloned().collect(),
            _ => complete.iter().filter(|g| ignored(g)).cloned().collect(),
        };
        let runs = runs(&s.list, &expected);
        let space = if present { self.host.space(root) } else { None };
        let receiving = present && self.host.receiving(root);
        let full_set_bytes = s
            .list
            .iter()
            .filter(|x| x.parent && expected.contains(&x.group))
            .map(|x| x.size.map(|z| z.referenced_bytes))
            .sum::<Option<u64>>()
            .filter(|&b| b > 0);
        let alert_hours = alert_hours(t.alert_hours, t.alert_days);
        let issues = if present {
            issues(&HealthFacts {
                list: &s.list,
                expected: &expected,
                alert_hours,
                receiving,
                space,
                full_set_bytes,
                now_ms,
            })
        } else {
            Vec::new()
        };
        let state = match (present, s.last_received_ms) {
            (false, _) => ReceiveState::Missing,
            (true, None) => ReceiveState::Waiting,
            (true, Some(ms)) if alert_hours > 0 && now_ms > ms + u64::from(alert_hours) * HOUR_MS => ReceiveState::Late,
            (true, Some(_)) => ReceiveState::Ok,
        };
        // the newest run that brought something complete (not a receive still going)
        let newest_run = runs.iter().find(|r| {
            r.snapshots.iter().any(|n| s.list.iter().any(|x| &x.name == n && !x.incomplete))
        });
        let bare_metal = bare_metal(&complete, manifest, newest_run);
        let fp = receive::parse_public_key(&t.public_key)
            .ok()
            .and_then(|(k, b)| receive::fingerprint(&b).map(|f| format!("{k} {f}")));
        let target = PcTarget {
            device: device.to_string(),
            description: t.description.clone(),
            path: t.path.clone(),
            public_key: t.public_key.clone(),
            key_fingerprint: fp,
            alert_days: t.alert_days,
            delete: t.delete,
            adopted: t.adopted,
            created_at_ms: t.created_at_ms,
            state,
            snapshots: s.list.iter().filter(|x| !x.incomplete && !x.not_subvolume).count() as u32,
            incomplete: s.incomplete,
            last_received_ms: s.last_received_ms,
            groups: s.groups,
            usage: t.usage,
            alert_hours: t.alert_hours,
            retention: t.retention.clone(),
            ignored_groups: t.ignored_groups.clone(),
            issues,
            bare_metal,
            free_bytes: space.map(|x| x.0),
            volume_bytes: space.map(|x| x.1),
            receiving,
            last_prune: t.last_prune.clone(),
        };
        View { target, list: s.list, runs, quota: quota.is_some() }
    }

    fn list(&self) -> Vec<PcTarget> {
        let all: Vec<(String, Stored)> = self
            .reg
            .lock()
            .unwrap()
            .targets
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let now = rynas_tasks::now_ms();
        all.iter().map(|(d, t)| self.view(d, t, now).target).collect()
    }

    fn setup(&self) -> ReceiveSetup {
        let f = self.host.facts();
        let mut problems = Vec::new();
        let p = |code, subject: Option<String>| SetupProblem { code, subject };
        if !f.sshd {
            problems.push(p(SetupProblemCode::SshdMissing, None));
        } else if !receive::sshd_allows(&f.sshd_config, ACCOUNT, &[ACCOUNT]) {
            problems.push(p(SetupProblemCode::SshdAllowList, Some(ACCOUNT.into())));
        }
        if !f.sudo {
            problems.push(p(SetupProblemCode::SudoMissing, None));
        }
        if f.btrfs.is_none() {
            problems.push(p(SetupProblemCode::BtrfsMissing, None));
        }
        if !f.filter {
            problems.push(p(SetupProblemCode::FilterMissing, Some(FILTER.into())));
        }
        ReceiveSetup {
            account: ACCOUNT.into(),
            filter: FILTER.into(),
            host: self.hostname.clone(),
            ssh_port: receive::sshd_port(&f.sshd_config),
            volumes: f.volumes,
            problems,
        }
    }

    /// Opens or closes the notifications of each device (hourly): late PC, a subvolume that
    /// stopped, a failed or stuck receive, low space. Each fires once and is closed when it
    /// is over.
    pub fn check_freshness(&self) {
        let now = rynas_tasks::now_ms();
        let all: Vec<(String, Stored)> = self
            .reg
            .lock()
            .unwrap()
            .targets
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (d, t) in all {
            let v = self.view(&d, &t, now).target;
            let hours = v.alert_after_hours();
            let late = hours > 0 && v.state == ReceiveState::Late;
            if late != t.alerted {
                let key = format!("pcbackup:late:{d}");
                let ev = if late {
                    let since_h = v.last_received_ms.map(|ms| (now - ms) / HOUR_MS).unwrap_or(0);
                    let days = since_h / 24;
                    let mut ev = firing(
                        NotifyCategory::Backup,
                        NotifySeverity::Warning,
                        "pcbackup",
                        key,
                        if since_h >= 48 {
                            format!("No backup from {d} for {days} days")
                        } else {
                            format!("No backup from {d} for {since_h} hours")
                        },
                    );
                    ev.body = format!("Nothing received in {} for {since_h} hours (alert after {hours}).", t.path);
                    // days only from two days on: the texts use hours below that
                    let ev = field(code(ev, "pcbackup.late"), "device", &d);
                    let ev = field(ev, "days", if since_h >= 48 { days.to_string() } else { String::new() });
                    field(field(field(ev, "hours", since_h), "alert_hours", hours), "alert_days", t.alert_days)
                } else {
                    let mut ev = resolved(NotifyCategory::Backup, "pcbackup", key, format!("Backups from {d} arrive again"));
                    ev.body = format!("{d} sent a snapshot to {}.", t.path);
                    field(code(ev, "pcbackup.resumed"), "device", &d)
                };
                if self
                    .edit(|r| {
                        if let Some(x) = r.targets.get_mut(&d) {
                            x.alerted = late;
                        }
                    })
                    .is_ok()
                {
                    self.notifier.emit(ev);
                }
            }
            self.other_alerts(&d, &t, &v);
        }
    }

    /// The notifications besides "late": key → (code, event) of what is wrong now, compared
    /// with the open ones.
    fn other_alerts(&self, d: &str, t: &Stored, v: &PcTarget) {
        let gib = |b: u64| format!("{:.1}", b as f64 / (1u64 << 30) as f64);
        let mut now_open: BTreeMap<String, (String, rynas_api_types::notify::NewEvent)> = BTreeMap::new();
        for i in &v.issues {
            let subject = i.subject.clone().unwrap_or_default();
            let (k, c, sev, title, body) = match i.code {
                PcIssueCode::Late => continue,
                PcIssueCode::SubvolumeLate => (
                    format!("subvol:{subject}"),
                    "pcbackup.subvolume_late",
                    NotifySeverity::Warning,
                    format!("{subject} of {d} is not backed up any more"),
                    format!("{d} still sends other subvolumes, but nothing new of {subject} for longer than the alert time."),
                ),
                PcIssueCode::ReceiveFailed => (
                    "receive".into(),
                    "pcbackup.receive_failed",
                    NotifySeverity::Warning,
                    format!("A backup from {d} was interrupted"),
                    format!("{subject} in {} was not received completely; nothing is receiving now.", t.path),
                ),
                PcIssueCode::ReceiveStuck => (
                    "receive".into(),
                    "pcbackup.receive_stuck",
                    NotifySeverity::Warning,
                    format!("A backup from {d} has been running for a long time"),
                    format!("{subject} in {} has been receiving for more than a day.", t.path),
                ),
                PcIssueCode::LowSpace => (
                    "space".into(),
                    "pcbackup.low_space",
                    NotifySeverity::Error,
                    format!("Little space left for the backups of {d}"),
                    format!(
                        "{} GiB free on the volume of {}, {} GiB wanted for another full set.",
                        gib(i.free_bytes.unwrap_or(0)),
                        t.path,
                        gib(i.needed_bytes.unwrap_or(0))
                    ),
                ),
            };
            let mut ev = firing(NotifyCategory::Backup, sev, "pcbackup", format!("pcbackup:{k}:{d}"), title);
            ev.body = body;
            let ev = field(field(code(ev, c), "device", d), "subject", &subject);
            let ev = field(field(ev, "free_gib", gib(i.free_bytes.unwrap_or(0))), "needed_gib", gib(i.needed_bytes.unwrap_or(0)));
            now_open.insert(k, (c.to_string(), ev));
        }
        let mut emit = Vec::new();
        for (k, (c, ev)) in &now_open {
            if t.open_alerts.get(k) != Some(c) {
                emit.push(ev.clone());
            }
        }
        for (k, c) in &t.open_alerts {
            if now_open.contains_key(k) {
                continue;
            }
            let (rc, title) = match c.as_str() {
                "pcbackup.subvolume_late" => ("pcbackup.subvolume_resumed", format!("{} of {d} is backed up again", k.trim_start_matches("subvol:"))),
                "pcbackup.low_space" => ("pcbackup.space_ok", format!("Enough space again for the backups of {d}")),
                _ => ("pcbackup.receive_ok", format!("Backups from {d} are received normally again")),
            };
            let ev = resolved(NotifyCategory::Backup, "pcbackup", format!("pcbackup:{k}:{d}"), title);
            emit.push(field(field(code(ev, rc), "device", d), "subject", k.trim_start_matches("subvol:")));
        }
        if emit.is_empty() {
            return;
        }
        let open: BTreeMap<String, String> = now_open.into_iter().map(|(k, (c, _))| (k, c)).collect();
        if self
            .edit(|r| {
                if let Some(x) = r.targets.get_mut(d) {
                    x.open_alerts = open;
                }
            })
            .is_ok()
        {
            for ev in emit {
                self.notifier.emit(ev);
            }
        }
    }
}

impl PcBackup {
    /// The folders of the shares in the applied configuration (none when unreadable).
    fn share_paths(&self) -> Vec<String> {
        self.ops
            .config
            .applied(rynas_services::SHARES_KEY)
            .ok()
            .and_then(|v| crate::services::from_value::<rynas_api_types::SharesConfig>(v).ok())
            .map(|d| d.shares.into_iter().map(|s| s.path).filter(|p| !p.is_empty()).collect())
            .unwrap_or_default()
    }

    /// Why `path` cannot be adopted (its owner and mode would change): see [`adopt_refusal`].
    fn adopt_refused(&self, path: &str) -> Option<String> {
        adopt_refusal(path, &self.share_paths(), &self.host.home_dirs())
    }

    /// What adopting changes on the folder itself; `None` when nothing.
    fn permission_change(&self, path: &Path) -> Option<PermissionChange> {
        let (uid, gid, mode) = self.host.folder_meta(path)?;
        let account = self.host.account_gid();
        if uid == 0 && account == Some(gid) && mode == 0o750 {
            return None;
        }
        let (owner, group) = self.host.names(uid, gid);
        Some(PermissionChange {
            uid,
            gid,
            mode,
            owner,
            group,
            new_owner: "root".into(),
            new_group: ACCOUNT.into(),
            new_mode: 0o750,
        })
    }

    /// The device and folder checks of a new target, against the current records.
    fn clash(&self, device: &str, path: &str) -> Res<()> {
        let reg = self.reg.lock().unwrap();
        if reg.targets.contains_key(device) {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::ALREADY_EXISTS,
                format!("{device} already exists"),
            )
            .field("device"));
        }
        let clash = reg.targets.iter().find(|(_, t)| {
            t.path == path
                || path.starts_with(&format!("{}/", t.path))
                || t.path.starts_with(&format!("{path}/"))
        });
        if let Some((d, _)) = clash {
            return Err(Error::invalid("path", format!("overlaps the folder of {d}")));
        }
        Ok(())
    }

    /// The folders of every device (the sudoers lines).
    fn roots(&self) -> Vec<String> {
        self.reg.lock().unwrap().targets.values().map(|t| t.path.clone()).collect()
    }

    /// What the NAS would delete of `t`'s folder now with these settings (read only).
    fn preview(&self, t: &Stored, retention: &PcRetention, floor: &DeletePolicy) -> Result<PcRetentionPreview, String> {
        let root = Path::new(&t.path);
        let Some((_, entries, _)) = self.host.scan(root)? else {
            return Err(format!("{}: no such folder", t.path));
        };
        let now = rynas_tasks::now_ms();
        let items = retention_plan(&entries, &retention.gfs, floor, self.host.receiving(root), now);
        let quota = self.host.quota_sizes(root, &entries);
        let excl = |name: &str| match &quota {
            Some(q) => q.get(name).map(|x| x.1),
            None => entries
                .iter()
                .find(|e| e.name == name)
                .and_then(|e| e.info.as_ref()?.uuid.as_ref())
                .and_then(|u| t.sizes.get(u))
                .map(|z| z.exclusive_bytes),
        };
        let gone: Vec<&RetentionItem> = items.iter().filter(|i| !i.keep).collect();
        Ok(PcRetentionPreview {
            delete: gone.len() as u32,
            frees_at_least_bytes: gone.iter().map(|i| excl(&i.name)).sum(),
            items,
        })
    }

    /// One retention pass of a device whose retention the NAS owns: under the folder's lock
    /// (no receive meanwhile), on a fresh listing, at most [`PRUNE_MAX`] deletes. Recorded as
    /// the device's last pass. `log` gets a line per snapshot.
    fn prune(&self, device: &str, log: &dyn Fn(&str)) -> Result<PruneRun, String> {
        let t = self.stored(device).map_err(|e| e.2)?;
        if t.retention.owner != RetentionOwner::Nas {
            return Err(format!("{device}: the PC deletes its old snapshots, not the NAS"));
        }
        let mut run = PruneRun { at_ms: rynas_tasks::now_ms(), ..Default::default() };
        let root = Path::new(&t.path);
        let result = (|| -> Result<(), String> {
            let Some(lock) = self.host.lock(root)? else {
                return Err("a receive is running: the next pass goes on".into());
            };
            let Some((_, entries, _)) = self.host.scan(root)? else {
                return Err(format!("{}: no such folder", t.path));
            };
            let plan = retention_plan(&entries, &t.retention.gfs, &t.rules(), false, rynas_tasks::now_ms());
            let mut gone = Vec::new();
            for i in plan.iter().filter(|i| !i.keep).take(PRUNE_MAX) {
                self.host.delete_snapshot(&lock, &i.name)?;
                log(&format!("{}: deleted ({})", i.name, if i.reason == RetentionReason::Interrupted { "interrupted receive" } else { "beyond the retention" }));
                run.deleted += 1;
                if i.reason == RetentionReason::Interrupted {
                    run.interrupted += 1;
                }
                if let Some(u) = entries.iter().find(|e| e.name == i.name).and_then(|e| e.info.as_ref()?.uuid.clone()) {
                    gone.push(u);
                }
            }
            if !gone.is_empty() {
                let _ = self.edit(|r| {
                    if let Some(x) = r.targets.get_mut(device) {
                        x.sizes.retain(|u, _| !gone.contains(u));
                    }
                });
            }
            Ok(())
        })();
        run.error = result.err();
        self.edit(|r| {
            if let Some(x) = r.targets.get_mut(device) {
                x.last_prune = Some(run.clone());
            }
        })?;
        Ok(run)
    }

    /// Retention passes of every device whose retention the NAS owns (hourly).
    pub fn prune_all(&self) {
        let devices: Vec<String> = self
            .reg
            .lock()
            .unwrap()
            .targets
            .iter()
            .filter(|(_, t)| t.retention.owner == RetentionOwner::Nas)
            .map(|(d, _)| d.clone())
            .collect();
        for d in devices {
            match self.prune(&d, &|l| tracing::info!("pc backup {d}: {l}")) {
                Ok(r) if r.deleted > 0 || r.error.is_some() => {
                    tracing::info!("pc backup {d}: retention deleted {} ({})", r.deleted, r.error.unwrap_or_default())
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("pc backup {d}: retention: {e}"),
            }
        }
    }
}

/// `p` is `base` or inside it.
fn within(p: &str, base: &str) -> bool {
    let base = base.trim_end_matches('/');
    p == base || p.starts_with(&format!("{base}/"))
}

/// Why an existing folder cannot be adopted: adopting makes the folder itself
/// `root:rynas-pcbackup 0750`, so a share's folder (or one holding shares), a user's home (or
/// one holding homes) and system folders are refused.
pub fn adopt_refusal(path: &str, shares: &[String], homes: &[String]) -> Option<String> {
    if let Err(m) = receive::valid_path(path) {
        return Some(m);
    }
    for sys in ["/var", "/home", "/opt", "/tmp", "/srv/.snapshots", "/mnt/.snapshots"] {
        if within(path, sys) {
            return Some(format!("{sys} is a system folder"));
        }
    }
    if let Some(sh) = shares.iter().find(|s| within(s, path)) {
        return Some(format!("{sh} is a shared folder: choose a folder inside it"));
    }
    if let Some(h) = homes.iter().find(|h| within(h, path)) {
        return Some(format!("{h} is a user's home"));
    }
    None
}

/// Every hour (first after two minutes): the retention passes of the devices whose retention
/// the NAS owns, then the alerts.
pub fn spawn_watch(svc: Arc<PcBackup>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(120)).await;
        loop {
            {
                let _g = svc.change.lock().await;
                let s = svc.clone();
                let _ = tokio::task::spawn_blocking(move || s.prune_all()).await;
            }
            let s = svc.clone();
            let _ = tokio::task::spawn_blocking(move || s.check_freshness()).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

// ------------------------------------------------------------------ routes

fn pb(st: &PbState) -> Res<Arc<PcBackup>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "PC backup is not enabled in this rynasd".into(),
        )
    })
}

fn audit(who: &str, action: &str, target: &str) {
    tracing::info!(target: "rynas::audit", "pc backup {action} {target} by {who}");
}

fn started(id: String) -> Response {
    (StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Res<T> {
    tokio::task::spawn_blocking(f).await.map_err(Error::internal)
}

pub fn router(st: PbState) -> Router {
    let admins = st.as_ref().map(|s| s.admins.clone()).unwrap_or_default();
    Router::new()
        .route("/v1/pc-backups/setup", get(setup_get))
        .route("/v1/pc-backups/plan/adopt", post(adopt_plan))
        .route("/v1/pc-backups/targets", get(list_get).post(create_post))
        .route(
            "/v1/pc-backups/targets/{device}",
            get(detail_get).put(update_put).delete(remove_delete),
        )
        .route("/v1/pc-backups/targets/{device}/usage", post(usage_post))
        .route("/v1/pc-backups/targets/{device}/browse", get(browse_get))
        .route("/v1/pc-backups/targets/{device}/copy-out", post(copy_out_post))
        .route("/v1/pc-backups/plan/retention", post(retention_plan_post))
        .route("/v1/pc-backups/targets/{device}/prune", post(prune_post))
        .route("/v1/pc-backups/targets/{device}/sizes", post(sizes_post))
        .route(
            "/v1/pc-backups/targets/{device}/snapshots/{name}",
            axum::routing::delete(snapshot_delete),
        )
        .with_state(st)
        .route_layer(axum::middleware::from_fn_with_state(
            crate::roles::AdminGate {
                policy: admins,
                what: "PC backup",
            },
            crate::roles::admin_only,
        ))
}

async fn setup_get(State(st): State<PbState>) -> Res<Json<ReceiveSetup>> {
    let s = pb(&st)?;
    Ok(Json(blocking(move || s.setup()).await?))
}

async fn list_get(State(st): State<PbState>) -> Res<Json<Vec<PcTarget>>> {
    let s = pb(&st)?;
    Ok(Json(blocking(move || s.list()).await?))
}

#[derive(Deserialize)]
struct DetailQuery {
    /// The name or address the PC reaches the NAS by (the browser's, by default).
    #[serde(default)]
    host: String,
}

/// A host name or address for the snippets: `[A-Za-z0-9.:-]`, or the NAS name.
fn snippet_host(asked: &str, fallback: &str) -> String {
    let ok = !asked.is_empty()
        && asked.len() <= 253
        && asked.bytes().all(|b| b.is_ascii_alphanumeric() || b".:-[]".contains(&b));
    if ok { asked.to_string() } else { fallback.to_string() }
}

async fn detail_get(
    State(st): State<PbState>,
    UrlPath(device): UrlPath<String>,
    q: Result<Query<DetailQuery>, axum::extract::rejection::QueryRejection>,
) -> Res<Json<PcTargetDetail>> {
    let s = pb(&st)?;
    let Query(q) = q.map_err(|e| Error::bad_request(e.body_text()))?;
    let t = s.stored(&device)?;
    Ok(Json(
        blocking(move || {
            let v = s.view(&device, &t, rynas_tasks::now_ms());
            let host = snippet_host(&q.host, &s.hostname);
            let port = receive::sshd_port(&s.host.facts().sshd_config);
            let key_line = receive::parse_public_key(&t.public_key)
                .ok()
                .map(|(k, b)| receive::key_line(&device, &t.path, &t.key_policy(), &k, &b));
            let nas = t.retention.owner == RetentionOwner::Nas;
            PcTargetDetail {
                key_line,
                key_commands: receive::client_key_commands(&device),
                client_config: receive::client_config(&device, &host, port, &t.path, nas),
                agent_setup: receive::agent_setup_command(&device, &host, port, &t.path, nas),
                test_commands: receive::client_test_commands(&host, port),
                target: v.target,
                list: v.list,
                runs: v.runs,
                sizes_from_quota: v.quota,
            }
        })
        .await?,
    ))
}

fn check_key(k: &str) -> Res<()> {
    if k.trim().is_empty() {
        return Ok(());
    }
    receive::parse_public_key(k)
        .map(|_| ())
        .map_err(|m| Error::invalid("public_key", m))
}

fn check_policy(alert_days: u32, d: &DeletePolicy) -> Res<()> {
    if alert_days > 365 {
        return Err(Error::invalid("alert_days", "at most 365 days"));
    }
    if d.keep_last > 1000 {
        return Err(Error::invalid("delete.keep_last", "at most 1000"));
    }
    if d.min_age_days > 3650 {
        return Err(Error::invalid("delete.min_age_days", "at most 3650 days"));
    }
    Ok(())
}

/// The settings added for NAS-side retention and alerts.
fn check_more(alert_hours: u32, retention: Option<&PcRetention>, ignored: Option<&[String]>) -> Res<()> {
    if alert_hours > 24 * 365 {
        return Err(Error::invalid("alert_hours", "at most 8760 hours"));
    }
    if let Some(r) = retention {
        let g = &r.gfs;
        if !rynas_btrfs::btrbk::valid_min(&g.min) {
            return Err(Error::invalid("retention.gfs.min", "all, latest or <n>h|d|w|m|y"));
        }
        for (n, f) in [(g.hourly, "hourly"), (g.daily, "daily"), (g.weekly, "weekly"), (g.monthly, "monthly"), (g.yearly, "yearly")] {
            if n > 1000 {
                return Err(Error::invalid(format!("retention.gfs.{f}"), "at most 1000"));
            }
        }
    }
    if let Some(ig) = ignored {
        let ok = |g: &String| !g.is_empty() && g.len() <= 128 && !g.contains(['/', '\n', '\0']);
        if ig.len() > 64 || !ig.iter().all(ok) {
            return Err(Error::invalid("ignored_groups", "at most 64 subvolume names"));
        }
    }
    Ok(())
}

async fn adopt_plan(
    State(st): State<PbState>,
    body: Result<Json<AdoptRequest>, JsonRejection>,
) -> Res<Json<AdoptPreview>> {
    let s = pb(&st)?;
    let Json(req) = body?;
    receive::valid_path(&req.path).map_err(|m| Error::invalid("path", m))?;
    let path = req.path.clone();
    let (refused, scan, change) = blocking(move || {
        let p = Path::new(&path);
        (s.adopt_refused(&path), s.host.scan(p), s.permission_change(p))
    })
    .await?;
    if let Some(m) = refused {
        return Err(Error::invalid("path", m));
    }
    let Some((sub, entries, other)) = scan.map_err(|m| Error::invalid("path", m))? else {
        return Err(Error::invalid("path", "no such folder").field("path"));
    };
    let sum = receive::summarize(&entries, &DeletePolicy::default(), rynas_tasks::now_ms());
    Ok(Json(AdoptPreview {
        suggested_device: receive::device_from_path(&req.path),
        path: req.path,
        subvolume: sub,
        list: sum.list,
        groups: sum.groups,
        other_entries: other,
        permission_change: change,
    }))
}

#[derive(Deserialize, Default)]
struct CreateQuery {
    /// The adopt plan's `permission_change` was shown and accepted.
    #[serde(default)]
    confirm_permission_change: bool,
}

async fn create_post(
    State(st): State<PbState>,
    hd: HeaderMap,
    q: Result<Query<CreateQuery>, axum::extract::rejection::QueryRejection>,
    body: Result<Json<PcTargetCreate>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (pb(&st)?, user(&hd)?);
    let Query(q) = q.map_err(|e| Error::bad_request(e.body_text()))?;
    let Json(req) = body?;
    if !valid_device(&req.device) {
        return Err(Error::invalid("device", "lowercase letters, digits and -, at most 32"));
    }
    if req.description.chars().count() > 120 {
        return Err(Error::invalid("description", "at most 120 characters"));
    }
    receive::valid_path(&req.path).map_err(|m| Error::invalid("path", m))?;
    check_key(&req.public_key)?;
    check_policy(req.alert_days, &req.delete)?;
    check_more(req.alert_hours, Some(&req.retention), None)?;
    // early answers; checked again under the change lock in the task
    s.clash(&req.device, &req.path)?;
    if req.adopt {
        let (s2, path) = (s.clone(), req.path.clone());
        let (refused, change) = blocking(move || {
            (s2.adopt_refused(&path), s2.permission_change(Path::new(&path)))
        })
        .await?;
        if let Some(m) = refused {
            return Err(Error::invalid("path", m));
        }
        if change.is_some() && !q.confirm_permission_change {
            return Err(Error::invalid(
                "path",
                "adopting changes the folder's owner and mode: confirm the change shown by the plan",
            ));
        }
    }
    let s2 = s.clone();
    let who2 = who.clone();
    let id = s.ops.tasks.spawn("pcbackup.create", &who, move |ctx| async move {
        let (s, who) = (s2, who2);
        let _g = s.change.lock().await;
        let s3 = s.clone();
        let req2 = req.clone();
        let log = tokio::task::spawn_blocking(move || -> Result<Vec<String>, String> {
            let (s, req) = (s3, req2);
            s.clash(&req.device, &req.path).map_err(|e| e.2)?;
            let f = s.host.facts();
            let btrfs = f.btrfs.ok_or("btrfs-progs is not installed")?;
            let mut roots = s.roots();
            roots.push(req.path.clone());
            let mut log = s.host.ensure_account(&btrfs, &roots)?;
            let path = Path::new(&req.path);
            match (s.host.scan(path)?, req.adopt) {
                (Some((_, entries, _)), true) => {
                    if let Some(m) = s.adopt_refused(&req.path) {
                        return Err(format!("{}: {m}", req.path));
                    }
                    if let Some(c) = s.permission_change(path) {
                        log.push(format!(
                            "{}: owner {}:{} mode {:o} -> root:{ACCOUNT} 750",
                            req.path, c.uid, c.gid, c.mode
                        ));
                    }
                    s.host.prepare_adopted(path)?;
                    log.push(format!("{} adopted as it is: {} snapshots", req.path, entries.len()));
                }
                (None, true) => return Err(format!("{}: no such folder", req.path)),
                (Some(_), false) => {
                    return Err(format!("{} already exists: adopt it instead", req.path));
                }
                (None, false) => {
                    s.host.create_folder(path)?;
                    log.push(format!("subvolume {} created", req.path));
                }
            }
            s.edit(|r| {
                r.targets.insert(
                    req.device.clone(),
                    Stored {
                        description: req.description.trim().to_string(),
                        path: req.path.clone(),
                        public_key: req.public_key.trim().to_string(),
                        alert_days: req.alert_days,
                        delete: req.delete,
                        adopted: req.adopt,
                        created_at_ms: rynas_tasks::now_ms(),
                        usage: None,
                        alerted: false,
                        alert_hours: req.alert_hours,
                        retention: req.retention.clone(),
                        ..Default::default()
                    },
                );
            })?;
            s.host.write_keys(&s.keys_text())?;
            log.push("keys of the account written".into());
            Ok(log)
        })
        .await
        .map_err(|e| e.to_string())??;
        for l in log {
            ctx.log(&l);
        }
        audit(&who, if req.adopt { "adopt" } else { "create" }, &req.device);
        Ok(())
    })?;
    Ok(started(id))
}

async fn update_put(
    State(st): State<PbState>,
    hd: HeaderMap,
    UrlPath(device): UrlPath<String>,
    body: Result<Json<PcTargetUpdate>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (pb(&st)?, user(&hd)?);
    let Json(req) = body?;
    s.stored(&device)?;
    if req.description.chars().count() > 120 {
        return Err(Error::invalid("description", "at most 120 characters"));
    }
    check_key(&req.public_key)?;
    check_policy(req.alert_days, &req.delete)?;
    check_more(req.alert_hours, req.retention.as_ref(), req.ignored_groups.as_deref())?;
    let s2 = s.clone();
    let who2 = who.clone();
    let id = s.ops.tasks.spawn("pcbackup.update", &who, move |ctx| async move {
        let (s, who) = (s2, who2);
        let _g = s.change.lock().await;
        let s3 = s.clone();
        let dev = device.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            s3.edit(|r| {
                if let Some(t) = r.targets.get_mut(&dev) {
                    t.description = req.description.trim().to_string();
                    t.public_key = req.public_key.trim().to_string();
                    t.alert_days = req.alert_days;
                    t.delete = req.delete;
                    t.alert_hours = req.alert_hours;
                    if let Some(r) = &req.retention {
                        t.retention = r.clone();
                    }
                    if let Some(g) = &req.ignored_groups {
                        t.ignored_groups = g.clone();
                    }
                }
            })?;
            s3.host.write_keys(&s3.keys_text())
        })
        .await
        .map_err(|e| e.to_string())??;
        ctx.log("settings saved, keys of the account written");
        audit(&who, "update", &device);
        Ok(())
    })?;
    Ok(started(id))
}

async fn remove_delete(
    State(st): State<PbState>,
    hd: HeaderMap,
    UrlPath(device): UrlPath<String>,
) -> Res<Response> {
    let (s, who) = (pb(&st)?, user(&hd)?);
    let t = s.stored(&device)?;
    let s2 = s.clone();
    let who2 = who.clone();
    let id = s.ops.tasks.spawn("pcbackup.remove", &who, move |ctx| async move {
        let (s, who) = (s2, who2);
        let _g = s.change.lock().await;
        let s3 = s.clone();
        let dev = device.clone();
        let path = t.path.clone();
        let log = tokio::task::spawn_blocking(move || -> Result<Vec<String>, String> {
            s3.edit(|r| {
                r.targets.remove(&dev);
            })?;
            let roots = s3.roots();
            let mut log = Vec::new();
            if roots.is_empty() {
                // the last device: no account, sudoers or keys left behind
                log.extend(s3.host.remove_account()?);
            } else {
                s3.host.write_keys(&s3.keys_text())?;
                if let Some(btrfs) = s3.host.facts().btrfs {
                    log.extend(s3.host.ensure_account(&btrfs, &roots)?);
                }
            }
            if let Err(e) = s3.host.release_folder(Path::new(&path)) {
                log.push(format!("{path}: group not changed back: {e}"));
            }
            Ok(log)
        })
        .await
        .map_err(|e| e.to_string())??;
        for l in log {
            ctx.log(&l);
        }
        ctx.log(&format!(
            "{device}: key removed; the folder {} and its snapshots stay",
            t.path
        ));
        if t.alerted {
            s.notifier.emit(resolved(
                NotifyCategory::Backup,
                "pcbackup",
                format!("pcbackup:late:{device}"),
                format!("{device} removed"),
            ));
        }
        for k in t.open_alerts.keys() {
            s.notifier.emit(resolved(
                NotifyCategory::Backup,
                "pcbackup",
                format!("pcbackup:{k}:{device}"),
                format!("{device} removed"),
            ));
        }
        audit(&who, "remove", &device);
        Ok(())
    })?;
    Ok(started(id))
}

async fn usage_post(
    State(st): State<PbState>,
    hd: HeaderMap,
    UrlPath(device): UrlPath<String>,
) -> Res<Response> {
    let (s, who) = (pb(&st)?, user(&hd)?);
    let t = s.stored(&device)?;
    let s2 = s.clone();
    let id = s.ops.tasks.spawn("pcbackup.usage", &who, move |ctx| async move {
        let s = s2;
        let s3 = s.clone();
        let path = t.path.clone();
        let (total, excl, shared) = tokio::task::spawn_blocking(move || s3.host.usage(Path::new(&path)))
            .await
            .map_err(|e| e.to_string())??;
        let u = ReceiveUsage {
            total_bytes: total,
            exclusive_bytes: excl,
            shared_bytes: shared,
            read_at_ms: rynas_tasks::now_ms(),
        };
        s.edit(|r| {
            if let Some(x) = r.targets.get_mut(&device) {
                x.usage = Some(u);
            }
        })?;
        ctx.log(&format!("{device}: {total} bytes in all, {excl} only in these snapshots"));
        Ok(())
    })?;
    Ok(started(id))
}

#[derive(Deserialize)]
struct BrowseQuery {
    snapshot: String,
    #[serde(default)]
    path: String,
}

/// The path of snapshot `name` of a device (a direct child with a btrbk name).
fn snapshot_path(t: &Stored, name: &str) -> Res<PathBuf> {
    if name.contains('/') || rynas_btrfs::name::split_snapshot_name(name).is_none() {
        return Err(Error::invalid("snapshot", "not a snapshot of this device"));
    }
    Ok(Path::new(&t.path).join(name))
}

async fn browse_get(
    State(st): State<PbState>,
    UrlPath(device): UrlPath<String>,
    q: Result<Query<BrowseQuery>, axum::extract::rejection::QueryRejection>,
) -> Res<Json<SnapshotListing>> {
    let s = pb(&st)?;
    let Query(q) = q.map_err(|e| Error::bad_request(e.body_text()))?;
    let t = s.stored(&device)?;
    let snap = snapshot_path(&t, &q.snapshot)?;
    let path = q.path.clone();
    let (entries, truncated) = blocking(move || s.host.browse(&snap, &path)).await??;
    let path = rynas_btrfs::browse::components(&q.path)?.join("/");
    Ok(Json(SnapshotListing {
        id: q.snapshot,
        path,
        entries,
        truncated,
    }))
}

/// A folder to copy into: absolute, normalized, not a system folder (spaces are fine: no
/// shell ever sees it).
fn valid_dest(p: &str) -> Result<(), &'static str> {
    let parts: Vec<&str> = p.strip_prefix('/').map(|r| r.split('/').collect()).unwrap_or_default();
    if parts.len() < 2 || p.len() > 1024 || p.contains(['\0', '\n']) {
        return Err("an absolute folder inside a volume, such as /volume1/restore");
    }
    if parts.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return Err("no empty, . or .. parts and no trailing /");
    }
    let sys = ["etc", "usr", "bin", "sbin", "lib", "boot", "proc", "sys", "dev", "run", "root"];
    if sys.contains(&parts[0]) || p.starts_with("/var/lib/rynas") {
        return Err("not inside a system folder");
    }
    Ok(())
}

async fn copy_out_post(
    State(st): State<PbState>,
    hd: HeaderMap,
    UrlPath(device): UrlPath<String>,
    body: Result<Json<CopyOutRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (pb(&st)?, user(&hd)?);
    let Json(req) = body?;
    let t = s.stored(&device)?;
    let snap = snapshot_path(&t, &req.snapshot)?;
    if rynas_btrfs::browse::components(&req.path)?.is_empty() {
        return Err(Error::invalid("path", "choose a file or folder inside the snapshot"));
    }
    let dest = req.destination.trim().to_string();
    valid_dest(&dest).map_err(|m| Error::invalid("destination", m))?;
    if dest == t.path || dest.starts_with(&format!("{}/", t.path)) {
        return Err(Error::invalid("destination", "a folder outside the device's backups"));
    }
    let s2 = s.clone();
    let who2 = who.clone();
    let id = s.ops.tasks.spawn("pcbackup.copy_out", &who, move |ctx| async move {
        let (s, who) = (s2, who2);
        let ctx2 = ctx.clone();
        let rel = req.path.clone();
        let d = dest.clone();
        let line = tokio::task::spawn_blocking(move || {
            s.host.copy_out(&snap, &rel, Path::new(&d), &|| ctx2.is_cancelled())
        })
        .await
        .map_err(|e| e.to_string())??;
        ctx.log(&line);
        audit(&who, "copy_out", &format!("{device}/{}:{} -> {dest}", req.snapshot, req.path));
        Ok(())
    })?;
    Ok(started(id))
}

/// What the NAS would delete now with these retention settings (read only).
async fn retention_plan_post(
    State(st): State<PbState>,
    body: Result<Json<PcRetentionPlanRequest>, JsonRejection>,
) -> Res<Json<PcRetentionPreview>> {
    let s = pb(&st)?;
    let Json(req) = body?;
    let t = s.stored(&req.device).map_err(|e| e.field("device"))?;
    check_policy(0, &req.delete)?;
    check_more(0, Some(&req.retention), None)?;
    let p = blocking(move || s.preview(&t, &req.retention, &req.delete)).await?;
    Ok(Json(p.map_err(Error::internal)?))
}

fn not_nas_owned(device: &str) -> Error {
    Error::new(
        StatusCode::CONFLICT,
        error_codes::CONFLICT,
        format!("{device}: the PC deletes its old snapshots; set the NAS to own retention first"),
    )
}

/// A retention pass now (the hourly one does the same).
async fn prune_post(State(st): State<PbState>, hd: HeaderMap, UrlPath(device): UrlPath<String>) -> Res<Response> {
    let (s, who) = (pb(&st)?, user(&hd)?);
    if s.stored(&device)?.retention.owner != RetentionOwner::Nas {
        return Err(not_nas_owned(&device));
    }
    let s2 = s.clone();
    let who2 = who.clone();
    let id = s.ops.tasks.spawn("pcbackup.prune", &who, move |ctx| async move {
        let (s, who) = (s2, who2);
        let _g = s.change.lock().await;
        let ctx2 = ctx.clone();
        let dev = device.clone();
        let s3 = s.clone();
        let run = tokio::task::spawn_blocking(move || s3.prune(&dev, &|l| ctx2.log(l)))
            .await
            .map_err(|e| e.to_string())??;
        audit(&who, "prune", &device);
        if let Some(e) = run.error {
            return Err(e);
        }
        ctx.log(&format!("{device}: {} snapshots deleted", run.deleted));
        Ok(())
    })?;
    Ok(started(id))
}

#[derive(Deserialize, Default)]
struct SizesQuery {
    /// Measure snapshots measured before too (their exclusive size grows as others go).
    #[serde(default)]
    again: bool,
}

/// Measures the snapshots without a known size, one at a time at idle priority.
async fn sizes_post(
    State(st): State<PbState>,
    hd: HeaderMap,
    UrlPath(device): UrlPath<String>,
    q: Result<Query<SizesQuery>, axum::extract::rejection::QueryRejection>,
) -> Res<Response> {
    let (s, who) = (pb(&st)?, user(&hd)?);
    let Query(q) = q.map_err(|e| Error::bad_request(e.body_text()))?;
    let t = s.stored(&device)?;
    let s2 = s.clone();
    let id = s.ops.tasks.spawn("pcbackup.sizes", &who, move |ctx| async move {
        let s = s2;
        let ctx2 = ctx.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let root = Path::new(&t.path);
            let Some((_, entries, _)) = s.host.scan(root)? else {
                return Err(format!("{}: no such folder", t.path));
            };
            if s.host.quota_sizes(root, &entries).is_some() {
                ctx2.log("quota groups are on for this volume: the sizes are read live, nothing to measure");
                return Ok(());
            }
            let present: BTreeSet<String> = entries.iter().filter_map(|e| e.info.as_ref()?.uuid.clone()).collect();
            let todo: Vec<(&Entry, String)> = entries
                .iter()
                .filter(|e| !e.incomplete())
                .filter_map(|e| Some((e, e.info.as_ref()?.uuid.clone()?)))
                .filter(|(_, u)| q.again || !t.sizes.contains_key(u))
                .collect();
            let n = todo.len().max(1);
            for (i, (e, uuid)) in todo.iter().enumerate() {
                if ctx2.is_cancelled() {
                    return Err("cancelled".into());
                }
                let (r, x) = s.host.measure(&root.join(&e.name))?;
                let size = SnapshotSize { referenced_bytes: r, exclusive_bytes: x, read_at_ms: rynas_tasks::now_ms(), from_quota: false };
                s.edit(|reg| {
                    if let Some(d) = reg.targets.get_mut(&device) {
                        d.sizes.insert(uuid.clone(), size);
                    }
                })?;
                ctx2.log(&format!("{}: {r} bytes referenced, {x} only in it", e.name));
                ctx2.progress(((i + 1) * 100 / n) as u8);
            }
            // sizes of snapshots that are gone
            s.edit(|reg| {
                if let Some(d) = reg.targets.get_mut(&device) {
                    d.sizes.retain(|u, _| present.contains(u));
                }
            })?;
            if todo.is_empty() {
                ctx2.log("every snapshot already has a size");
            }
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())??;
        Ok(())
    })?;
    Ok(started(id))
}

/// Why an administrator may not delete snapshot `name` now.
fn manual_delete_refusal(entries: &[Entry], name: &str, device: &str, receiving: bool) -> Option<Error> {
    let Some(e) = entries.iter().find(|e| e.name == name && e.info.is_some()) else {
        return Some(Error::not_found(format!("no snapshot {name}")));
    };
    if receive::parents(entries, rynas_tasks::now_ms()).contains(name) {
        return Some(
            Error::new(
                StatusCode::CONFLICT,
                error_codes::CONFLICT,
                format!("{name} is the parent of the next incremental send of {device}"),
            )
            .field("snapshot"),
        );
    }
    if e.incomplete() && receiving {
        return Some(Error::new(StatusCode::CONFLICT, error_codes::BUSY, "a receive is running".into()));
    }
    None
}

/// Deletes one snapshot by hand (an interrupted receive, or an old snapshot). The parent of
/// the PC's next incremental send is refused (409): deleting it would force a full send.
async fn snapshot_delete(
    State(st): State<PbState>,
    hd: HeaderMap,
    UrlPath((device, name)): UrlPath<(String, String)>,
) -> Res<Response> {
    let (s, who) = (pb(&st)?, user(&hd)?);
    let t = s.stored(&device)?;
    snapshot_path(&t, &name)?;
    let (s3, path) = (s.clone(), t.path.clone());
    let early = blocking(move || {
        let root = Path::new(&path);
        s3.host.scan(root).map(|x| (x.map(|(_, e, _)| e).unwrap_or_default(), s3.host.receiving(root)))
    })
    .await?
    .map_err(Error::internal)?;
    if let Some(e) = manual_delete_refusal(&early.0, &name, &device, early.1) {
        return Err(e);
    }
    let s2 = s.clone();
    let who2 = who.clone();
    let id = s.ops.tasks.spawn("pcbackup.delete_snapshot", &who, move |ctx| async move {
        let (s, who) = (s2, who2);
        let _g = s.change.lock().await;
        let (s3, n3, d3, path) = (s.clone(), name.clone(), device.clone(), t.path.clone());
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let root = Path::new(&path);
            let lock = s3.host.lock(root)?.ok_or("a receive is running: try again when it ends")?;
            let (_, entries, _) = s3.host.scan(root)?.ok_or("the folder is gone")?;
            if let Some(e) = manual_delete_refusal(&entries, &n3, &d3, false) {
                return Err(e.2);
            }
            s3.host.delete_snapshot(&lock, &n3)
        })
        .await
        .map_err(|e| e.to_string())??;
        ctx.log(&format!("{device}: {name} deleted"));
        audit(&who, "delete_snapshot", &format!("{device}/{name}"));
        Ok(())
    })?;
    Ok(started(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounts() {
        let mi = "\
29 1 0:26 /@ / rw - btrfs /dev/sda2 rw\n\
30 29 0:27 / /volume1 rw,relatime - btrfs /dev/md2 rw\n\
31 29 0:28 / /boot rw - vfat /dev/sda1 rw\n\
32 29 0:26 /@home /home rw - btrfs /dev/sda2 rw\n\
33 29 0:29 / /srv/data rw - btrfs /dev/md3 rw\n\
34 29 0:30 / /mnt/usb\\040disk rw - btrfs /dev/sdc1 rw\n";
        // the btrfs root offers the system volume's folder (D-ROOTVOL)
        assert_eq!(btrfs_mounts(mi), ["/srv/data", "/volume1", "/srv/rynas/system"]);
        assert_eq!(btrfs_mounts("1 0 0:1 / / rw - ext4 /dev/sda2 rw\n"), Vec::<String>::new());
    }

    fn entry(group: &str, at_s: i64, done: bool) -> Entry {
        Entry {
            name: format!("{group}.{}", rynas_btrfs::name::long_timestamp(at_s)),
            info: Some(rynas_btrfs::ioctl::SubvolInfo {
                readonly: done,
                received_uuid: done.then(|| "6f1c2a8e-0d3b-4c5e-9a7f-000000000001".into()),
                uuid: Some(format!("u-{group}-{at_s}")),
                otime: at_s,
                ..Default::default()
            }),
        }
    }

    fn now_s() -> i64 {
        (rynas_tasks::now_ms() / 1000) as i64
    }

    fn keep_of(plan: &[RetentionItem], name: &str) -> (bool, RetentionReason) {
        let i = plan.iter().find(|i| i.name == name).unwrap_or_else(|| panic!("{name} not in the plan"));
        (i.keep, i.reason)
    }

    #[test]
    fn retention_keeps_the_parent_the_floor_and_the_counts() {
        let now = now_s();
        let day = 86_400;
        // one snapshot a day for 60 days, newest first as scanned
        let mut e: Vec<Entry> = (1..=60).map(|d| entry("home", now - d * day, true)).collect();
        e.insert(0, entry("root", now - 50 * day, true));
        let gfs = Retention { preset: None, hourly: 0, daily: 7, weekly: 0, monthly: 0, yearly: 0, min: "latest".into() };
        let floor = DeletePolicy { allowed: false, keep_last: 0, min_age_days: 0 };
        let plan = retention_plan(&e, &gfs, &floor, false, now as u64 * 1000);
        // the newest of each subvolume is the parent, however old (root only has one)
        assert_eq!(keep_of(&plan, &e[1].name), (true, RetentionReason::Parent));
        assert_eq!(keep_of(&plan, &e[0].name), (true, RetentionReason::Parent));
        assert_eq!(keep_of(&plan, &e[3].name), (true, RetentionReason::Daily));
        assert_eq!(keep_of(&plan, &e[30].name), (false, RetentionReason::Expired));
        let kept = plan.iter().filter(|i| i.keep).count();
        assert!((7..=9).contains(&kept), "{kept} kept");
        // a floor of 100 days keeps everything
        let floor = DeletePolicy { allowed: true, keep_last: 1, min_age_days: 100 };
        assert!(retention_plan(&e, &gfs, &floor, false, now as u64 * 1000).iter().all(|i| i.keep));
        // min "all" keeps everything too; monthly counts keep the first of each month
        let all = Retention { min: "all".into(), ..gfs.clone() };
        assert!(retention_plan(&e, &all, &DeletePolicy::default(), false, now as u64 * 1000).iter().all(|i| i.keep));
        let monthly = Retention { daily: 0, monthly: 12, ..gfs };
        let plan = retention_plan(&e, &monthly, &DeletePolicy::default(), false, now as u64 * 1000);
        let months = plan.iter().filter(|i| i.reason == RetentionReason::Monthly).count();
        assert!((1..=3).contains(&months), "{months}");
    }

    #[test]
    fn retention_and_interrupted_or_future_snapshots() {
        let now = now_s();
        let e = vec![
            entry("home", now + 400 * 86_400, true),
            entry("home", now - 120, false),
            entry("home", now - 7200, false),
            entry("home", now - 86_400, true),
            entry("home", now - 9 * 86_400, true),
        ];
        let gfs = Retention { preset: None, hourly: 0, daily: 1, weekly: 0, monthly: 0, yearly: 0, min: "latest".into() };
        let floor = DeletePolicy::default();
        let plan = retention_plan(&e, &gfs, &floor, false, now as u64 * 1000);
        // a name in the future stays and is not the parent: the real newest is
        assert_eq!(keep_of(&plan, &e[0].name), (true, RetentionReason::Future));
        assert_eq!(keep_of(&plan, &e[3].name), (true, RetentionReason::Parent));
        // an interrupted receive goes after an hour, not before, and never while receiving
        assert_eq!(keep_of(&plan, &e[1].name), (true, RetentionReason::Interrupted));
        assert_eq!(keep_of(&plan, &e[2].name), (false, RetentionReason::Interrupted));
        assert_eq!(keep_of(&plan, &e[4].name), (false, RetentionReason::Expired));
        let busy = retention_plan(&e, &gfs, &floor, true, now as u64 * 1000);
        assert_eq!(keep_of(&busy, &e[2].name), (true, RetentionReason::Interrupted));
    }

    fn listed(e: &[Entry]) -> Vec<ReceivedSnapshot> {
        receive::summarize(e, &DeletePolicy::default(), rynas_tasks::now_ms()).list
    }

    #[test]
    fn runs_group_by_timestamp_and_name_what_is_missing() {
        let t = now_s() - 3 * 3600;
        let e = vec![entry("home", t, true), entry("root", t, true), entry("home", t - 86_400, true), entry("root", t - 86_400, false)];
        let mut l = listed(&e);
        for s in l.iter_mut() {
            s.size = Some(SnapshotSize { referenced_bytes: 10, exclusive_bytes: 1, ..Default::default() });
        }
        let expected = BTreeSet::from(["home".to_string(), "root".to_string()]);
        let r = runs(&l, &expected);
        assert_eq!(r.len(), 2);
        assert!(r[0].lacks.is_empty() && !r[0].incomplete);
        assert_eq!((r[0].referenced_bytes, r[0].exclusive_bytes), (Some(20), Some(2)));
        assert_eq!(r[1].lacks, ["root"]);
        assert!(r[1].incomplete);
        assert_eq!(run_key("home.20261110T0300_2"), Some("20261110T0300"));
        assert_eq!(run_key("home.20261110_101500"), Some("20261110_101500"));
    }

    #[test]
    fn manifests_and_bare_metal() {
        let good = br#"{"format":1,"written_at_ms":1000,"boot":{"loader":"systemd-boot"},
            "btrbk":{"subvolumes":[{"name":"@","mount":"/"},{"name":"@home","mount":"/home"}]}}"#;
        let m = parse_manifest(good).unwrap();
        assert_eq!(m.subvolumes, ["@", "@home"]);
        assert!(m.root && m.loader.as_deref() == Some("systemd-boot"));
        assert_eq!(parse_manifest(b"{\"format\":2}").unwrap_err(), (BareMetalCode::ManifestUnsupported, Some("2".into())));
        assert_eq!(parse_manifest(b"not json").unwrap_err().0, BareMetalCode::ManifestInvalid);
        assert_eq!(parse_manifest(br#"{"format":1,"btrbk":{"subvolumes":[{"name":"../x"}]}}"#).unwrap_err().0, BareMetalCode::ManifestInvalid);
        let groups = BTreeSet::from(["@".to_string(), "@home".to_string()]);
        let b = bare_metal(&groups, Some(("@.20261110T0300".into(), Some(2000), Ok(m.clone()))), None);
        assert!(b.ready && b.missing.is_empty() && b.warnings.is_empty());
        // a subvolume never received, no loader, written long before the snapshot
        let only_root = BTreeSet::from(["@".to_string()]);
        let old = Manifest { loader: None, ..m };
        let b = bare_metal(&only_root, Some(("@.x".into(), Some(1000 + 30 * DAY_MS), Ok(old))), None);
        assert!(!b.ready);
        let codes: Vec<_> = b.missing.iter().map(|g| (g.code, g.subject.clone())).collect();
        assert_eq!(codes, [(BareMetalCode::SubvolumeMissing, Some("@home".into())), (BareMetalCode::LoaderUnknown, None)]);
        assert_eq!(b.warnings[0].code, BareMetalCode::ManifestStale);
        // no manifest at all (the PC side comes later), no snapshots at all
        assert_eq!(bare_metal(&groups, None, None).missing[0].code, BareMetalCode::NoManifest);
        assert_eq!(bare_metal(&BTreeSet::new(), None, None).missing[0].code, BareMetalCode::NoSnapshots);
    }

    /// The manifest `rynas-agent manifest` writes on a btrbk PC (golden file produced by the
    /// agent's own test from fixture inputs) is what B1 reads.
    #[test]
    fn manifest_written_by_the_agent() {
        let m = parse_manifest(include_bytes!("../tests/golden/pc-manifest-b2.json")).unwrap();
        assert_eq!(m.subvolumes, ["@", "@home"]);
        assert!(m.root);
        assert_eq!(m.loader.as_deref(), Some("systemd_boot"));
        assert_eq!(m.written_at_ms, Some(1_762_905_600_000));
        assert_eq!(m.disks.len(), 1);
        assert_eq!(m.disks[0].table.as_ref().unwrap().partitions.len(), 2);
        assert_eq!(m.layout[1].mount, "/home");
        assert_eq!(m.boot_copies[0].mount, "/boot");
        let groups = BTreeSet::from(["@".to_string(), "@home".to_string()]);
        let b = bare_metal(&groups, Some(("@.20261112T0300".into(), Some(1_762_905_700_000), Ok(m))), None);
        assert!(b.ready, "{:?}", b.missing);
        assert_eq!(b.disks[0].serial, "S123456789");
        assert_eq!(b.layout[0].path, "@");
        // display parts that do not parse are dropped, the manifest stays usable
        let odd = br#"{"format":1,"boot":{"loader":"grub"},"disks":[{"name":5}],
            "btrbk":{"subvolumes":[{"name":"@","mount":"/"}],"boot_copies":"x"}}"#;
        let m = parse_manifest(odd).unwrap();
        assert!(m.disks.is_empty() && m.boot_copies.is_empty() && m.root);
    }

    #[test]
    fn health_issues() {
        let now = now_s();
        let e = vec![entry("home", now - 4000, true), entry("root", now - 3 * 86_400, true), entry("home", now - 2 * 3600, false)];
        let l = listed(&e);
        let expected = BTreeSet::from(["home".to_string(), "root".to_string()]);
        let f = |receiving, alert_hours, space| HealthFacts {
            list: &l,
            expected: &expected,
            alert_hours,
            receiving,
            space,
            full_set_bytes: Some(100 << 30),
            now_ms: now as u64 * 1000,
        };
        let codes = |v: Vec<PcIssue>| v.into_iter().map(|i| (i.code, i.subject)).collect::<Vec<_>>();
        let i = codes(issues(&f(false, 24, Some((50 << 30, 1 << 40)))));
        assert_eq!(i[0], (PcIssueCode::SubvolumeLate, Some("root".into())));
        assert_eq!(i[1].0, PcIssueCode::ReceiveFailed);
        assert_eq!(i[2].0, PcIssueCode::LowSpace);
        // receiving for two hours: neither failed nor stuck yet; plenty of space
        let i = codes(issues(&f(true, 24, Some((500 << 30, 1 << 40)))));
        assert_eq!(i, [(PcIssueCode::SubvolumeLate, Some("root".into()))]);
        // the whole PC late: one issue, not one per subvolume; alerts off: none
        let i = codes(issues(&f(true, 1, None)));
        assert_eq!(i, [(PcIssueCode::Late, None)]);
        assert!(issues(&f(true, 0, None)).is_empty());
    }

    #[test]
    fn destinations() {
        assert!(valid_dest("/volume1/Marco Docs/restore").is_ok());
        for bad in ["volume1/x", "/volume1", "/etc/x", "/volume1/../etc", "/volume1/x/", "/var/lib/rynas/x"] {
            assert!(valid_dest(bad).is_err(), "{bad}");
        }
    }
}
