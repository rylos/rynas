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
use rynas_api_types::pcagent::{LEGACY_BOOT_DIR, LEGACY_FILE_MAX, LegacyFile, RestoreBtrfsListing, RestoreLegacy};
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
    /// A new subvolume folder (parents made as needed): `0750 root:<account>`, or with
    /// `agent` (fed by `rynas-agent` over HTTPS, no SSH account) `0700 root:root`.
    fn create_folder(&self, path: &Path, agent: bool) -> Result<(), String>;
    /// An existing folder: only its own owner group and mode change (as `create_folder`).
    fn prepare_adopted(&self, path: &Path, agent: bool) -> Result<(), String>;
    /// `btrfs receive` of the (decompressed) send stream `input` for `device`, chrooted in the
    /// staging folder `<folder>/.incoming/<staging>/` of the locked folder, in a transient
    /// unit that can write only in the folder and does not see the `hidden` folders (the
    /// other PCs'). Memory uses the stream's own subvolume name. Blocking; returns when the
    /// receive has ended.
    fn receive(&self, device: &str, lock: &FolderLock, staging: &str, hidden: &[String], input: &mut dyn std::io::Read) -> Result<(), String>;
    /// Makes the empty staging folder `<folder>/.incoming/<staging>/` (`0700 root`).
    fn make_staging(&self, lock: &FolderLock, staging: &str) -> Result<(), String>;
    /// Everything directly in `<folder>/.incoming/<staging>/` (subvolume facts for the
    /// subvolumes, `None` for anything else).
    fn staged(&self, lock: &FolderLock, staging: &str) -> Result<Vec<Entry>, String>;
    /// Moves the received read-only subvolume `.incoming/<staging>/<name>` to `<folder>/<name>`
    /// (never replacing; the read-only flag is lifted only for the move: a read-only folder
    /// cannot change parent).
    fn adopt_staged(&self, lock: &FolderLock, staging: &str, name: &str) -> Result<(), String>;
    /// Removes `<folder>/.incoming` with whatever an earlier or failed receive left there
    /// (subvolumes deleted, files and folders removed without following links).
    fn clear_staging(&self, lock: &FolderLock) -> Result<(), String>;
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
    /// Takes the folder's lock **shared** after the same check as [`ReceiveHost::lock`]: a
    /// restore's send only reads, and while it runs no receive or retention pass (they take
    /// it exclusively) can start; `Ok(None)` when one holds it now.
    fn lock_shared(&self, root: &Path) -> Result<Option<FolderLock>, String>;
    /// `btrfs send <folder>/<name>` (the raw full stream) into `out`, for a bare-metal
    /// restore: a complete read-only received subvolume directly in the locked folder. Runs
    /// in a transient unit that can only read and does not see the `hidden` folders
    /// ([`send_unit_properties`]); stops (and the unit with it) as soon as `out` fails or
    /// `stop` says so.
    fn send(&self, device: &str, lock: &FolderLock, name: &str, hidden: &[String], out: &mut dyn std::io::Write, stop: &dyn Fn() -> bool) -> Result<(), String>;
    /// What an older restore script left in root snapshot `name` without a manifest
    /// (descriptor walk, no symlink followed, each file at most [`LEGACY_FILE_MAX`]).
    fn read_legacy(&self, root: &Path, name: &str) -> Result<RestoreLegacy, String>;
}

/// Prefix of the transient unit of a restore's send (`<prefix>-<device>.service`).
pub const SEND_UNIT: &str = "rynas-pcsend";

/// The sandbox of a restore's `btrfs send`: it only reads. The whole system read-only, the
/// other device folders inaccessible, no network, no device, only the capabilities of the
/// send ioctl (`CAP_SYS_ADMIN`) and of reading folders of other owners on the way
/// (`CAP_DAC_READ_SEARCH`), capped CPU, memory and tasks, idle-ish I/O. No `PrivateTmp`: a
/// folder below `/tmp` would vanish (tests on loop images).
pub fn send_unit_properties(hidden: &[String]) -> Vec<String> {
    let mut p = base_unit_properties(hidden);
    p.extend(
        [
            "ProtectHome=read-only",
            "CapabilityBoundingSet=CAP_SYS_ADMIN CAP_DAC_READ_SEARCH",
            "SystemCallFilter=@system-service",
            "RestrictSUIDSGID=yes",
            "MemoryMax=256M",
            "TasksMax=8",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    p
}

/// What every confined btrfs unit of the PC backup has (the agent PCs' receive, a restore's
/// send): the whole system read-only, the other device folders (`hidden`) inaccessible, no
/// network, IPC or device, no new privileges, the kernel's knobs out of reach, capped CPU,
/// idle-ish I/O, at most 48 h (a first full send over a slow link takes long).
fn base_unit_properties(hidden: &[String]) -> Vec<String> {
    let mut p: Vec<String> = [
        "ProtectSystem=strict",
        "PrivateNetwork=yes",
        "PrivateIPC=yes",
        "NoNewPrivileges=yes",
        "RestrictAddressFamilies=AF_UNIX",
        "SystemCallArchitectures=native",
        "DevicePolicy=closed",
        "ProtectKernelModules=yes",
        "ProtectKernelTunables=yes",
        "ProtectKernelLogs=yes",
        "ProtectControlGroups=yes",
        "ProtectClock=yes",
        "ProtectHostname=yes",
        "RestrictNamespaces=yes",
        "RestrictRealtime=yes",
        "LockPersonality=yes",
        "MemoryDenyWriteExecute=yes",
        "UMask=0077",
        "CPUQuota=100%",
        "Nice=10",
        "IOSchedulingClass=best-effort",
        "IOSchedulingPriority=7",
        "RuntimeMaxSec=172800",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // `-`: a folder that is not there is no error
    p.extend(hidden.iter().map(|h| format!("InaccessiblePaths=-{h}")));
    p
}

/// `systemd-run --wait --pipe` of `argv` in the transient unit `unit` with `props`, the
/// environment cleared; stdout and stderr piped, stdin as `stdin` says.
fn confined(unit: &str, desc: &str, props: &[String], argv: &[&std::ffi::OsStr], stdin: std::process::Stdio) -> std::process::Command {
    use std::process::{Command, Stdio};
    let mut c = Command::new("systemd-run");
    c.args(["--wait", "--pipe", "--collect", "--quiet", "--service-type=exec"]).arg(format!("--unit={unit}")).arg(format!("--description={desc}"));
    for p in props {
        c.arg(format!("--property={p}"));
    }
    c.arg("--").args(argv);
    c.env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").env("LC_ALL", "C").stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::piped());
    c
}

/// The first 16 KiB of the child's stderr, read in a thread (the rest drained): join it
/// for the error text.
fn stderr_tail(child: &mut std::process::Child) -> std::thread::JoinHandle<String> {
    use std::io::Read as _;
    let err = child.stderr.take();
    std::thread::spawn(move || {
        let Some(mut err) = err else { return String::new() };
        let mut s = Vec::new();
        let _ = (&mut err).take(16 << 10).read_to_end(&mut s);
        let _ = std::io::copy(&mut err, &mut std::io::sink());
        String::from_utf8_lossy(&s).into_owned()
    })
}

/// The last line of a btrfs tool's stderr worth showing (not its progress chatter).
fn last_line(text: &str) -> String {
    text.lines().rfind(|l| !l.trim().is_empty() && !l.starts_with("At ") && !l.starts_with("Chroot to ")).unwrap_or("").trim().to_string()
}

/// A received snapshot fit to be sent: a read-only subvolume with a received UUID, directly
/// in the folder (`dir`), its name a snapshot name.
fn sendable(dir: &std::fs::File, name: &str) -> Result<(), String> {
    if name.contains('/') || rynas_btrfs::name::split_snapshot_name(name).is_none() {
        return Err(format!("{name}: not a snapshot name"));
    }
    let sub = rynas_btrfs::ioctl::open_dir_at(dir, name).map_err(|e| format!("{name}: {e}"))?;
    if !rynas_btrfs::ioctl::is_subvolume(&sub) {
        return Err(format!("{name}: not a subvolume"));
    }
    let i = rynas_btrfs::ioctl::subvol_info(&sub).map_err(|e| format!("{name}: {e}"))?;
    if !i.readonly || i.received_uuid.is_none() {
        return Err(format!("{name}: not a complete received snapshot"));
    }
    Ok(())
}

/// What [`read_at`] found.
enum SmallFile {
    Missing,
    /// There, but a symlink, not a regular file, or bigger than allowed.
    Refused(String),
    Data(Vec<u8>),
}

/// A regular file `name` in `dir`, never through a symlink, never blocking on a FIFO, at
/// most `max` bytes.
fn read_at(dir: &std::fs::File, name: &str, max: usize) -> Result<SmallFile, String> {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    let c = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
    // SAFETY: openat(2) on a directory descriptor with a C name; never follows a final
    // symlink, never blocks on a FIFO
    let fd = unsafe {
        libc::openat(dir.as_raw_fd(), c.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY)
    };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ENOENT) | Some(libc::ENOTDIR) => Ok(SmallFile::Missing),
            Some(libc::ELOOP) => Ok(SmallFile::Refused(format!("{name}: a symlink"))),
            _ => Err(format!("{name}: {e}")),
        };
    }
    // SAFETY: a descriptor we just opened and own
    let f = unsafe { std::fs::File::from_raw_fd(fd) };
    let m = f.metadata().map_err(|e| e.to_string())?;
    if !m.is_file() || m.len() > max as u64 {
        return Ok(SmallFile::Refused(format!("{name}: not a regular file of at most {max} bytes")));
    }
    let mut buf = Vec::with_capacity(m.len() as usize);
    f.take(max as u64 + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(if buf.len() <= max { SmallFile::Data(buf) } else { SmallFile::Refused(format!("{name}: bigger than {max} bytes")) })
}

/// [`read_at`], anything but a fitting regular file being `None`.
fn read_small_at(dir: &std::fs::File, name: &str, max: usize) -> Result<Option<Vec<u8>>, String> {
    Ok(match read_at(dir, name, max)? {
        SmallFile::Data(b) => Some(b),
        SmallFile::Missing | SmallFile::Refused(_) => None,
    })
}

/// Text from a file of the PC (control characters but tab and newline dropped).
fn legacy_text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect()
}

/// `sfdisk --dump` output of a GPT disk: a `label: gpt` line and partition lines.
pub fn looks_like_sfdisk_dump(text: &str) -> bool {
    let label = text.lines().any(|l| l.trim().replace(' ', "") == "label:gpt");
    let parts = text.lines().any(|l| l.contains(": start=") || l.contains(":start="));
    label && parts
}

/// Largest manifest read from a snapshot.
pub const MANIFEST_MAX: u64 = 1 << 20;

/// Longest display string kept from a manifest (model, serial, paths, file names).
pub const SHOWN_MAX: usize = 256;

/// Prefix of the transient unit of an agent PC's receive (`<prefix>-<device>.service`).
pub const RECEIVE_UNIT: &str = "rynas-pcreceive";

/// Staging folder inside a device folder: every receive lands in a fresh empty folder below
/// it and moves into the device folder only once checked.
pub const STAGING_DIR: &str = ".incoming";

/// Why an agent folder must be a subvolume: inside the receive unit the folder is a bind
/// mount (`ReadWritePaths`), `btrfs receive` resolves parents relative to that mount's
/// subvolume, and with a plain folder it finds none (checked on a loop btrfs).
const AGENT_NOT_SUBVOLUME: &str =
    "the folder is not a btrfs subvolume: rynas-agent's incremental receives need one (make a new folder from rynas, or turn this one into a subvolume)";

/// The capabilities of an agent PC's receive, one per line: subvolume ioctls, chroot,
/// owners, modes on others' files, set-id bits kept across a write (`CAP_FSETID`), others'
/// read-only folders (`CAP_DAC_OVERRIDE`), device nodes. `CAP_DAC_READ_SEARCH` is not needed
/// (checked on a loop: same result without it).
const RECEIVE_CAPS: &[&str] = &[
    "CAP_SYS_ADMIN",
    "CAP_SYS_CHROOT",
    "CAP_CHOWN",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_DAC_OVERRIDE",
    "CAP_MKNOD",
    // file capabilities (`security.capability`, e.g. newgidmap, ping): without it a real
    // system's root snapshot fails with EPERM (seen on Arch in the B4 VMs)
    "CAP_SETFCAP",
];

/// The sandbox of an agent PC's `btrfs receive` (`docs/research/pc-backup.md` §11.9).
///
/// - `--chroot` into the receive's own empty staging folder (`<root>/.incoming/<random>/`):
///   every path of the stream, and every symlink a later command follows, stays there. The
///   other snapshots of the PC are not reachable by path.
/// - What the chroot does not cover: `btrfs receive` opens the mount holding its target
///   *before* it chroots, looks the parent of an incremental stream (and any clone source)
///   up by UUID across the whole filesystem, and opens it through that descriptor. In this
///   unit that mount is the bind mount `ReadWritePaths` makes of the device folder, and
///   btrfs-progs refuses a parent outside that mount's subvolume ("parent subvol is not
///   reachable from inside the root subvol"; 7.3 says the same of clone sources). That
///   limit holds only because the folder is a subvolume and its own mount here (an agent
///   folder must be one, [`AGENT_NOT_SUBVOLUME`]); it is not a promise of btrfs. So the
///   stream guard ([`rynas_btrfs::send_stream`]) admits only the announced parent and the
///   subvolume itself as sources, rynasd checks the received subvolume's `parent_uuid`
///   afterwards, and `hidden` (the other device folders) is made inaccessible in the unit.
/// - `ReadWritePaths` is the device folder, not only the staging folder: a clone needs its
///   source and target on the same mount (`FICLONERANGE` refuses across bind mounts), and
///   the parent lives in the device folder. The rest of the system is read-only.
/// - No network, no device can be opened (`DevicePolicy=closed` with `DeviceAllow=… m`: a
///   stream may create device nodes, as system snapshots hold them, but a write into one is
///   refused by the kernel), the capabilities a receive needs (subvolume ioctls, owners, modes,
///   set-id bits, device nodes, the chroot), the system-service syscalls plus
///   `chroot`/`mknod`; no `RestrictSUIDSGID` (system snapshots carry set-id files). CPU,
///   memory and tasks capped. No `PrivateTmp`/`PrivateDevices`: a folder below `/tmp` would
///   vanish behind the first, and the second removes `CAP_MKNOD`.
///
/// Checked on a loop btrfs (btrfs-progs 7.3, kernel 7.3) with this set: full and incremental
/// receives with clones, a parent and a clone source in another folder refused, a plain
/// folder unable to find its parents, a device node created but not writable.
pub fn receive_unit_properties(root: &str, hidden: &[String]) -> Vec<String> {
    let mut p = base_unit_properties(hidden);
    p.extend([
        format!("ReadWritePaths={root}"),
        format!("CapabilityBoundingSet={}", RECEIVE_CAPS.join(" ")),
        "SystemCallFilter=@system-service chroot mknod mknodat".into(),
        // device nodes may be created (overlayfs whiteouts, container root filesystems) but
        // never opened: a block node written to would be a disk of the NAS
        "DeviceAllow=char-* m".into(),
        "DeviceAllow=block-* m".into(),
        "ProtectProc=invisible".into(),
        "ProcSubset=pid".into(),
        "MemoryMax=512M".into(),
        "TasksMax=16".into(),
    ]);
    p
}

/// The device folder, opened and locked; the lock goes with it.
pub struct FolderLock {
    pub path: PathBuf,
    /// `None` in memory.
    pub dir: Option<std::fs::File>,
}

/// The device folder checked from `/` without symlinks (and still the same folder at its
/// path), then `flock(op | LOCK_NB)` on it; `Ok(None)` when somebody holds it.
fn lock_with(root: &Path, op: libc::c_int) -> Result<Option<FolderLock>, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let dir = rynas_btrfs::receive_filter::verify_root(&root.to_string_lossy())?;
    // the path still names the folder that was checked (nothing swapped in since)
    let (a, b) = (dir.metadata().map_err(|e| e.to_string())?, std::fs::symlink_metadata(root).map_err(|e| e.to_string())?);
    if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
        return Err(format!("{}: changed while it was checked", root.display()));
    }
    // SAFETY: flock(2) on our own descriptor; the lock goes when `dir` closes
    if unsafe { libc::flock(dir.as_raw_fd(), op | libc::LOCK_NB) } != 0 {
        return Ok(None);
    }
    Ok(Some(FolderLock { path: root.to_path_buf(), dir: Some(dir) }))
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

    fn create_folder(&self, path: &Path, agent: bool) -> Result<(), String> {
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
        self.prepare_adopted(path, agent)
    }

    fn prepare_adopted(&self, path: &Path, agent: bool) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        let (gid, mode) = if agent {
            (0, 0o700)
        } else {
            (self.account_gid().ok_or_else(|| format!("group {ACCOUNT} not found"))?, 0o750)
        };
        // on the descriptor: a symlink swapped in for the folder is not followed
        let dir = rynas_btrfs::ioctl::open_dir(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if agent && !rynas_btrfs::ioctl::is_subvolume(&dir) {
            return Err(format!("{}: {AGENT_NOT_SUBVOLUME}", path.display()));
        }
        std::os::unix::fs::fchown(&dir, Some(0), Some(gid)).map_err(|e| format!("{}: {e}", path.display()))?;
        dir.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    fn receive(&self, device: &str, lock: &FolderLock, staging: &str, hidden: &[String], input: &mut dyn std::io::Read) -> Result<(), String> {
        use std::process::Stdio;
        if !valid_device(device) {
            return Err(format!("{device}: not a device name"));
        }
        let btrfs = rynas_btrfs::receive_filter::locate("btrfs").ok_or("btrfs-progs is not installed")?;
        if !lock.dir.as_ref().is_some_and(rynas_btrfs::ioctl::is_subvolume) {
            return Err(AGENT_NOT_SUBVOLUME.into());
        }
        let root = lock.path.to_string_lossy().into_owned();
        let into = format!("{root}/{STAGING_DIR}/{staging}");
        // the folder came through verify_root (no symlink on the way, not inside a received
        // snapshot) and valid_path (no spaces or quotes: safe as a unit property value); the
        // staging name is rynasd's own (hex)
        receive::valid_path(&root)?;
        receive::valid_path(&into)?;
        let hidden: Vec<String> = hidden
            .iter()
            .filter(|h| receive::valid_path(h).is_ok() && !within(&root, h) && !within(h, &root))
            .cloned()
            .collect();
        // one name per device: systemd refuses a second unit while one still runs (even when
        // rynasd restarted and lost the folder lock meanwhile); -e: stop at the end command
        // (the guard lets nothing follow it anyway)
        let target = format!("{into}/");
        let argv: [&std::ffi::OsStr; 5] = [btrfs.as_os_str(), "receive".as_ref(), "-e".as_ref(), "--chroot".as_ref(), target.as_ref()];
        let mut c = confined(&format!("{RECEIVE_UNIT}-{device}"), &format!("rynas: receive of PC {device}"), &receive_unit_properties(&root, &hidden), &argv, Stdio::piped());
        c.stdout(Stdio::null());
        let mut child = c.spawn().map_err(|e| format!("systemd-run: {e}"))?;
        let errs = stderr_tail(&mut child);
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        let copied = std::io::copy(input, &mut stdin);
        // end of stream (or the stream broke: a short stream makes the receive fail)
        drop(stdin);
        let status = child.wait().map_err(|e| e.to_string())?;
        let last = last_line(&errs.join().unwrap_or_default());
        match copied {
            // the input failed first: that is the cause
            Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(format!("the stream broke off: {e}")),
            _ if status.success() => Ok(()),
            _ => Err(if last.is_empty() { format!("btrfs receive failed ({status})") } else { format!("btrfs receive: {last}") }),
        }
    }

    fn make_staging(&self, lock: &FolderLock, staging: &str) -> Result<(), String> {
        let dir = lock.dir.as_ref().ok_or("folder not open")?;
        let top = mkdir_open(dir, STAGING_DIR)?;
        mkdir_open(&top, staging).map(|_| ())
    }

    fn staged(&self, lock: &FolderLock, staging: &str) -> Result<Vec<Entry>, String> {
        use std::os::fd::AsRawFd;
        let dir = lock.dir.as_ref().ok_or("folder not open")?;
        let top = rynas_btrfs::ioctl::open_dir_at(dir, STAGING_DIR).map_err(|e| format!("{STAGING_DIR}: {e}"))?;
        let st = rynas_btrfs::ioctl::open_dir_at(&top, staging).map_err(|e| format!("{staging}: {e}"))?;
        let mut out = Vec::new();
        for e in std::fs::read_dir(format!("/proc/self/fd/{}", st.as_raw_fd())).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            let name = e.file_name().to_string_lossy().into_owned();
            let info = e
                .file_name()
                .to_str()
                .and_then(|n| rynas_btrfs::ioctl::open_dir_at(&st, n).ok())
                .filter(rynas_btrfs::ioctl::is_subvolume)
                .and_then(|f| rynas_btrfs::ioctl::subvol_info(&f).ok());
            out.push(Entry { name, info });
        }
        Ok(out)
    }

    fn adopt_staged(&self, lock: &FolderLock, staging: &str, name: &str) -> Result<(), String> {
        use rynas_btrfs::ioctl;
        let dir = lock.dir.as_ref().ok_or("folder not open")?;
        let top = ioctl::open_dir_at(dir, STAGING_DIR).map_err(|e| format!("{STAGING_DIR}: {e}"))?;
        let st = ioctl::open_dir_at(&top, staging).map_err(|e| format!("{staging}: {e}"))?;
        let sub = ioctl::open_dir_at(&st, name).map_err(|e| format!("{name}: {e}"))?;
        if !ioctl::is_subvolume(&sub) {
            return Err(format!("{name}: not a subvolume"));
        }
        ioctl::set_readonly(&sub, false).map_err(|e| format!("{name}: {e}"))?;
        let moved = ioctl::rename_noreplace_at(&st, name, dir, name).map_err(|e| format!("moving {name}: {e}"));
        // read-only again wherever it is now (the same subvolume, by descriptor)
        let ro = ioctl::set_readonly(&sub, true).map_err(|e| format!("{name}: {e}"));
        moved.and(ro)
    }

    fn clear_staging(&self, lock: &FolderLock) -> Result<(), String> {
        let dir = lock.dir.as_ref().ok_or("folder not open")?;
        remove_tree_at(dir, STAGING_DIR, 0)
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
        use rynas_btrfs::ioctl::{open_dir, open_dir_at};
        let gone = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
        let snap = open_dir_at(&open_dir(root).map_err(|e| e.to_string())?, name).map_err(|e| format!("{name}: {e}"))?;
        let dot = match open_dir_at(&snap, ".rynas") {
            Ok(d) => d,
            Err(e) if gone(&e) => return Ok(None),
            Err(e) => return Err(format!("{name}/.rynas: {e}")),
        };
        match read_at(&dot, "manifest.json", MANIFEST_MAX as usize)? {
            SmallFile::Data(b) => Ok(Some(b)),
            SmallFile::Missing => Ok(None),
            SmallFile::Refused(why) => Err(format!("{name}/.rynas/{why}")),
        }
    }

    fn receiving(&self, root: &Path) -> bool {
        use std::os::fd::AsRawFd;
        let Ok(dir) = rynas_btrfs::ioctl::open_dir(root) else { return false };
        // SAFETY: flock(2) on our own descriptor; a shared lock is refused while the filter
        // holds its exclusive one, and goes when `dir` closes
        unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) != 0 }
    }

    fn lock(&self, root: &Path) -> Result<Option<FolderLock>, String> {
        // the same lock the filter takes per receive
        lock_with(root, libc::LOCK_EX)
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

    fn lock_shared(&self, root: &Path) -> Result<Option<FolderLock>, String> {
        // shared: a receive or a retention pass (exclusive) cannot start while a send reads
        lock_with(root, libc::LOCK_SH)
    }

    fn send(&self, device: &str, lock: &FolderLock, name: &str, hidden: &[String], out: &mut dyn std::io::Write, stop: &dyn Fn() -> bool) -> Result<(), String> {
        use std::io::Read as _;
        use std::process::{Command, Stdio};
        if !valid_device(device) {
            return Err(format!("{device}: not a device name"));
        }
        let btrfs = rynas_btrfs::receive_filter::locate("btrfs").ok_or("btrfs-progs is not installed")?;
        let dir = lock.dir.as_ref().ok_or("folder not open")?;
        sendable(dir, name)?;
        let root = lock.path.to_string_lossy().into_owned();
        let snap = format!("{root}/{name}");
        receive::valid_path(&snap)?;
        let hidden: Vec<String> = hidden
            .iter()
            .filter(|h| receive::valid_path(h).is_ok() && !within(&root, h) && !within(h, &root))
            .cloned()
            .collect();
        // a name of its own per send: a retry right after a cancelled one does not meet the
        // old unit still stopping (one send per PC is SendSlot's rule, not the unit name's)
        let unit = format!("{SEND_UNIT}-{device}-{}", staging_name()?);
        let mut argv: Vec<&std::ffi::OsStr> = vec![btrfs.as_os_str(), "send".as_ref(), "-q".as_ref()];
        // v1 (what every btrfs receive reads, and what the agent's own guard takes)
        if send_knows_proto(&btrfs) {
            argv.extend([std::ffi::OsStr::new("--proto"), std::ffi::OsStr::new("1")]);
        }
        argv.push(snap.as_ref());
        let mut child = confined(&unit, &format!("rynas: restore send of PC {device}"), &send_unit_properties(&hidden), &argv, Stdio::null())
            .spawn()
            .map_err(|e| format!("systemd-run: {e}"))?;
        let errs = stderr_tail(&mut child);
        let mut stdout = child.stdout.take().ok_or("no stdout")?;
        let mut buf = vec![0u8; 1 << 20];
        let copied: Result<(), String> = loop {
            if stop() {
                break Err("cancelled".into());
            }
            match stdout.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    if let Err(e) = out.write_all(&buf[..n]) {
                        break Err(format!("the client went away: {e}"));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => break Err(format!("reading btrfs send: {e}")),
            }
        };
        if copied.is_err() {
            // the unit and the process go: a cancelled restore leaves nothing running
            let _ = Command::new("systemctl").args(["stop", "--no-block", &format!("{unit}.service")]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
            let _ = child.kill();
        }
        drop(stdout);
        let status = child.wait().map_err(|e| e.to_string())?;
        let last = last_line(&errs.join().unwrap_or_default());
        copied?;
        out.flush().map_err(|e| format!("the client went away: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(if last.is_empty() { format!("btrfs send failed ({status})") } else { format!("btrfs send: {last}") })
        }
    }

    fn read_legacy(&self, root: &Path, name: &str) -> Result<RestoreLegacy, String> {
        use std::os::fd::AsRawFd;
        use rynas_btrfs::ioctl::{open_dir, open_dir_at};
        let gone = |e: &std::io::Error| matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ELOOP) | Some(libc::ENOTDIR));
        let snap = open_dir_at(&open_dir(root).map_err(|e| e.to_string())?, name).map_err(|e| format!("{name}: {e}"))?;
        let mut out = RestoreLegacy { snapshot: name.to_string(), ..Default::default() };
        if let Ok(etc) = open_dir_at(&snap, "etc") {
            out.fstab = read_small_at(&etc, "fstab", LEGACY_FILE_MAX)?.map(|b| legacy_text(&b));
            out.os_release = read_small_at(&etc, "os-release", LEGACY_FILE_MAX)?.map(|b| legacy_text(&b));
        }
        if out.os_release.is_none()
            && let Ok(lib) = open_dir_at(&snap, "usr").and_then(|u| open_dir_at(&u, "lib"))
        {
            out.os_release = read_small_at(&lib, "os-release", LEGACY_FILE_MAX)?.map(|b| legacy_text(&b));
        }
        let bb = match open_dir_at(&snap, LEGACY_BOOT_DIR) {
            Ok(d) => d,
            Err(e) if gone(&e) => return Ok(out),
            Err(e) => return Err(format!("{name}/{LEGACY_BOOT_DIR}: {e}")),
        };
        let mut names: Vec<String> = std::fs::read_dir(format!("/proc/self/fd/{}", bb.as_raw_fd()))
            .map_err(|e| e.to_string())?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.len() <= 128 && !n.starts_with('.'))
            .collect();
        names.sort();
        for n in names.iter().take(256) {
            if n.ends_with("_post") {
                if open_dir_at(&bb, n).is_ok() && out.boot_backups.len() < 64 {
                    out.boot_backups.push(n.clone());
                }
                continue;
            }
            if out.partition_dumps.len() < 8
                && let Some(b) = read_small_at(&bb, n, LEGACY_FILE_MAX)?
            {
                let text = legacy_text(&b);
                if looks_like_sfdisk_dump(&text) {
                    out.partition_dumps.push(LegacyFile { name: n.clone(), text });
                }
            }
        }
        Ok(out)
    }
}

/// `btrfs send --help` lists `--proto` (btrfs-progs 5.19 and later).
fn send_knows_proto(btrfs: &Path) -> bool {
    static KNOWS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *KNOWS.get_or_init(|| {
        std::process::Command::new(btrfs)
            .args(["send", "--help"])
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("--proto") || String::from_utf8_lossy(&o.stderr).contains("--proto"))
    })
}

/// `name` inside `dir` as a folder of root, `0700`: made when missing, refused when it is
/// anything else (a symlink, a file, another owner).
fn mkdir_open(dir: &std::fs::File, name: &str) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let c = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
    // SAFETY: valid descriptor and C string
    if unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o700) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(format!("{name}: {e}"));
        }
    }
    let f = rynas_btrfs::ioctl::open_dir_at(dir, name).map_err(|e| format!("{name}: {e}"))?;
    let m = f.metadata().map_err(|e| format!("{name}: {e}"))?;
    if m.uid() != 0 || m.mode() & 0o077 != 0 {
        return Err(format!("{name}: not a 0700 folder of root"));
    }
    Ok(f)
}

/// Deepest folder level [`remove_tree_at`] goes down to.
const REMOVE_DEPTH: usize = 64;

fn remove_tree_at(dir: &std::fs::File, name: &str, depth: usize) -> Result<(), String> {
    let c = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
    remove_tree_c(dir, &c, depth)
}

fn remove_tree_c(dir: &std::fs::File, c: &std::ffi::CStr, depth: usize) -> Result<(), String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let shown = c.to_string_lossy().into_owned();
    let unlink = |flags: libc::c_int| -> Result<(), String> {
        // SAFETY: valid descriptor and C string
        if unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), flags) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(format!("{shown}: {e}"));
            }
        }
        Ok(())
    };
    // SAFETY: valid descriptor and C string
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        return if e.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            // a symlink (ELOOP), a file (ENOTDIR), a device node, a socket
            unlink(0)
        };
    }
    // SAFETY: a descriptor we own
    let sub = unsafe { std::fs::File::from_raw_fd(fd) };
    if rynas_btrfs::ioctl::is_subvolume(&sub) {
        drop(sub);
        let name = c.to_str().map_err(|_| format!("{shown}: a subvolume whose name is not UTF-8"))?;
        return rynas_btrfs::ioctl::destroy_subvolume(dir, name).map_err(|e| format!("deleting {shown}: {e}"));
    }
    if depth >= REMOVE_DEPTH {
        return Err(format!("{shown}: folders nested deeper than {REMOVE_DEPTH} levels"));
    }
    let names: Vec<std::ffi::CString> = std::fs::read_dir(format!("/proc/self/fd/{}", sub.as_raw_fd()))
        .map_err(|e| format!("{shown}: {e}"))?
        .flatten()
        .filter_map(|e| std::ffi::CString::new(e.file_name().as_bytes()).ok())
        .collect();
    let mut first_err = None;
    for n in names {
        if let Err(e) = remove_tree_c(&sub, &n, depth + 1) {
            first_err.get_or_insert(e);
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    unlink(libc::AT_REMOVEDIR)
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
    /// The name the next receives create instead of the expected one (tests).
    pub receive_as: Mutex<Option<String>>,
    /// Staging folders of receives (`<folder>/.incoming/<name>`) and what they hold.
    pub staged: Mutex<BTreeMap<PathBuf, Vec<Entry>>>,
    /// Extra non-subvolume entries every staging folder shows (tests: what a stream could
    /// leave next to its subvolume).
    pub staged_extra: Mutex<Vec<String>>,
    /// The folders hidden from the last receive.
    pub hidden: Mutex<Vec<String>>,
    /// Restore sends: `<folder>/<name>[ -p <parent>]`, in order.
    pub sent: Mutex<Vec<String>>,
    /// What [`ReceiveHost::read_legacy`] answers, by snapshot path.
    pub legacy: Mutex<BTreeMap<PathBuf, RestoreLegacy>>,
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
    fn create_folder(&self, path: &Path, agent: bool) -> Result<(), String> {
        self.folders
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), (true, Vec::new()));
        self.prepare_adopted(path, agent)
    }
    fn prepare_adopted(&self, path: &Path, agent: bool) -> Result<(), String> {
        let mut m = self.meta.lock().unwrap();
        if agent {
            m.insert(path.to_path_buf(), (0, 0, 0o700));
        } else {
            m.remove(path);
        }
        Ok(())
    }
    fn receive(&self, _device: &str, lock: &FolderLock, staging: &str, hidden: &[String], input: &mut dyn std::io::Read) -> Result<(), String> {
        *self.hidden.lock().unwrap() = hidden.to_vec();
        let mut data = Vec::new();
        let copied = std::io::copy(input, &mut data);
        let first = rynas_btrfs::send_stream::first_subvolume(&data);
        let name = self
            .receive_as
            .lock()
            .unwrap()
            .clone()
            .or_else(|| first.as_ref().map(|f| f.0.clone()))
            .unwrap_or_else(|| "unknown".into());
        let key = lock.path.join(STAGING_DIR).join(staging);
        let folder = self.folders.lock().unwrap().get(&lock.path).map(|(_, e)| e.clone()).ok_or("no such folder")?;
        let mut st = self.staged.lock().unwrap();
        let entries = st.get_mut(&key).ok_or("no staging folder")?;
        if !matches!(copied, Ok(n) if n > 0) || first.is_none() {
            // what an interrupted receive leaves: a writable subvolume without received UUID
            entries.push(Entry {
                name,
                info: Some(rynas_btrfs::ioctl::SubvolInfo { root_id: 900, ..Default::default() }),
            });
            return Err(match copied {
                Err(e) => format!("the stream broke off: {e}"),
                Ok(_) => "btrfs receive: short read from stream".into(),
            });
        }
        let (_, uuid, clone) = first.unwrap_or_default();
        let parent_uuid = clone.and_then(|c| folder.iter().find(|e| e.info.as_ref().and_then(|i| i.received_uuid.as_deref()) == Some(c.as_str())))
            .and_then(|e| e.info.as_ref().and_then(|i| i.uuid.clone()));
        let now = rynas_tasks::now_ms();
        let id = 1000 + folder.len() as u64 + entries.len() as u64;
        entries.push(Entry {
            name,
            info: Some(rynas_btrfs::ioctl::SubvolInfo {
                root_id: id,
                readonly: true,
                uuid: Some(format!("0b7d9e4c-0000-4000-8000-{:012x}", now % 0xffff_ffff_ffff + id)),
                parent_uuid,
                received_uuid: uuid,
                otime: (now / 1000) as i64,
                ..Default::default()
            }),
        });
        Ok(())
    }

    fn make_staging(&self, lock: &FolderLock, staging: &str) -> Result<(), String> {
        self.staged.lock().unwrap().insert(lock.path.join(STAGING_DIR).join(staging), Vec::new());
        Ok(())
    }

    fn staged(&self, lock: &FolderLock, staging: &str) -> Result<Vec<Entry>, String> {
        let key = lock.path.join(STAGING_DIR).join(staging);
        let mut v = self.staged.lock().unwrap().get(&key).cloned().ok_or("no staging folder")?;
        v.extend(self.staged_extra.lock().unwrap().iter().map(|n| Entry { name: n.clone(), info: None }));
        Ok(v)
    }

    fn adopt_staged(&self, lock: &FolderLock, staging: &str, name: &str) -> Result<(), String> {
        let key = lock.path.join(STAGING_DIR).join(staging);
        let mut st = self.staged.lock().unwrap();
        let entries = st.get_mut(&key).ok_or("no staging folder")?;
        let at = entries.iter().position(|e| e.name == name).ok_or_else(|| format!("{name}: not staged"))?;
        let mut f = self.folders.lock().unwrap();
        let (_, folder) = f.get_mut(&lock.path).ok_or("no such folder")?;
        if folder.iter().any(|e| e.name == name) {
            return Err(format!("moving {name}: File exists"));
        }
        folder.push(entries.remove(at));
        folder.sort_by(|a, b| rynas_btrfs::name::compare_names(&b.name, &a.name));
        Ok(())
    }

    fn clear_staging(&self, lock: &FolderLock) -> Result<(), String> {
        let top = lock.path.join(STAGING_DIR);
        let mut st = self.staged.lock().unwrap();
        let gone: Vec<PathBuf> = st.keys().filter(|k| k.starts_with(&top)).cloned().collect();
        for k in gone {
            for e in st.remove(&k).unwrap_or_default() {
                self.deleted.lock().unwrap().push(format!("{}/{}", k.display(), e.name));
            }
        }
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
    fn lock_shared(&self, root: &Path) -> Result<Option<FolderLock>, String> {
        self.lock(root)
    }
    fn send(&self, _device: &str, lock: &FolderLock, name: &str, _hidden: &[String], out: &mut dyn std::io::Write, stop: &dyn Fn() -> bool) -> Result<(), String> {
        let entries = self.folders.lock().unwrap().get(&lock.path).map(|(_, e)| e.clone()).ok_or("no such folder")?;
        let e = entries.iter().map(agent_entry).find(|e| e.name == name && e.complete).ok_or_else(|| format!("{name}: not a complete received snapshot"))?;
        self.sent.lock().unwrap().push(format!("{}/{name}", lock.path.display()));
        // the stream `btrfs send` writes for a received snapshot: its received UUID
        let s = rynas_btrfs::send_stream::fake::stream(name, e.received_uuid.as_deref().unwrap_or_default(), None);
        for chunk in s.chunks(64) {
            if stop() {
                return Err("cancelled".into());
            }
            out.write_all(chunk).map_err(|e| format!("the client went away: {e}"))?;
        }
        out.flush().map_err(|e| e.to_string())
    }
    fn read_legacy(&self, root: &Path, name: &str) -> Result<RestoreLegacy, String> {
        Ok(self.legacy.lock().unwrap().get(&root.join(name)).cloned().unwrap_or_else(|| RestoreLegacy { snapshot: name.to_string(), ..Default::default() }))
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
    // display strings: at most SHOWN_MAX characters, control characters out
    fn cap(s: &mut String) {
        if s.chars().count() > SHOWN_MAX || s.chars().any(char::is_control) {
            *s = s.chars().filter(|c| !c.is_control()).take(SHOWN_MAX).collect();
        }
    }
    let mut disks: Vec<rynas_api_types::pcagent::Disk> = shown(v.get("disks"), 16);
    for d in &mut disks {
        for s in [&mut d.name, &mut d.model, &mut d.serial, &mut d.wwn] {
            cap(s);
        }
        if let Some(t) = &mut d.table {
            t.partitions.truncate(128);
            cap(&mut t.kind);
            cap(&mut t.disk_guid);
            for p in &mut t.partitions {
                for s in [&mut p.type_guid, &mut p.part_guid, &mut p.name, &mut p.dev] {
                    cap(s);
                }
            }
        }
    }
    // name → path → mount: only entries that look like what the agent writes
    let layout: Vec<rynas_api_types::pcagent::BtrbkSubvolume> = shown(v.pointer("/btrbk/subvolumes"), 256);
    let plain = |s: &str| s.len() <= SHOWN_MAX && !s.chars().any(char::is_control);
    let layout = layout
        .into_iter()
        .filter(|l| {
            valid_snapshot_prefix(&l.name)
                && plain(&l.path)
                && plain(&l.mount)
                && (l.mount.is_empty() || l.mount.starts_with('/'))
                // a nested one without a mount: its parent's name and a relative path
                && (l.nested_in.is_empty() || (valid_snapshot_prefix(&l.nested_in) && l.mount.is_empty()))
                && plain(&l.inside)
                && !l.inside.starts_with('/')
        })
        .collect();
    let mut boot_copies: Vec<rynas_api_types::pcagent::BootCopy> = shown(v.pointer("/btrbk/boot_copies"), 8);
    for c in &mut boot_copies {
        for s in [&mut c.mount, &mut c.dir, &mut c.signature] {
            cap(s);
        }
        c.skipped.truncate(32);
        c.skipped.iter_mut().for_each(cap);
        if let Some(e) = &mut c.error {
            cap(e);
        }
    }
    Ok(Manifest {
        written_at_ms: v.get("written_at_ms").and_then(serde_json::Value::as_u64),
        loader: v.pointer("/boot/loader").and_then(serde_json::Value::as_str).and_then(short),
        subvolumes,
        root,
        disks,
        layout,
        boot_copies,
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
    /// Fed by `rynas-agent` over HTTPS (no key line, retention on the NAS).
    #[serde(default)]
    agent: bool,
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

/// Called with (device, folder id) when an agent folder is removed.
pub type AgentRemoved = Box<dyn Fn(&str, u64) + Send + Sync>;

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
    /// Agent receives running now (all PCs, at most [`MAX_RECEIVES`]).
    receives: std::sync::atomic::AtomicUsize,
    /// Restore sends running now: the devices (one stream per PC), at most [`MAX_SENDS`].
    sending: Mutex<BTreeSet<String>>,
    /// Told when an agent folder goes (device, folder id): the PC records bound to it are
    /// revoked or fall back to the files method (set once at start, [`crate::pc_agent`]).
    pub on_agent_removed: std::sync::OnceLock<AgentRemoved>,
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
            receives: std::sync::atomic::AtomicUsize::new(0),
            sending: Mutex::new(BTreeSet::new()),
            on_agent_removed: std::sync::OnceLock::new(),
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
            .filter(|(_, t)| !t.agent)
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
            agent: t.agent,
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
    /// The folders of the PCs that send over SSH (the sudoers lines); agent folders need none.
    fn roots(&self) -> Vec<String> {
        self.reg.lock().unwrap().targets.values().filter(|t| !t.agent).map(|t| t.path.clone()).collect()
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
        // what receives cut off by a stop of rynasd left in the staging folders
        {
            let s = svc.clone();
            let _ = tokio::task::spawn_blocking(move || s.clear_all_staging()).await;
        }
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

// ------------------------------------------------------------------ rynas-agent's receive

/// A receive gives up after this long without a byte from the PC.
pub const RECEIVE_IDLE: Duration = Duration::from_secs(600);
/// Free space kept on the volume during a receive: this much, or 2 % of the volume.
pub const RECEIVE_RESERVE: u64 = 1 << 30;
/// Receives running at once on the NAS, all PCs together: they share the disks and the free
/// space (each one checks the space as it goes, see [`RECEIVE_SPACE_EVERY`]).
pub const MAX_RECEIVES: usize = 2;
/// The free space is read again after every this many bytes of decompressed stream: a
/// receive stops once the volume is down to the reserve, whoever filled it.
pub const RECEIVE_SPACE_EVERY: u64 = 64 << 20;
/// Lowest average rate (decompressed stream bytes per second, counted from the start) a
/// receive may run at after [`RECEIVE_RATE_GRACE`]: a PC that trickles holds its folder's
/// lock and one of the [`MAX_RECEIVES`] slots. 64 KiB/s is about 0.5 Mbit/s; the unit's
/// `RuntimeMaxSec` (48 h) stays the hard end.
pub const RECEIVE_MIN_RATE: u64 = 64 << 10;
pub const RECEIVE_RATE_GRACE: Duration = Duration::from_secs(15 * 60);

/// The names of an agent's receive: `snapshot` in btrbk's `long` form, not dated more than
/// a day ahead, and `parent` (when there is one) of the same subvolume, in any btrbk
/// timestamp format (a parent can be a copy an earlier btrbk setup sent with `long-iso`).
pub fn check_agent_names(snapshot: &str, parent: Option<&str>, now_ms: u64) -> Res<()> {
    let ok = |n: &str| {
        split_long_snapshot(n).is_some()
            && rynas_btrfs::name::split_snapshot_name(n).is_some()
            && !receive::future_dated(n, now_ms)
    };
    if !ok(snapshot) || !is_long_snapshot(snapshot) {
        return Err(Error::invalid("snapshot", "<name>.YYYYMMDDTHHMM[_N], not in the future"));
    }
    if let Some(p) = parent {
        let same = split_long_snapshot(p).map(|x| x.0) == split_long_snapshot(snapshot).map(|x| x.0);
        if !ok(p) || !same || p == snapshot {
            return Err(Error::invalid("parent", "an older snapshot of the same subvolume, or none"));
        }
    }
    Ok(())
}

/// The request body as a blocking reader (fed by an async task), counting the bytes.
struct BodyReader {
    rx: tokio::sync::mpsc::Receiver<Result<axum::body::Bytes, String>>,
    cur: axum::body::Bytes,
    read: Arc<std::sync::atomic::AtomicU64>,
}

impl std::io::Read for BodyReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.cur.is_empty() {
            match self.rx.blocking_recv() {
                Some(Ok(b)) => {
                    self.read.fetch_add(b.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    self.cur = b;
                }
                Some(Err(e)) => return Err(std::io::Error::other(e)),
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.cur.len());
        out[..n].copy_from_slice(&self.cur[..n]);
        let _ = self.cur.split_to(n);
        Ok(n)
    }
}

/// Stops a decompressed stream that would leave the volume with less than the reserve (the
/// room at the start, and the free space read again every [`RECEIVE_SPACE_EVERY`] bytes:
/// other receives share it), or that runs below [`RECEIVE_MIN_RATE`] on average.
struct Capped<R> {
    inner: R,
    left: Option<u64>,
    /// Reads the volume again: `Err` once it is down to the reserve.
    space_ok: Box<dyn FnMut() -> Result<(), String> + Send>,
    since_check: u64,
    total: u64,
    started: std::time::Instant,
}

impl<R: std::io::Read> std::io::Read for Capped<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(out)?;
        if let Some(l) = &mut self.left {
            *l = l.checked_sub(n as u64).ok_or_else(|| std::io::Error::other("the stream is larger than the free space of the volume"))?;
        }
        self.total += n as u64;
        self.since_check += n as u64;
        if self.since_check >= RECEIVE_SPACE_EVERY {
            self.since_check = 0;
            (self.space_ok)().map_err(std::io::Error::other)?;
        }
        // reading the monotonic clock is cheap (vDSO): checked on every read
        let secs = self.started.elapsed();
        if secs > RECEIVE_RATE_GRACE && self.total < RECEIVE_MIN_RATE.saturating_mul(secs.as_secs()) {
            return Err(std::io::Error::other(format!("the PC sends slower than {} KiB/s on average", RECEIVE_MIN_RATE >> 10)));
        }
        Ok(n)
    }
}

/// A slot among the [`MAX_RECEIVES`]; given back on drop.
struct ReceiveSlot<'a>(&'a std::sync::atomic::AtomicUsize);

impl<'a> ReceiveSlot<'a> {
    fn take(n: &'a std::sync::atomic::AtomicUsize) -> Option<Self> {
        use std::sync::atomic::Ordering;
        let mut v = n.load(Ordering::SeqCst);
        while v < MAX_RECEIVES {
            match n.compare_exchange(v, v + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(ReceiveSlot(n)),
                Err(now) => v = now,
            }
        }
        None
    }
}

impl Drop for ReceiveSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A fresh staging name: 16 hex characters.
fn staging_name() -> Result<String, String> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|e| e.to_string())?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// What a finished receive must have left in its staging folder: exactly one entry, the
/// announced snapshot, a read-only subvolume whose received UUID is the stream's and whose
/// parent (incremental) is the NAS' copy of the announced parent.
fn check_staged(staged: &[Entry], name: &str, stream_uuid: Option<&str>, parent_nas_uuid: Option<&str>) -> Result<(), String> {
    let [only] = staged else {
        let names: Vec<&str> = staged.iter().map(|e| e.name.as_str()).take(5).collect();
        return Err(format!("the receive left {} entries ({}), not just {name}", staged.len(), names.join(", ")));
    };
    let i = only.info.as_ref().filter(|_| only.name == name).ok_or_else(|| format!("the receive left {:?}, not the subvolume {name}", only.name))?;
    if !i.readonly || i.received_uuid.is_none() {
        return Err("the received snapshot is not read-only with a received UUID".into());
    }
    if stream_uuid.is_none() || i.received_uuid.as_deref() != stream_uuid {
        return Err("the received UUID is not the stream's".into());
    }
    if let Some(p) = parent_nas_uuid
        && i.parent_uuid.as_deref() != Some(p)
    {
        return Err("the received snapshot is not based on the announced parent".into());
    }
    Ok(())
}

fn agent_entry(e: &Entry) -> rynas_api_types::pcagent::AgentBtrfsSnapshot {
    let i = e.info.as_ref();
    rynas_api_types::pcagent::AgentBtrfsSnapshot {
        name: e.name.clone(),
        received_uuid: i.and_then(|i| i.received_uuid.clone()),
        complete: i.is_some_and(|i| i.readonly && i.received_uuid.is_some()),
    }
}

impl PcBackup {
    /// The folder of `device` fed by `rynas-agent` (404 without one, 409 when btrbk feeds it),
    /// and only the very folder the PC was bound to (`folder` = its [`Self::agent_folder_id`]
    /// then): a folder removed and made again under the same name does not take the old
    /// token's streams.
    fn agent_target(&self, device: &str, folder: Option<u64>) -> Res<Stored> {
        let t = self.stored(device)?;
        if !t.agent {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::CONFLICT,
                format!("the folder of {device} is fed by btrbk over SSH, not by rynas-agent"),
            ));
        }
        if folder != Some(t.created_at_ms) {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::CONFLICT,
                format!("the folder of {device} on the NAS is not the one this PC was set up for: choose the btrfs method for the PC again"),
            ));
        }
        Ok(t)
    }

    /// The identity of `device`'s agent folder (when it was made): the PC records keep it, so
    /// the token works only with that folder.
    pub fn agent_folder_id(&self, device: &str) -> Option<u64> {
        self.reg.lock().unwrap().targets.get(device).filter(|t| t.agent).map(|t| t.created_at_ms)
    }

    /// The other device folders (hidden from a receive).
    fn other_folders(&self, device: &str) -> Vec<String> {
        self.reg.lock().unwrap().targets.iter().filter(|(d, _)| d.as_str() != device).map(|(_, t)| t.path.clone()).collect()
    }

    /// Clears what earlier receives left in the staging folders of the agent folders (after
    /// a crash or a restart of rynasd); folders being received into are skipped.
    pub fn clear_all_staging(&self) {
        let targets: Vec<(String, String)> = self.reg.lock().unwrap().targets.iter().filter(|(_, t)| t.agent).map(|(d, t)| (d.clone(), t.path.clone())).collect();
        for (device, path) in targets {
            match self.host.lock(Path::new(&path)) {
                Ok(Some(lock)) => {
                    if let Err(e) = self.host.clear_staging(&lock) {
                        tracing::warn!("pc backup: {device}: clearing {path}/{STAGING_DIR}: {e}");
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("pc backup: {device}: {e}"),
            }
        }
    }

    /// `rynas-agent`'s folder exists for `device` (the btrfs method may be chosen).
    pub fn has_agent_target(&self, device: &str) -> bool {
        self.reg.lock().unwrap().targets.get(device).is_some_and(|t| t.agent)
    }

    /// What the agent's folder holds (the agent picks its parents from it).
    pub async fn agent_listing(self: &Arc<Self>, device: &str, folder: Option<u64>) -> Res<rynas_api_types::pcagent::AgentBtrfsListing> {
        let t = self.agent_target(device, folder)?;
        let s = self.clone();
        blocking(move || {
            let root = Path::new(&t.path);
            let Some((_, entries, _)) = s.host.scan(root).map_err(Error::internal)? else {
                return Err(Error::not_found(format!("{}: no such folder", t.path)));
            };
            Ok(rynas_api_types::pcagent::AgentBtrfsListing {
                snapshots: entries.iter().map(agent_entry).collect(),
                receiving: s.host.receiving(root),
            })
        })
        .await?
    }

    /// One `btrfs send` stream (zstd) from `rynas-agent` into the folder of `device`, with
    /// the protections of the SSH filter and more: the folder walked from `/` without
    /// symlinks and locked (the filter's per-device lock: one receive at a time, no prune
    /// meanwhile), at most [`MAX_RECEIVES`] PCs at once, btrbk-style names, a parent that is
    /// a complete snapshot here, a free-space reserve checked as the stream goes, a minimum
    /// rate, the stream checked command by command ([`rynas_btrfs::send_stream`]: exactly
    /// this subvolume, on this parent, nothing outside it), the receive confined to a fresh
    /// staging folder ([`receive_unit_properties`]); afterwards the staging folder must hold
    /// exactly the announced read-only subvolume with the stream's UUID on the announced
    /// parent ([`check_staged`]), which then moves into the folder. Anything else is deleted.
    pub async fn agent_receive(
        self: &Arc<Self>,
        device: &str,
        folder: Option<u64>,
        snapshot: String,
        parent: Option<String>,
        body: axum::body::Body,
    ) -> Res<rynas_api_types::pcagent::AgentBtrfsReceived> {
        use futures_util::StreamExt;
        use rynas_api_types::pcagent::{AGENT_ZSTD_WINDOW_LOG_MAX, PARENT_MISSING};
        let t = self.agent_target(device, folder)?;
        check_agent_names(&snapshot, parent.as_deref(), rynas_tasks::now_ms())?;
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, String>>(16);
        let pump = tokio::spawn(async move {
            let mut stream = body.into_data_stream();
            loop {
                let msg = match tokio::time::timeout(RECEIVE_IDLE, stream.next()).await {
                    Ok(Some(Ok(b))) => Ok(b),
                    Ok(Some(Err(e))) => Err(format!("the upload broke off: {e}")),
                    Ok(None) => break,
                    Err(_) => Err("no data from the PC for 10 minutes".to_string()),
                };
                let stop = msg.is_err();
                if tx.send(msg).await.is_err() || stop {
                    break;
                }
            }
        });
        let read = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let hidden = self.other_folders(device);
        let (s, dev, read2) = (self.clone(), device.to_string(), read.clone());
        let out = blocking(move || -> Res<rynas_api_types::pcagent::AgentBtrfsReceived> {
            let root = PathBuf::from(&t.path);
            let busy = |why: String| Error::new(StatusCode::CONFLICT, error_codes::BUSY, why);
            let _slot = ReceiveSlot::take(&s.receives)
                .ok_or_else(|| busy(format!("the NAS is receiving from {MAX_RECEIVES} PCs already: try again later")))?;
            let lock = s
                .host
                .lock(&root)
                .map_err(|e| Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, e))?
                .ok_or_else(|| busy(format!("a receive or a retention pass of {dev} is running")))?;
            // what an earlier receive may have left (rynasd stopped in the middle)
            s.host.clear_staging(&lock).map_err(Error::internal)?;
            let scan = |s: &PcBackup| -> Res<Vec<Entry>> {
                match s.host.scan(&root).map_err(Error::internal)? {
                    Some((_, e, _)) => Ok(e),
                    None => Err(Error::not_found(format!("{}: no such folder", root.display()))),
                }
            };
            let before = scan(&s)?;
            if before.iter().any(|e| e.name == snapshot) {
                return Err(Error::new(StatusCode::CONFLICT, error_codes::ALREADY_EXISTS, format!("{snapshot} is already on the NAS")));
            }
            // (received UUID the stream must name, the NAS copy's own UUID it must be based on)
            let parent_ids = match &parent {
                None => None,
                Some(p) => {
                    let e = before.iter().find(|e| &e.name == p).filter(|e| agent_entry(e).complete);
                    match e.and_then(|e| e.info.as_ref()) {
                        Some(i) => Some((i.received_uuid.clone(), i.uuid.clone())),
                        None => {
                            return Err(Error::new(StatusCode::CONFLICT, PARENT_MISSING, format!("{p} is not a complete snapshot on the NAS: send in full")));
                        }
                    }
                }
            };
            let reserve_of = |total: u64| RECEIVE_RESERVE.max(total / 50);
            let room = match s.host.space(&root) {
                Some((free, total)) => {
                    let reserve = reserve_of(total);
                    if free <= reserve {
                        return Err(Error::new(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            error_codes::PAYLOAD_TOO_LARGE,
                            format!("the NAS volume has {} MiB free, less than the {} MiB it keeps", free >> 20, reserve >> 20),
                        ));
                    }
                    Some(free - reserve)
                }
                None => None,
            };
            let staging = staging_name().map_err(Error::internal)?;
            s.host.make_staging(&lock, &staging).map_err(Error::internal)?;
            let body = BodyReader { rx, cur: axum::body::Bytes::new(), read: read2.clone() };
            let mut decoded = zstd::stream::read::Decoder::new(body).map_err(Error::internal)?;
            // a bounded window: the agent sends 2 MiB windows, a larger one is refused
            decoded.window_log_max(AGENT_ZSTD_WINDOW_LOG_MAX).map_err(Error::internal)?;
            let (host, root2) = (s.host.clone(), root.clone());
            let space_ok = Box::new(move || match host.space(&root2) {
                Some((free, total)) if free <= reserve_of(total) => {
                    Err(format!("the NAS volume is down to {} MiB free (other receives, or other writes)", free >> 20))
                }
                _ => Ok(()),
            });
            let limits = Capped { inner: decoded, left: room, space_ok, since_check: 0, total: 0, started: std::time::Instant::now() };
            let expect = rynas_btrfs::send_stream::Expect { name: snapshot.clone(), parent_uuid: parent_ids.as_ref().and_then(|p| p.0.clone()) };
            let mut guard = rynas_btrfs::send_stream::Guard::new(limits, expect);
            let res = s.host.receive(&dev, &lock, &staging, &hidden, &mut guard);
            // checked under the lock: the staging folder, then the move
            let res = res.and_then(|()| {
                if !guard.seen.ended {
                    return Err("the stream did not end".into());
                }
                let staged = s.host.staged(&lock, &staging)?;
                check_staged(&staged, &snapshot, guard.seen.uuid.as_deref(), parent_ids.as_ref().and_then(|p| p.1.as_deref()))?;
                s.host.adopt_staged(&lock, &staging, &snapshot)
            });
            if let Err(e) = s.host.clear_staging(&lock) {
                tracing::warn!("pc backup: {dev}: clearing the staging folder: {e}");
            }
            let after = scan(&s)?;
            let got = after.iter().find(|e| e.name == snapshot).map(agent_entry);
            match (res, got) {
                (Ok(()), Some(e)) if e.complete => Ok(rynas_api_types::pcagent::AgentBtrfsReceived {
                    name: snapshot.clone(),
                    received_uuid: e.received_uuid.unwrap_or_default(),
                    bytes: read2.load(std::sync::atomic::Ordering::Relaxed),
                }),
                (res, got) => {
                    // a move that half failed (writable in the folder): never a later parent
                    if got.is_some()
                        && let Err(e) = s.host.delete_snapshot(&lock, &snapshot)
                    {
                        tracing::warn!("pc backup: {dev}: removing the failed receive {snapshot}: {e}");
                    }
                    let why = match res {
                        Err(e) => e,
                        Ok(()) => "the received snapshot is not read-only with a received UUID".into(),
                    };
                    Err(Error::new(StatusCode::UNPROCESSABLE_ENTITY, error_codes::INVALID_VALUE, why))
                }
            }
        })
        .await;
        pump.abort();
        let out = out?;
        match &out {
            Ok(r) => tracing::info!(target: "rynas::audit", "pc backup: {device}: received {} ({} bytes)", r.name, r.bytes),
            Err(e) => tracing::warn!("pc backup: {device}: receive refused or failed: {}", e.2),
        }
        out
    }
}

// ------------------------------------------------------------------ bare-metal restore

/// Restore sends running at once on the NAS, all PCs together.
pub const MAX_SENDS: usize = 2;

/// A restore send's place among [`MAX_SENDS`], one per device; given back on drop.
struct SendSlot {
    svc: Arc<PcBackup>,
    device: String,
}

impl SendSlot {
    fn take(svc: &Arc<PcBackup>, device: &str) -> Res<SendSlot> {
        let busy = |why: String| Error::new(StatusCode::CONFLICT, error_codes::BUSY, why);
        let mut s = svc.sending.lock().unwrap();
        if s.contains(device) {
            return Err(busy(format!("a restore of {device} is already reading from the NAS")));
        }
        if s.len() >= MAX_SENDS {
            return Err(busy(format!("the NAS is sending to {MAX_SENDS} restores already: try again later")));
        }
        s.insert(device.to_string());
        Ok(SendSlot { svc: svc.clone(), device: device.to_string() })
    }
}

impl Drop for SendSlot {
    fn drop(&mut self) {
        self.svc.sending.lock().unwrap().remove(&self.device);
    }
}

/// The compressed stream as chunks for the response body; a failed send ends the body with
/// an error (the client never sees a clean end of a broken stream).
struct ChanWriter {
    tx: tokio::sync::mpsc::Sender<Result<axum::body::Bytes, std::io::Error>>,
    buf: Vec<u8>,
    sent: u64,
    /// Called every [`KEEPALIVE_EVERY`] while the stream runs: keeps the restore session
    /// alive during one long send, and says whether it still is (`false`: it ended, e.g. the
    /// report closed it, and the stream stops).
    keepalive: Box<dyn Fn() -> bool + Send + Sync>,
    last: std::time::Instant,
}

/// See [`ChanWriter::keepalive`].
const KEEPALIVE_EVERY: Duration = Duration::from_secs(300);

/// A client that takes nothing for this long is gone: the send stops, and with it the
/// shared lock of the folder and the NAS-wide send slot.
const SEND_STALL: Duration = Duration::from_secs(600);

impl ChanWriter {
    fn push(&mut self) -> std::io::Result<()> {
        use tokio::sync::mpsc::error::TrySendError;
        let gone = |why: &str| std::io::Error::new(std::io::ErrorKind::BrokenPipe, why.to_string());
        if self.buf.is_empty() {
            return Ok(());
        }
        if self.last.elapsed() >= KEEPALIVE_EVERY {
            self.last = std::time::Instant::now();
            if !(self.keepalive)() {
                return Err(gone("the restore session ended"));
            }
        }
        let mut b = Ok(axum::body::Bytes::from(std::mem::take(&mut self.buf)));
        let len = b.as_ref().map(|x| x.len()).unwrap_or(0) as u64;
        // never wait without end on a full channel: a stalled client would hold the folder
        let since = std::time::Instant::now();
        loop {
            match self.tx.try_send(b) {
                Ok(()) => break,
                Err(TrySendError::Closed(_)) => return Err(gone("the client went away")),
                Err(TrySendError::Full(back)) => {
                    if since.elapsed() >= SEND_STALL {
                        return Err(gone("the client took nothing for 10 minutes"));
                    }
                    b = back;
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        self.sent += len;
        Ok(())
    }
}

impl std::io::Write for ChanWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(b);
        if self.buf.len() >= 256 << 10 {
            self.push()?;
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.push()
    }
}

impl PcBackup {
    /// `device` has a folder of btrfs snapshots (agent or btrbk): a restore code may cover it.
    pub fn has_target(&self, device: &str) -> bool {
        self.reg.lock().unwrap().targets.contains_key(device)
    }

    /// Every folder: (device, description).
    pub fn target_names(&self) -> Vec<(String, String)> {
        self.reg.lock().unwrap().targets.iter().map(|(d, t)| (d.clone(), t.description.clone())).collect()
    }

    /// A restore's view of the folder of `device`: every snapshot with its state and size,
    /// the runs, the manifest's snapshot.
    pub async fn restore_listing(self: &Arc<Self>, device: &str) -> Res<RestoreBtrfsListing> {
        let t = self.stored(device)?;
        let (s, d) = (self.clone(), device.to_string());
        blocking(move || {
            let v = s.view(&d, &t, rynas_tasks::now_ms());
            if v.target.state == ReceiveState::Missing {
                return Err(Error::not_found(format!("{}: no such folder", t.path)));
            }
            Ok(RestoreBtrfsListing {
                device: d.clone(),
                agent: t.agent,
                snapshots: v.list,
                runs: v.runs,
                manifest_snapshot: v.target.bare_metal.manifest_snapshot,
            })
        })
        .await?
    }

    /// `name` is a complete received snapshot directly in the folder of `t` (what a restore
    /// may read).
    fn complete_entry(&self, t: &Stored, name: &str) -> Res<Entry> {
        if name.contains('/') || rynas_btrfs::name::split_snapshot_name(name).is_none() {
            return Err(Error::invalid("snapshot", "a snapshot name of this PC"));
        }
        let entries = match self.host.scan(Path::new(&t.path)).map_err(Error::internal)? {
            Some((_, e, _)) => e,
            None => return Err(Error::not_found(format!("{}: no such folder", t.path))),
        };
        entries
            .into_iter()
            .find(|e| e.name == name && agent_entry(e).complete)
            .ok_or_else(|| Error::not_found(format!("{name} is not a complete snapshot of this PC")))
    }

    /// `/.rynas/manifest.json` of snapshot `name` (B1's descriptor-walk reader), checked to
    /// be a JSON object of the format this NAS knows (`MANIFEST_FORMAT`).
    pub async fn restore_manifest(self: &Arc<Self>, device: &str, name: String) -> Res<Vec<u8>> {
        let t = self.stored(device)?;
        let s = self.clone();
        blocking(move || {
            s.complete_entry(&t, &name)?;
            let b = s
                .host
                .read_manifest(Path::new(&t.path), &name)
                .map_err(|e| Error::new(StatusCode::UNPROCESSABLE_ENTITY, error_codes::INVALID_VALUE, e))?
                .ok_or_else(|| Error::not_found(format!("{name} has no layout manifest")))?;
            let v: serde_json::Value = serde_json::from_slice(&b)
                .map_err(|_| Error::new(StatusCode::UNPROCESSABLE_ENTITY, error_codes::INVALID_VALUE, format!("the manifest of {name} does not parse")))?;
            if v.get("format").and_then(serde_json::Value::as_u64) != Some(u64::from(rynas_api_types::pcagent::MANIFEST_FORMAT)) {
                return Err(Error::new(StatusCode::UNPROCESSABLE_ENTITY, error_codes::INVALID_VALUE, format!("the manifest of {name} has a format this NAS does not know")));
            }
            Ok(b)
        })
        .await?
    }

    /// What an older restore script left in root snapshot `name` (no manifest).
    pub async fn restore_legacy(self: &Arc<Self>, device: &str, name: String) -> Res<RestoreLegacy> {
        let t = self.stored(device)?;
        let s = self.clone();
        blocking(move || {
            s.complete_entry(&t, &name)?;
            s.host.read_legacy(Path::new(&t.path), &name).map_err(Error::internal)
        })
        .await?
    }

    /// `btrfs send <snapshot> | zstd` (full) of the folder of `device` for a bare-metal
    /// restore: a complete read-only snapshot directly in the folder, the folder walked from
    /// `/` without symlinks and locked shared (no receive or retention pass meanwhile), one
    /// stream per PC and at most [`MAX_SENDS`] on the NAS, the send in its read-only unit
    /// ([`send_unit_properties`]); a client that goes away, takes nothing for
    /// [`SEND_STALL`], or whose session ends stops it. Audited.
    pub async fn restore_send(self: &Arc<Self>, device: &str, name: String, who: String, keepalive: Box<dyn Fn() -> bool + Send + Sync>) -> Res<axum::body::Body> {
        use rynas_api_types::pcagent::AGENT_ZSTD_WINDOW_LOG;
        let t = self.stored(device)?;
        let slot = SendSlot::take(self, device)?;
        let (s, n) = (self.clone(), name.clone());
        let lock = blocking(move || -> Res<FolderLock> {
            s.complete_entry(&t, &n)?;
            s.host
                .lock_shared(Path::new(&t.path))
                .map_err(|e| Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, e))?
                .ok_or_else(|| Error::new(StatusCode::CONFLICT, error_codes::BUSY, "a receive or a retention pass of this PC is running: try again in a moment".into()))
        })
        .await??;
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(8);
        let hidden = self.other_folders(device);
        let (s, dev) = (self.clone(), device.to_string());
        tracing::info!(target: "rynas::audit", "pc backup: {dev}: restore send of {name} to {who}");
        std::thread::Builder::new()
            .name("pc-send".into())
            .spawn(move || {
                let _slot = slot;
                let w = ChanWriter { tx: tx.clone(), buf: Vec::new(), sent: 0, keepalive, last: std::time::Instant::now() };
                let r = (|| -> Result<u64, String> {
                    let mut enc = zstd::stream::write::Encoder::new(w, 3).map_err(|e| e.to_string())?;
                    enc.window_log(AGENT_ZSTD_WINDOW_LOG).map_err(|e| e.to_string())?;
                    let closed = tx.clone();
                    s.host.send(&dev, &lock, &name, &hidden, &mut enc, &|| closed.is_closed())?;
                    let mut w = enc.finish().map_err(|e| e.to_string())?;
                    std::io::Write::flush(&mut w).map_err(|e| e.to_string())?;
                    Ok(w.sent)
                })();
                drop(lock);
                match r {
                    Ok(bytes) => tracing::info!(target: "rynas::audit", "pc backup: {dev}: restore send of {name} done ({bytes} bytes)"),
                    Err(e) => {
                        tracing::warn!("pc backup: {dev}: restore send of {name} stopped: {e}");
                        // the body ends in an error, never cleanly; a client that takes
                        // nothing gets no wait (its stream is cut short, which its guard sees)
                        let _ = tx.try_send(Err(std::io::Error::other(e)));
                    }
                }
            })
            .map_err(Error::internal)?;
        let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|x| (x, rx)) });
        Ok(axum::body::Body::from_stream(stream))
    }
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
                manifest_hook: if t.agent { String::new() } else { receive::manifest_hook() },
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
    let mut req = req;
    if req.agent {
        if !req.public_key.trim().is_empty() {
            return Err(Error::invalid("public_key", "a folder for rynas-agent takes no SSH key"));
        }
        // the agent never deletes on the NAS: retention is the NAS's
        req.retention.owner = RetentionOwner::Nas;
    }
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
            // an agent folder needs no SSH account, key or sudoers line
            let mut log = if req.agent {
                Vec::new()
            } else {
                let mut roots = s.roots();
                roots.push(req.path.clone());
                s.host.ensure_account(&btrfs, &roots)?
            };
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
                    s.host.prepare_adopted(path, req.agent)?;
                    log.push(format!("{} adopted as it is: {} snapshots", req.path, entries.len()));
                }
                (None, true) => return Err(format!("{}: no such folder", req.path)),
                (Some(_), false) => {
                    return Err(format!("{} already exists: adopt it instead", req.path));
                }
                (None, false) => {
                    s.host.create_folder(path, req.agent)?;
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
                        agent: req.agent,
                        ..Default::default()
                    },
                );
            })?;
            if !req.agent {
                s.host.write_keys(&s.keys_text())?;
                log.push("keys of the account written".into());
            }
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
    let Json(mut req) = body?;
    let cur = s.stored(&device)?;
    if req.description.chars().count() > 120 {
        return Err(Error::invalid("description", "at most 120 characters"));
    }
    check_key(&req.public_key)?;
    let agent = req.agent.unwrap_or(cur.agent);
    if agent {
        if !req.public_key.trim().is_empty() {
            return Err(Error::invalid("public_key", "a folder for rynas-agent takes no SSH key"));
        }
        // the agent never deletes on the NAS: retention is the NAS's
        let mut r = req.retention.take().unwrap_or_else(|| cur.retention.clone());
        r.owner = RetentionOwner::Nas;
        req.retention = Some(r);
    }
    check_policy(req.alert_days, &req.delete)?;
    check_more(req.alert_hours, req.retention.as_ref(), req.ignored_groups.as_deref())?;
    let switched = agent != cur.agent;
    let s2 = s.clone();
    let who2 = who.clone();
    let id = s.ops.tasks.spawn("pcbackup.update", &who, move |ctx| async move {
        let (s, who) = (s2, who2);
        let _g = s.change.lock().await;
        let s3 = s.clone();
        let dev = device.clone();
        let log = tokio::task::spawn_blocking(move || -> Result<Vec<String>, String> {
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
                    t.agent = agent;
                }
            })?;
            let mut log = Vec::new();
            let roots = s3.roots();
            if roots.is_empty() {
                // no PC sends over SSH any more
                if switched {
                    log.extend(s3.host.remove_account()?);
                }
            } else {
                if switched {
                    let btrfs = s3.host.facts().btrfs.ok_or("btrfs-progs is not installed")?;
                    log.extend(s3.host.ensure_account(&btrfs, &roots)?);
                }
                s3.host.write_keys(&s3.keys_text())?;
                log.push("keys of the account written".into());
            }
            if switched {
                // the snapshots stay as they are: the chain goes on with the other sender
                s3.host.prepare_adopted(Path::new(&cur.path), agent)?;
                log.push(if agent {
                    format!("{dev}: now sent by rynas-agent over HTTPS (key line removed)")
                } else {
                    format!("{dev}: now sent by btrbk over SSH")
                });
            }
            Ok(log)
        })
        .await
        .map_err(|e| e.to_string())??;
        for l in log {
            ctx.log(&l);
        }
        ctx.log("settings saved");
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
        // the agent PC bound to this folder: its token stops working with it at once
        if t.agent
            && let Some(f) = s.on_agent_removed.get()
        {
            f(&device, t.created_at_ms);
            ctx.log(&format!("{device}: the PC bound to this folder can no longer send into it"));
        }
        let removed = |key: String| field(code(resolved(NotifyCategory::Backup, "pcbackup", key, format!("{device} removed")), "pcbackup.removed"), "device", &device);
        if t.alerted {
            s.notifier.emit(removed(format!("pcbackup:late:{device}")));
        }
        for k in t.open_alerts.keys() {
            s.notifier.emit(removed(format!("pcbackup:{k}:{device}")));
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

    /// A subvolume nested in another without a mount (`@home/games`) is a group of its own:
    /// expected, needed for bare metal, kept in the layout with its parent.
    #[test]
    fn nested_subvolumes_in_the_manifest() {
        let m = parse_manifest(
            br#"{"format":1,"boot":{"loader":"grub"},"btrbk":{"subvolumes":[
            {"name":"@","path":"@","mount":"/"},{"name":"@home","path":"@home","mount":"/home"},
            {"name":"games","path":"@home/games","nested_in":"@home","inside":"games"},
            {"name":"bad","path":"@home/bad","mount":"/x","nested_in":"@home","inside":"bad"},
            {"name":"abs","path":"@home/abs","nested_in":"@home","inside":"/abs"}]}}"#,
        )
        .unwrap();
        assert_eq!(m.subvolumes, ["@", "@home", "games", "bad", "abs"]);
        // the layout shown keeps only well-formed nested entries
        let names: Vec<&str> = m.layout.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["@", "@home", "games"]);
        assert_eq!((m.layout[2].nested_in.as_str(), m.layout[2].inside.as_str()), ("@home", "games"));
        let b = bare_metal(&BTreeSet::from(["@".to_string(), "@home".to_string()]), Some(("@.x".into(), None, Ok(m.clone()))), None);
        assert!(b.missing.iter().any(|g| g.code == BareMetalCode::SubvolumeMissing && g.subject.as_deref() == Some("games")));
        let all: BTreeSet<String> = m.subvolumes.iter().cloned().collect();
        assert!(bare_metal(&all, Some(("@.x".into(), None, Ok(m))), None).ready);
        // expected groups of a run: the nested one counts
        let snap = |g: &str| ReceivedSnapshot { name: format!("{g}.20261110T0300"), group: g.into(), created_ms: Some(1), readonly: true, received_uuid: Some("u".into()), ..Default::default() };
        let list = vec![snap("@"), snap("@home")];
        let r = runs(&list, &BTreeSet::from(["@".to_string(), "@home".to_string(), "games".to_string()]));
        assert_eq!(r[0].lacks, ["games"]);
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

    #[test]
    fn agent_names_and_the_receive_sandbox() {
        let now = 1_794_500_000_000;
        let ts = |h: i64| rynas_btrfs::name::long_timestamp((now / 1000) as i64 - h * 3600);
        let (a, b) = (format!("@home.{}", ts(2)), format!("@home.{}", ts(1)));
        assert!(check_agent_names(&a, None, now).is_ok());
        assert!(check_agent_names(&b, Some(&a), now).is_ok());
        assert!(check_agent_names(&format!("{b}_3"), Some(&a), now).is_ok());
        // a parent btrbk sent with `long-iso` before the switch to the agent
        assert!(check_agent_names(&b, Some("@home.20261101T030512+0100"), now).is_ok());
        for (s, p) in [
            ("@home.20261101T030512+0100".to_string(), None),
            ("@home".to_string(), None),
            ("@home.29991231T0000".to_string(), None),
            (format!("../@home.{}", ts(1)), None),
            (format!("@home.{}+0100", ts(1)), None),
            (b.clone(), Some(format!("@.{}", ts(2)))),
            (b.clone(), Some(b.clone())),
            (b.clone(), Some("junk".to_string())),
        ] {
            assert!(check_agent_names(&s, p.as_deref(), now).is_err(), "{s} {p:?}");
        }
        let p = receive_unit_properties("/volume1/backup/pc", &["/volume1/backup/other".into()]);
        for want in [
            "ProtectSystem=strict",
            "ReadWritePaths=/volume1/backup/pc",
            "InaccessiblePaths=-/volume1/backup/other",
            "PrivateNetwork=yes",
            "NoNewPrivileges=yes",
            "DevicePolicy=closed",
            "DeviceAllow=char-* m",
            "DeviceAllow=block-* m",
            "RestrictNamespaces=yes",
            "ProtectKernelLogs=yes",
            "ProtectClock=yes",
            "ProtectHostname=yes",
        ] {
            assert!(p.iter().any(|x| x == want), "{want}");
        }
        assert!(p.iter().any(|x| x.starts_with("TasksMax=")) && p.iter().any(|x| x.starts_with("CPUQuota=")));
        let caps = p.iter().find(|x| x.starts_with("CapabilityBoundingSet=")).unwrap();
        assert!(caps.contains("CAP_SYS_CHROOT") && caps.contains("CAP_MKNOD") && !caps.contains("CAP_NET") && !caps.contains("CAP_SYS_PTRACE"));
        // set-id files and device nodes must still be received
        for never in ["PrivateTmp", "PrivateDevices", "RestrictSUIDSGID"] {
            assert!(!p.iter().any(|x| x.starts_with(never)), "{never}");
        }
    }

    #[test]
    fn a_staging_folder_must_hold_exactly_the_announced_snapshot() {
        use rynas_btrfs::ioctl::SubvolInfo;
        let (u, pu, n) = ("u-stream", "u-parent-on-nas", "@.20261112T0300");
        let good = |name: &str| Entry {
            name: name.into(),
            info: Some(SubvolInfo { readonly: true, received_uuid: Some(u.into()), parent_uuid: Some(pu.into()), ..Default::default() }),
        };
        assert!(check_staged(&[good(n)], n, Some(u), Some(pu)).is_ok());
        assert!(check_staged(&[good(n)], n, Some(u), None).is_ok());
        // nothing, two entries, another name, a file, writable, another received or parent UUID
        assert!(check_staged(&[], n, Some(u), None).is_err());
        assert!(check_staged(&[good(n), Entry { name: "big-file".into(), info: None }], n, Some(u), None).is_err());
        assert!(check_staged(&[good("other.20261112T0300")], n, Some(u), None).is_err());
        assert!(check_staged(&[Entry { name: n.into(), info: None }], n, Some(u), None).is_err());
        let mut w = good(n);
        w.info.as_mut().unwrap().readonly = false;
        assert!(check_staged(&[w], n, Some(u), None).is_err());
        assert!(check_staged(&[good(n)], n, Some("u-other"), None).is_err());
        assert!(check_staged(&[good(n)], n, None, None).is_err());
        assert!(check_staged(&[good(n)], n, Some(u), Some("u-another-pcs")).is_err());
    }

    #[test]
    fn receive_slots_and_limits() {
        use std::io::Read;
        let n = std::sync::atomic::AtomicUsize::new(0);
        let slots: Vec<_> = (0..MAX_RECEIVES).map(|_| ReceiveSlot::take(&n).unwrap()).collect();
        assert!(ReceiveSlot::take(&n).is_none());
        drop(slots);
        assert!(ReceiveSlot::take(&n).is_some());
        // the free space read again as the stream goes: another receive filled the volume
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = calls.clone();
        let data = vec![0u8; (RECEIVE_SPACE_EVERY as usize) * 2 + 10];
        let mut r = Capped {
            inner: &data[..],
            left: None,
            space_ok: Box::new(move || if c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 { Ok(()) } else { Err("full".into()) }),
            since_check: 0,
            total: 0,
            started: std::time::Instant::now(),
        };
        let e = std::io::copy(&mut r, &mut std::io::sink()).unwrap_err();
        assert!(e.to_string().contains("full"));
        // the room at the start
        let mut r = Capped { inner: &data[..], left: Some(100), space_ok: Box::new(|| Ok(())), since_check: 0, total: 0, started: std::time::Instant::now() };
        assert!(std::io::copy(&mut r, &mut std::io::sink()).is_err());
        // too slow on average once the grace is over
        let mut r = Capped {
            inner: &data[..10],
            left: None,
            space_ok: Box::new(|| Ok(())),
            since_check: 0,
            total: 0,
            started: std::time::Instant::now() - RECEIVE_RATE_GRACE - Duration::from_secs(60),
        };
        let mut b = [0u8; 4];
        assert!(r.read(&mut b).unwrap_err().to_string().contains("slower"));
    }

    #[test]
    fn manifest_display_strings_are_bounded() {
        let long = "x".repeat(1000);
        let m = serde_json::json!({
            "format": 1,
            "boot": {"loader": "grub"},
            "disks": [{"name": "nvme0n1", "model": long, "serial": "a\u{1b}[31mb", "size_bytes": 1}],
            "btrbk": {
                "subvolumes": [
                    {"name": "@", "path": "@", "mount": "/"},
                    {"name": ".bad", "path": "x", "mount": "/x"},
                    {"name": "ok", "path": "p", "mount": "relative"},
                    {"name": "ctl", "path": "a\nb", "mount": "/c"}
                ],
                "boot_copies": [{"mount": "/boot", "dir": "/.rynas/boot/boot", "bytes": 1, "files": 1, "skipped": vec![long.clone(); 50], "stale": true, "error": long}]
            }
        });
        let r = parse_manifest(&serde_json::to_vec(&m).unwrap()).unwrap();
        assert_eq!(r.disks[0].model.chars().count(), SHOWN_MAX);
        assert_eq!(r.disks[0].serial, "a[31mb");
        assert_eq!(r.layout.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["@"]);
        let c = &r.boot_copies[0];
        assert!(c.stale && c.skipped.len() == 32 && c.skipped[0].len() == SHOWN_MAX);
        assert_eq!(c.error.as_ref().unwrap().len(), SHOWN_MAX);
    }


    #[test]
    fn restore_sends_one_per_pc_and_a_read_only_unit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("r")).unwrap();
        let config = Arc::new(
            rynas_config::Config::open(rynas_config::ConfigOptions::new(dir.path().join("s"), dir.path().join("r")), Arc::new(rynas_config::MemorySystem::new("nas")), Vec::new()).unwrap(),
        );
        let tasks = rynas_tasks::TaskManager::new(dir.path().join("t"), "nas", rynas_tasks::TaskConfig::default()).unwrap();
        let svc = PcBackup::new(
            Arc::new(crate::ops::Ops::new(config, tasks)),
            Arc::new(MemoryReceive::new()),
            Arc::new(rynas_notify::MemoryNotifier::default()),
            dir.path().join("s"),
            AdminPolicy::default(),
            "nas".into(),
        );
        let a = SendSlot::take(&svc, "a").unwrap();
        assert_eq!(SendSlot::take(&svc, "a").err().unwrap().1, error_codes::BUSY);
        let b = SendSlot::take(&svc, "b").unwrap();
        assert!(SendSlot::take(&svc, "c").is_err(), "at most {MAX_SENDS} on the NAS");
        drop(a);
        let _a = SendSlot::take(&svc, "a").unwrap();
        drop(b);
        assert!(svc.sending.lock().unwrap().len() == 1);
        // the send's unit only reads: nothing writable, the other folders hidden
        let p = send_unit_properties(&["/volume1/backup/other".into()]);
        assert!(p.contains(&"ProtectSystem=strict".to_string()));
        assert!(!p.iter().any(|x| x.starts_with("ReadWritePaths")));
        assert!(p.contains(&"CapabilityBoundingSet=CAP_SYS_ADMIN CAP_DAC_READ_SEARCH".to_string()));
        assert!(p.contains(&"InaccessiblePaths=-/volume1/backup/other".to_string()));
        assert!(p.contains(&"DevicePolicy=closed".to_string()));
    }

    #[test]
    fn partition_table_dumps_are_told_by_their_content() {
        assert!(looks_like_sfdisk_dump("label: gpt\nlabel-id: X\n\n/dev/sda1 : start=2048, size=10, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B\n"));
        assert!(!looks_like_sfdisk_dump("label: gpt\n"));
        assert!(!looks_like_sfdisk_dump("loader.conf\ntimeout 3\n"));
    }
}
