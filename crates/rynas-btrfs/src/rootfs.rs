//! Rolling the system back to a restore point (D-SNAP).
//!
//! The root is the btrfs subvolume `@rootfs`; the kernel that boots it lives where the boot
//! loader reads it, which may be outside the root (a boot flash with its own kernel entries).
//! A rollback must change both together, or the machine boots a kernel whose `/lib/modules`
//! is not in the root. That second half is a [`BootSync`]: the boot layer of a hardware
//! module, which rewrites the boot entry from the kernel of the system at a directory and
//! refuses when that directory lacks `/lib/modules/<release>`. Without one the rollback is
//! refused (a console procedure).
//!
//! [`rollback_root`] runs, in order:
//! 1. the restore point is bind-mounted read-only at `/run/rynas/rollback/<id>`;
//! 2. its kernel is read (`/vmlinuz` link or newest `/boot/vmlinuz-*`) and the boot entry is
//!    pointed at it ([`BootSync::sync_root`]); a refusal stops everything, nothing has changed
//!    yet;
//! 3. the filesystem's top level (subvolid 5) is mounted at `/run/rynas/rollback/top`; a new
//!    read-only restore point of the running system is taken (reason "before rollback"); a
//!    writable snapshot of the chosen restore point is created next to `@rootfs`; the restore
//!    points (nested subvolumes inside `@rootfs`) move into it, so they stay listed after the
//!    reboot; then `@rootfs` is renamed `@rootfs.rynas-rollback-<stamp>` and the new one takes
//!    its name (`RENAME_NOREPLACE`, every step undone if a later one fails, and the boot
//!    entry synced back to the running system);
//! 4. both mounts go away and the caller asks for a reboot. The running system keeps working
//!    until then (it is the renamed subvolume).
//!
//! The mounts and unmounts of steps 1, 3 and 4 run in transient units outside rynasd's sandbox
//! ([`rynas_tasks::Outside`], units `rynas-rollback-*`): rynasd's seccomp filter has no mount
//! calls, and the boot layer's tool (outside too) must see the restore point's mount. They
//! propagate into rynasd's mount namespace, where the btrfs steps run (ioctls and renames).
//! Living in the host's namespace, they outlast rynasd: whatever an interrupted rollback left
//! below `/run/rynas/rollback` is unmounted ([`clear_mounts`], deepest first) when rynasd
//! starts and before a rollback mounts anything, and a mount whose command failed is
//! unmounted if it happened anyway.
//!
//! Every effect goes through [`RootOps`]: [`HostRoot`] on the machine, [`DryRoot`] for
//! `rynasd --dry-system` (logs only), a recording fake in the tests.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rynas_tasks::{Cmd, Outside};

use crate::ioctl;
use crate::name::stamp_seconds;
use crate::system::{self, SystemSnapshot};

/// Where the rollback mounts what it needs.
pub const RUN_DIR: &str = "/run/rynas/rollback";
/// Reason of the restore point kept of the system being replaced.
pub const BEFORE_REASON: &str = "before rollback";
const MOUNT_TIMEOUT: Duration = Duration::from_secs(30);

/// The kernel a root tree boots.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Kernel {
    /// `uname -r` of the kernel image, e.g. `7.3.0-1-amd64`.
    pub release: Option<String>,
    /// `/lib/modules/<release>` exists in the tree.
    pub modules: bool,
}

/// Compares kernel releases by their numeric parts (`7.10.0` after `7.9.2`).
fn release_key(r: &str) -> Vec<(u64, String)> {
    r.split(['.', '-', '+', '~'])
        .map(|p| {
            let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
            (digits.parse().unwrap_or(0), p[digits.len()..].to_string())
        })
        .collect()
}

/// Whether `rel` looks like a kernel release (no path tricks).
fn valid_release(rel: &str) -> bool {
    !rel.is_empty()
        && rel.len() <= 128
        && rel != "."
        && rel != ".."
        && rel
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+' | b'~'))
}

/// The kernel of the root tree at `root` (a snapshot or its mount): the target of
/// `/vmlinuz` (Debian's link) or else the newest `/boot/vmlinuz-*`, and whether its modules
/// are there. Links are read, never followed out of `root`.
pub fn snapshot_kernel(root: &Path) -> Kernel {
    let from_link = std::fs::read_link(root.join("vmlinuz"))
        .ok()
        .and_then(|t| {
            t.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("vmlinuz-"))
                .map(str::to_string)
        })
        .filter(|r| valid_release(r));
    let release = from_link.or_else(|| {
        let mut v: Vec<String> = std::fs::read_dir(root.join("boot"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_prefix("vmlinuz-"))
                    .map(str::to_string)
            })
            .filter(|r| valid_release(r))
            .collect();
        v.sort_by_key(|r| release_key(r));
        v.pop()
    });
    let modules = release.as_ref().is_some_and(|r| {
        // merged /usr: /lib is the relative link `usr/lib`; an absolute one would point
        // at the running system, so only a real directory counts
        root.join("usr/lib/modules").join(r).is_dir()
            || (std::fs::symlink_metadata(root.join("lib")).is_ok_and(|m| m.is_dir())
                && root.join("lib/modules").join(r).is_dir())
    });
    Kernel { release, modules }
}

/// The boot half of a root rollback: points the boot entry at the kernel of a root tree
/// (a hardware module's boot layer, `rynas_module::BootLayer`).
pub trait BootSync: Send + Sync {
    /// The layer's tool, for the task log (`<tool>: …`).
    fn tool(&self) -> &str;
    /// Rewrites the boot entry from the kernel of the system at `root`; refuses when `root`
    /// cannot boot. Its output (a few lines for the task log).
    fn sync_root(&self, root: &Path) -> Result<String, String>;
    /// What the admin runs when the boot entry could not be put back to the running system.
    fn undo_hint(&self) -> String;
}

/// Every effect of a root rollback on the filesystem.
pub trait RootOps: Send + Sync {
    /// `uname -r` of the running kernel.
    fn running_kernel(&self) -> Option<String>;
    /// The kernel of the tree mounted at `dir` (see [`snapshot_kernel`]).
    fn kernel(&self, dir: &Path) -> Kernel {
        snapshot_kernel(dir)
    }
    /// Read-only bind mount of `src` at `dir` (created).
    fn mount_ro(&self, src: &Path, dir: &Path) -> Result<(), String>;
    /// Mounts the top level (subvolid 5) of the root filesystem at `dir` (created); returns
    /// the name of the root subvolume there (`@rootfs`).
    fn mount_top(&self, dir: &Path) -> Result<String, String>;
    /// Unmounts `dir` and removes it.
    fn unmount(&self, dir: &Path) -> Result<(), String>;
    /// Unmounts whatever is mounted at or below `dir` (left by an interrupted rollback, or by
    /// a mount whose command failed after mounting), deepest first; nothing mounted: nothing
    /// done. `log` gets one line per mount. Err when one stays mounted.
    fn clear_mounts(&self, _dir: &Path, _log: &dyn Fn(&str)) -> Result<(), String> {
        Ok(())
    }
    /// A read-only restore point of the running system, never skipped; its name.
    fn restore_point(&self, reason: &str, now_s: i64) -> Result<String, String>;
    /// Writable snapshot of `src` as `dir/name`.
    fn snapshot(&self, src: &Path, dir: &Path, name: &str) -> Result<(), String>;
    /// Names of the subvolumes directly in `dir`.
    fn subvolumes(&self, dir: &Path) -> Result<Vec<String>, String>;
    /// Moves the subvolume `from` to `to` (another directory of the same filesystem); an
    /// empty placeholder directory at `to` (what a snapshot shows for a nested subvolume) is
    /// removed first.
    fn move_subvolume(&self, from: &Path, to: &Path) -> Result<(), String>;
    /// `RENAME_NOREPLACE` inside `dir`, then fsync of `dir`.
    fn rename(&self, dir: &Path, from: &str, to: &str) -> Result<(), String>;
    /// Deletes the subvolume `dir/name`.
    fn delete_subvolume(&self, dir: &Path, name: &str) -> Result<(), String>;
}

/// What [`rollback_root`] needs.
#[derive(Debug, Clone)]
pub struct RootRollback {
    /// The restore point name (`rootfs.20261101T0300`).
    pub id: String,
    /// Its path on the running system (`/.snapshots/rootfs/<id>`).
    pub snapshot: PathBuf,
    /// The restore points directory on the running system (`/.snapshots/rootfs`).
    pub snapshot_dir: PathBuf,
    /// Where the root subvolume is mounted (`/`, the `subvolume` of the restore points).
    pub root: PathBuf,
    /// Move the restore points into the new root (the directory is inside the root
    /// subvolume, not a separate subvolume or mount).
    pub move_points: bool,
    /// Mount directory (`/run/rynas/rollback`).
    pub run_dir: PathBuf,
    pub now_s: i64,
}

/// What a root rollback did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolledBackRoot {
    /// The kernel that boots next.
    pub kernel: String,
    /// The restore point of the replaced system.
    pub before: String,
    /// The replaced root subvolume, at the top of the filesystem.
    pub kept_as: String,
    /// Restore points moved into the new root.
    pub moved: usize,
}

/// Why a root rollback is refused without a boot layer.
pub const NO_BOOT_LAYER: &str = "this system has no boot layer rynas can point at a restore point's kernel: rolling the system back is a console procedure";

/// See the module documentation. Errors are complete sentences for the task log.
pub fn rollback_root(
    ops: &dyn RootOps,
    boot: Option<&dyn BootSync>,
    r: &RootRollback,
    log: &dyn Fn(&str),
) -> Result<RolledBackRoot, String> {
    let Some(boot) = boot else {
        return Err(NO_BOOT_LAYER.into());
    };
    // mounts of an earlier rollback interrupted by a rynasd restart: never mount on top
    ops.clear_mounts(&r.run_dir, log).map_err(|e| {
        format!(
            "leftover mounts below {}: {e}. Nothing was changed.",
            r.run_dir.display()
        )
    })?;
    let mnt = r.run_dir.join(&r.id);
    log(&format!(
        "mounting {} read-only at {}",
        r.snapshot.display(),
        mnt.display()
    ));
    if let Err(e) = ops.mount_ro(&r.snapshot, &mnt) {
        // the command may have failed after mounting (e.g. the client's timeout)
        if let Err(e2) = ops.clear_mounts(&mnt, log) {
            log(&format!("warning: {e2}"));
        }
        return Err(e);
    }
    let out = with_snapshot(ops, boot, r, &mnt, log);
    if let Err(e) = ops.unmount(&mnt) {
        log(&format!("warning: unmounting {}: {e}", mnt.display()));
    }
    out
}

fn with_snapshot(
    ops: &dyn RootOps,
    boot: &dyn BootSync,
    r: &RootRollback,
    mnt: &Path,
    log: &dyn Fn(&str),
) -> Result<RolledBackRoot, String> {
    let k = ops.kernel(mnt);
    let Some(kernel) = k.release else {
        return Err(format!(
            "{} has no kernel (/vmlinuz or /boot/vmlinuz-*): nothing was changed",
            r.id
        ));
    };
    if !k.modules {
        return Err(format!(
            "{} has kernel {kernel} but not /lib/modules/{kernel}: it cannot boot; nothing was changed",
            r.id
        ));
    }
    log(&format!("the restore point boots kernel {kernel}"));
    let tool = boot.tool();
    log(&format!("{tool}: boot entry from {}", mnt.display()));
    match boot.sync_root(mnt) {
        Ok(o) => {
            for l in o.lines().filter(|l| !l.trim().is_empty()).take(20) {
                log(&format!("  {l}"));
            }
        }
        Err(e) => {
            return Err(format!(
                "{tool} refused the restore point: {e}. Nothing was changed."
            ));
        }
    }
    match swap_root(ops, r, log) {
        Ok(mut done) => {
            done.kernel = kernel;
            Ok(done)
        }
        Err(e) => {
            log("putting the boot entry back to the running system");
            match boot.sync_root(Path::new("/")) {
                Ok(_) => Err(format!("{e}. The system was left as it was.")),
                Err(e2) => Err(format!(
                    "{e}. The root was left as it was, but {tool} could not restore the boot entry ({e2}): {} before rebooting.",
                    boot.undo_hint()
                )),
            }
        }
    }
}

/// Step 3: the btrfs part, all undone on failure.
fn swap_root(
    ops: &dyn RootOps,
    r: &RootRollback,
    log: &dyn Fn(&str),
) -> Result<RolledBackRoot, String> {
    let top = r.run_dir.join("top");
    log(&format!(
        "mounting the top level of the root filesystem at {}",
        top.display()
    ));
    let root_name = match ops.mount_top(&top) {
        Ok(n) => n,
        Err(e) => {
            if let Err(e2) = ops.clear_mounts(&top, log) {
                log(&format!("warning: {e2}"));
            }
            return Err(e);
        }
    };
    let out = in_top(ops, r, &top, &root_name, log);
    if let Err(e) = ops.unmount(&top) {
        log(&format!("warning: unmounting {}: {e}", top.display()));
    }
    out
}

fn in_top(
    ops: &dyn RootOps,
    r: &RootRollback,
    top: &Path,
    root_name: &str,
    log: &dyn Fn(&str),
) -> Result<RolledBackRoot, String> {
    let rel = r
        .snapshot_dir
        .strip_prefix(&r.root)
        .map_err(|_| "the restore points directory is not inside the root subvolume".to_string())?;
    let before = ops
        .restore_point(BEFORE_REASON, r.now_s)
        .map_err(|e| format!("restore point of the current system: {e}"))?;
    log(&format!(
        "the current system is kept as restore point {before}"
    ));
    let stamp = stamp_seconds(r.now_s);
    let fresh = format!("{root_name}.rynas-new-{stamp}");
    let kept = format!("{root_name}.rynas-rollback-{stamp}");
    let old_root = top.join(root_name);
    ops.snapshot(&old_root.join(rel).join(&r.id), top, &fresh)
        .map_err(|e| format!("writable copy of {}: {e}", r.id))?;
    log(&format!("created {fresh} from {}", r.id));
    let undo_fresh = |why: String| -> String {
        if let Err(e) = ops.delete_subvolume(top, &fresh) {
            log(&format!("warning: deleting {fresh}: {e}"));
        }
        why
    };
    let mut moved: Vec<String> = Vec::new();
    if r.move_points {
        let names = ops
            .subvolumes(&old_root.join(rel))
            .map_err(|e| undo_fresh(format!("listing the restore points: {e}")))?;
        for n in names {
            let (from, to) = (
                old_root.join(rel).join(&n),
                top.join(&fresh).join(rel).join(&n),
            );
            if let Err(e) = ops.move_subvolume(&from, &to) {
                for m in moved.iter().rev() {
                    let _ = ops.move_subvolume(
                        &top.join(&fresh).join(rel).join(m),
                        &old_root.join(rel).join(m),
                    );
                }
                return Err(undo_fresh(format!("moving restore point {n}: {e}")));
            }
            moved.push(n);
        }
        log(&format!(
            "{} restore points moved into the new root",
            moved.len()
        ));
    }
    // the system volume's data (D-ROOTVOL) is the top-level subvolume @rynas-system, not
    // inside the root: a rollback leaves it alone, nothing to carry
    let move_back = || {
        for m in moved.iter().rev() {
            let _ = ops.move_subvolume(
                &top.join(&fresh).join(rel).join(m),
                &old_root.join(rel).join(m),
            );
        }
    };
    if let Err(e) = ops.rename(top, root_name, &kept) {
        move_back();
        return Err(undo_fresh(format!("renaming {root_name}: {e}")));
    }
    if let Err(e) = ops.rename(top, &fresh, root_name) {
        let _ = ops.rename(top, &kept, root_name);
        move_back();
        return Err(undo_fresh(format!("moving {fresh} in place: {e}")));
    }
    log(&format!(
        "{root_name} is now {}; the replaced root is kept as {kept} at the top of the filesystem",
        r.id
    ));
    Ok(RolledBackRoot {
        kernel: String::new(),
        before,
        kept_as: kept,
        moved: moved.len(),
    })
}

// ---------------------------------------------------------------------------------------------
// the machine
// ---------------------------------------------------------------------------------------------

/// The root filesystem from `/proc/self/mountinfo`: (subvolume path, source device, fstype).
fn root_mount() -> Option<(String, String, String)> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    text.lines().find_map(|l| {
        let (pre, post) = l.split_once(" - ")?;
        let f: Vec<&str> = pre.split(' ').collect();
        if f.get(4) != Some(&"/") {
            return None;
        }
        let mut p = post.split(' ');
        let fstype = p.next()?.to_string();
        let source = p.next()?.to_string();
        Some((f.get(3)?.to_string(), source, fstype))
    })
}

fn run(argv: Vec<OsString>) -> Result<String, String> {
    let shown = shown(&argv);
    outside(argv)
        .run_blocking()
        .map(|o| o.stdout)
        .map_err(|e| failure(&shown, &e))
}

/// The inner command line for errors and logs (not the `systemd-run` wrapper).
fn shown(argv: &[OsString]) -> String {
    argv.iter()
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `mount …: exit code N: <last stderr line>` (or the timeout, the spawn error).
fn failure(shown: &str, e: &rynas_tasks::CmdError) -> String {
    format!("{shown}: {e}")
}

/// Prefix of the transient units the rollback mounts and unmounts in.
pub const UNIT_PREFIX: &str = "rynas-rollback";

/// `argv` in a transient unit outside rynasd's sandbox ([`rynas_tasks::Outside`]): its
/// seccomp filter has no `mount`/`umount2`, and a mount made in its own namespace would stay
/// invisible to the boot layer's tool, which runs outside too.
fn outside(argv: Vec<OsString>) -> Cmd {
    Outside::new(UNIT_PREFIX, "rynas: system rollback", MOUNT_TIMEOUT).cmd(argv)
}

/// Read-only bind mount of the restore point `src` at `dir`.
pub fn mount_ro_argv(src: &Path, dir: &Path) -> Vec<OsString> {
    vec!["mount".into(), "-o".into(), "bind,ro".into(), src.into(), dir.into()]
}

/// The top level (subvolid 5) of the root filesystem on `source` at `dir`.
pub fn mount_top_argv(source: &str, dir: &Path) -> Vec<OsString> {
    vec![
        "mount".into(),
        "-t".into(),
        "btrfs".into(),
        "-o".into(),
        "subvolid=5,noatime".into(),
        source.into(),
        dir.into(),
    ]
}

pub fn umount_argv(dir: &Path) -> Vec<OsString> {
    vec!["umount".into(), dir.into()]
}

/// Lazy unmount of `dir`: only when the plain one failed on a leftover mount.
pub fn umount_lazy_argv(dir: &Path) -> Vec<OsString> {
    vec!["umount".into(), "-l".into(), dir.into()]
}

/// A mount point field of `/proc/self/mountinfo` decoded (`\040` space, `\011`, `\012`,
/// `\134`).
fn unescape_mountinfo(s: &str) -> PathBuf {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 4 <= b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = b[i + 1..i + 4]
                .iter()
                .fold(0u32, |a, c| a * 8 + u32::from(c - b'0'));
            if let Ok(v) = u8::try_from(v) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    PathBuf::from(<OsString as std::os::unix::ffi::OsStringExt>::from_vec(out))
}

/// The mount points at or below `dir` in `mountinfo` (the text of `/proc/self/mountinfo`),
/// in the order they must be unmounted: deepest first, and of mounts stacked on one path
/// the last mounted first (one entry per mount: a path mounted twice is listed twice).
pub fn mounts_under(mountinfo: &str, dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<(usize, PathBuf)> = mountinfo
        .lines()
        .filter_map(|l| l.split(' ').nth(4).map(unescape_mountinfo))
        .filter(|p| p.starts_with(dir))
        .enumerate()
        .collect();
    v.sort_by(|(i, a), (j, b)| {
        b.components()
            .count()
            .cmp(&a.components().count())
            .then(j.cmp(i))
    });
    v.into_iter().map(|(_, p)| p).collect()
}

/// [`RootOps::clear_mounts`] on the machine, also run when rynasd starts (on
/// [`RUN_DIR`]): every mount at or below `dir` is unmounted outside the sandbox
/// ([`outside`]), lazily (logged) only when the plain unmount fails; the emptied mount
/// points are removed. Errors name the mounts that stayed.
pub fn clear_mounts(dir: &Path, log: &dyn Fn(&str)) -> Result<(), String> {
    let text = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("/proc/self/mountinfo: {e}"))?;
    let mut stuck = Vec::new();
    for m in mounts_under(&text, dir) {
        log(&format!(
            "unmounting {} (left by an interrupted rollback)",
            m.display()
        ));
        if let Err(e) = run(umount_argv(&m)) {
            log(&format!("warning: {e}; detaching it lazily"));
            if let Err(e) = run(umount_lazy_argv(&m)) {
                stuck.push(e);
                continue;
            }
        }
        if m != dir {
            let _ = std::fs::remove_dir(&m);
        }
    }
    if stuck.is_empty() {
        Ok(())
    } else {
        Err(stuck.join("; "))
    }
}

/// The real thing (rynasd as root).
pub struct HostRoot {
    /// The restore point settings (subvolume `/`, directory, prefix, keep, reasons log).
    pub subvolume: PathBuf,
    pub snapshot_dir: PathBuf,
    pub prefix: String,
    pub keep: u32,
    pub reasons: PathBuf,
}

impl RootOps for HostRoot {
    fn running_kernel(&self) -> Option<String> {
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn mount_ro(&self, src: &Path, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        run(mount_ro_argv(src, dir)).map(drop)
    }

    fn mount_top(&self, dir: &Path) -> Result<String, String> {
        if self.subvolume != Path::new("/") {
            return Err(format!(
                "restore points of {} are not of the running root (/)",
                self.subvolume.display()
            ));
        }
        let (subvol, source, fstype) =
            root_mount().ok_or("the root filesystem is not in /proc/self/mountinfo")?;
        if fstype != "btrfs" {
            return Err(format!("the root filesystem is {fstype}, not btrfs"));
        }
        let name = subvol.trim_start_matches('/').to_string();
        if name.is_empty() || name.contains('/') {
            return Err(format!(
                "the root subvolume {subvol} is not directly below the top level"
            ));
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        run(mount_top_argv(&source, dir))?;
        Ok(name)
    }

    fn unmount(&self, dir: &Path) -> Result<(), String> {
        run(umount_argv(dir))?;
        let _ = std::fs::remove_dir(dir);
        Ok(())
    }

    fn clear_mounts(&self, dir: &Path, log: &dyn Fn(&str)) -> Result<(), String> {
        clear_mounts(dir, log)
    }

    fn restore_point(&self, reason: &str, now_s: i64) -> Result<String, String> {
        system::take_always(
            &SystemSnapshot {
                subvolume: &self.subvolume,
                snapshot_dir: &self.snapshot_dir,
                prefix: &self.prefix,
                keep: self.keep,
                reasons: Some(&self.reasons),
            },
            reason,
            now_s,
        )
        .map_err(|e| e.to_string())
    }

    fn snapshot(&self, src: &Path, dir: &Path, name: &str) -> Result<(), String> {
        let s = ioctl::open_dir(src).map_err(|e| format!("{}: {e}", src.display()))?;
        let d = ioctl::open_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        ioctl::create_snapshot(&s, &d, name, false).map_err(|e| e.to_string())
    }

    fn subvolumes(&self, dir: &Path) -> Result<Vec<String>, String> {
        let d = ioctl::open_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut v = Vec::new();
        for e in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
            let Some(n) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if ioctl::open_dir_at(&d, &n).is_ok_and(|f| ioctl::is_subvolume(&f)) {
                v.push(n);
            }
        }
        v.sort();
        Ok(v)
    }

    fn move_subvolume(&self, from: &Path, to: &Path) -> Result<(), String> {
        if let Ok(m) = std::fs::symlink_metadata(to) {
            let placeholder = m.is_dir()
                && ioctl::open_dir(to).is_ok_and(|f| !ioctl::is_subvolume(&f))
                && std::fs::read_dir(to).is_ok_and(|mut d| d.next().is_none());
            if !placeholder {
                return Err(format!("{} already exists", to.display()));
            }
            std::fs::remove_dir(to).map_err(|e| format!("{}: {e}", to.display()))?;
        }
        if let Some(p) = to.parent() {
            std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        let c = |p: &Path| {
            std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str()))
                .map_err(|e| e.to_string())
        };
        let (f, t) = (c(from)?, c(to)?);
        // SAFETY: two valid C strings; AT_FDCWD with absolute paths
        let rc = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                f.as_ptr(),
                libc::AT_FDCWD,
                t.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }

    fn rename(&self, dir: &Path, from: &str, to: &str) -> Result<(), String> {
        let d: File = ioctl::open_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        ioctl::rename_noreplace(&d, from, to).map_err(|e| e.to_string())?;
        // SAFETY: valid descriptor
        unsafe {
            libc::fsync(d.as_raw_fd());
        }
        Ok(())
    }

    fn delete_subvolume(&self, dir: &Path, name: &str) -> Result<(), String> {
        let d = ioctl::open_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        ioctl::destroy_subvolume(&d, name).map_err(|e| e.to_string())
    }
}

/// `rynasd --dry-system`: every step is logged, nothing is mounted, run or renamed. The
/// kernel is read from the restore point itself.
#[derive(Default)]
pub struct DryRoot {
    mounts: Mutex<HashMap<PathBuf, PathBuf>>,
}

impl DryRoot {
    pub fn new() -> Self {
        Self::default()
    }
}

impl RootOps for DryRoot {
    fn running_kernel(&self) -> Option<String> {
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn kernel(&self, dir: &Path) -> Kernel {
        let src = self.mounts.lock().unwrap().get(dir).cloned();
        snapshot_kernel(src.as_deref().unwrap_or(dir))
    }

    fn mount_ro(&self, src: &Path, dir: &Path) -> Result<(), String> {
        tracing::info!("dry: mount -o bind,ro {} {}", src.display(), dir.display());
        self.mounts
            .lock()
            .unwrap()
            .insert(dir.to_path_buf(), src.to_path_buf());
        Ok(())
    }

    fn mount_top(&self, dir: &Path) -> Result<String, String> {
        tracing::info!(
            "dry: mount -t btrfs -o subvolid=5 <root device> {}",
            dir.display()
        );
        Ok("@rootfs".into())
    }

    fn unmount(&self, dir: &Path) -> Result<(), String> {
        tracing::info!("dry: umount {}", dir.display());
        self.mounts.lock().unwrap().remove(dir);
        Ok(())
    }

    fn restore_point(&self, reason: &str, now_s: i64) -> Result<String, String> {
        let name = format!("rootfs.{}", crate::name::long_timestamp(now_s));
        tracing::info!("dry: restore point {name} ({reason})");
        Ok(name)
    }

    fn snapshot(&self, src: &Path, dir: &Path, name: &str) -> Result<(), String> {
        tracing::info!(
            "dry: btrfs subvolume snapshot {} {}/{name}",
            src.display(),
            dir.display()
        );
        Ok(())
    }

    fn subvolumes(&self, _dir: &Path) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    fn move_subvolume(&self, from: &Path, to: &Path) -> Result<(), String> {
        tracing::info!("dry: mv {} {}", from.display(), to.display());
        Ok(())
    }

    fn rename(&self, dir: &Path, from: &str, to: &str) -> Result<(), String> {
        tracing::info!("dry: mv {0}/{from} {0}/{to}", dir.display());
        Ok(())
    }

    fn delete_subvolume(&self, dir: &Path, name: &str) -> Result<(), String> {
        tracing::info!("dry: btrfs subvolume delete {}/{name}", dir.display());
        Ok(())
    }
}

/// The default of tests: every effect fails.
pub struct NoRoot;

impl RootOps for NoRoot {
    fn running_kernel(&self) -> Option<String> {
        None
    }
    fn mount_ro(&self, _: &Path, _: &Path) -> Result<(), String> {
        Err("not supported".into())
    }
    fn mount_top(&self, _: &Path) -> Result<String, String> {
        Err("not supported".into())
    }
    fn unmount(&self, _: &Path) -> Result<(), String> {
        Ok(())
    }
    fn restore_point(&self, _: &str, _: i64) -> Result<String, String> {
        Err("not supported".into())
    }
    fn snapshot(&self, _: &Path, _: &Path, _: &str) -> Result<(), String> {
        Err("not supported".into())
    }
    fn subvolumes(&self, _: &Path) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
    fn move_subvolume(&self, _: &Path, _: &Path) -> Result<(), String> {
        Err("not supported".into())
    }
    fn rename(&self, _: &Path, _: &str, _: &str) -> Result<(), String> {
        Err("not supported".into())
    }
    fn delete_subvolume(&self, _: &Path, _: &str) -> Result<(), String> {
        Err("not supported".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every call; `fail` names the call (prefix) that fails.
    struct Fake {
        calls: Mutex<Vec<String>>,
        fail: Option<&'static str>,
        kernel: Kernel,
        points: Vec<String>,
    }

    impl Fake {
        fn new(fail: Option<&'static str>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail,
                kernel: Kernel {
                    release: Some("7.3.0-1-synodeb".into()),
                    modules: true,
                },
                points: vec!["rootfs.20261020T0300".into(), "rootfs.20261101T0300".into()],
            }
        }
        fn rec(&self, c: String) -> Result<(), String> {
            let failing = self.fail.is_some_and(|f| c.starts_with(f));
            self.calls.lock().unwrap().push(c);
            if failing { Err("boom".into()) } else { Ok(()) }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl BootSync for Fake {
        fn tool(&self) -> &str {
            "boot-tool"
        }
        fn sync_root(&self, r: &Path) -> Result<String, String> {
            self.rec(format!("boot_sync {}", r.display()))
                .map(|_| "DEBIAN: 7.3.0-1-synodeb".into())
        }
        fn undo_hint(&self) -> String {
            "run `boot-tool sync --root /`".into()
        }
    }

    impl RootOps for Fake {
        fn running_kernel(&self) -> Option<String> {
            Some("7.4.0-1-synodeb".into())
        }
        fn kernel(&self, _: &Path) -> Kernel {
            self.kernel.clone()
        }
        fn mount_ro(&self, s: &Path, d: &Path) -> Result<(), String> {
            self.rec(format!("mount_ro {} {}", s.display(), d.display()))
        }
        fn mount_top(&self, d: &Path) -> Result<String, String> {
            self.rec(format!("mount_top {}", d.display()))
                .map(|_| "@rootfs".into())
        }
        fn unmount(&self, d: &Path) -> Result<(), String> {
            self.rec(format!("unmount {}", d.display()))
        }
        fn clear_mounts(&self, d: &Path, _: &dyn Fn(&str)) -> Result<(), String> {
            self.rec(format!("clear {}", d.display()))
        }
        fn restore_point(&self, reason: &str, _: i64) -> Result<String, String> {
            self.rec(format!("restore_point {reason}"))
                .map(|_| "rootfs.20261105T1200".into())
        }
        fn snapshot(&self, s: &Path, d: &Path, n: &str) -> Result<(), String> {
            self.rec(format!("snapshot {} {}/{n}", s.display(), d.display()))
        }
        fn subvolumes(&self, d: &Path) -> Result<Vec<String>, String> {
            self.rec(format!("subvolumes {}", d.display()))
                .map(|_| self.points.clone())
        }
        fn move_subvolume(&self, f: &Path, t: &Path) -> Result<(), String> {
            self.rec(format!("move {} {}", f.display(), t.display()))
        }
        fn rename(&self, d: &Path, f: &str, t: &str) -> Result<(), String> {
            self.rec(format!("rename {}: {f} -> {t}", d.display()))
        }
        fn delete_subvolume(&self, d: &Path, n: &str) -> Result<(), String> {
            self.rec(format!("delete {}/{n}", d.display()))
        }
    }

    fn plan() -> RootRollback {
        RootRollback {
            id: "rootfs.20261101T0300".into(),
            snapshot: "/.snapshots/rootfs/rootfs.20261101T0300".into(),
            snapshot_dir: "/.snapshots/rootfs".into(),
            root: "/".into(),
            move_points: true,
            run_dir: "/run/rynas/rollback".into(),
            now_s: 1_793_880_000,
        }
    }

    #[test]
    fn full_rollback_in_order() {
        let f = Fake::new(None);
        let r = rollback_root(&f, Some(&f), &plan(), &|_| {}).unwrap();
        assert_eq!(r.kernel, "7.3.0-1-synodeb");
        assert_eq!(r.before, "rootfs.20261105T1200");
        assert!(
            r.kept_as.starts_with("@rootfs.rynas-rollback-"),
            "{}",
            r.kept_as
        );
        assert_eq!(r.moved, 2);
        let c = f.calls();
        let stamp = stamp_seconds(plan().now_s);
        let want = [
            "clear /run/rynas/rollback".to_string(),
            "mount_ro /.snapshots/rootfs/rootfs.20261101T0300 /run/rynas/rollback/rootfs.20261101T0300".into(),
            "boot_sync /run/rynas/rollback/rootfs.20261101T0300".into(),
            "mount_top /run/rynas/rollback/top".into(),
            "restore_point before rollback".into(),
            format!("snapshot /run/rynas/rollback/top/@rootfs/.snapshots/rootfs/rootfs.20261101T0300 /run/rynas/rollback/top/@rootfs.rynas-new-{stamp}"),
            "subvolumes /run/rynas/rollback/top/@rootfs/.snapshots/rootfs".into(),
            format!("move /run/rynas/rollback/top/@rootfs/.snapshots/rootfs/rootfs.20261020T0300 /run/rynas/rollback/top/@rootfs.rynas-new-{stamp}/.snapshots/rootfs/rootfs.20261020T0300"),
            format!("move /run/rynas/rollback/top/@rootfs/.snapshots/rootfs/rootfs.20261101T0300 /run/rynas/rollback/top/@rootfs.rynas-new-{stamp}/.snapshots/rootfs/rootfs.20261101T0300"),
            format!("rename /run/rynas/rollback/top: @rootfs -> @rootfs.rynas-rollback-{stamp}"),
            format!("rename /run/rynas/rollback/top: @rootfs.rynas-new-{stamp} -> @rootfs"),
            "unmount /run/rynas/rollback/top".into(),
            "unmount /run/rynas/rollback/rootfs.20261101T0300".into(),
        ];
        assert_eq!(c, want);
    }

    #[test]
    fn a_boot_refusal_changes_nothing() {
        let f = Fake::new(Some("boot_sync"));
        let e = rollback_root(&f, Some(&f), &plan(), &|_| {}).unwrap_err();
        assert!(e.contains("boot-tool refused") && e.contains("boom"), "{e}");
        let c = f.calls();
        assert_eq!(c.len(), 4, "{c:?}");
        assert!(c[3].starts_with("unmount /run/rynas/rollback/rootfs."));
    }

    #[test]
    fn leftover_mounts_that_stay_stop_everything() {
        let f = Fake::new(Some("clear /run/rynas/rollback"));
        let e = rollback_root(&f, Some(&f), &plan(), &|_| {}).unwrap_err();
        assert!(e.contains("leftover mounts") && e.contains("Nothing was changed"), "{e}");
        assert_eq!(f.calls(), ["clear /run/rynas/rollback"]);
    }

    #[test]
    fn a_failed_mount_is_cleared_in_case_it_happened() {
        let f = Fake::new(Some("mount_ro"));
        rollback_root(&f, Some(&f), &plan(), &|_| {}).unwrap_err();
        let c = f.calls();
        assert_eq!(c.len(), 3, "{c:?}");
        assert_eq!(c[2], "clear /run/rynas/rollback/rootfs.20261101T0300");
        let f = Fake::new(Some("mount_top"));
        rollback_root(&f, Some(&f), &plan(), &|_| {}).unwrap_err();
        let c = f.calls();
        let i = c.iter().position(|x| x.starts_with("mount_top")).unwrap();
        assert_eq!(c[i + 1], "clear /run/rynas/rollback/top");
        assert_eq!(c[i + 2], "boot_sync /");
        assert!(c[i + 3].starts_with("unmount /run/rynas/rollback/rootfs."));
    }

    #[test]
    fn leftover_mounts_deepest_and_latest_first() {
        // a rollback interrupted twice: top mounted twice, a restore point, a mount with a
        // space; the root, other /run mounts and a sibling prefix stay out
        let mi = "\
22 1 0:21 /@rootfs / rw,noatime shared:1 - btrfs /dev/nvme0n1p2 rw
30 22 0:25 / /run rw,nosuid shared:5 - tmpfs tmpfs rw
41 30 0:21 / /run/rynas/rollback/top rw shared:1 - btrfs /dev/nvme0n1p2 rw,subvolid=5
42 30 0:21 /@rootfs/.snapshots/rootfs/rootfs.20261101T0300 /run/rynas/rollback/rootfs.20261101T0300 ro shared:1 - btrfs /dev/nvme0n1p2 rw
43 30 0:21 / /run/rynas/rollback/top rw shared:1 - btrfs /dev/nvme0n1p2 rw,subvolid=5
44 41 0:30 / /run/rynas/rollback/top/@rootfs/x rw - tmpfs tmpfs rw
45 30 0:31 / /run/rynas/rollback\\040odd rw - tmpfs tmpfs rw
46 30 0:32 / /run/rynas/rollback2 rw - tmpfs tmpfs rw
47 30 0:33 / /run/rynas/rollback/a\\040b rw - tmpfs tmpfs rw
";
        let got = mounts_under(mi, Path::new(RUN_DIR));
        let want: Vec<PathBuf> = [
            "/run/rynas/rollback/top/@rootfs/x",
            "/run/rynas/rollback/a b",
            "/run/rynas/rollback/top",
            "/run/rynas/rollback/rootfs.20261101T0300",
            "/run/rynas/rollback/top",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(got, want);
        // one directory: only its own mounts
        assert_eq!(
            mounts_under(mi, Path::new("/run/rynas/rollback/rootfs.20261101T0300")),
            [PathBuf::from("/run/rynas/rollback/rootfs.20261101T0300")]
        );
        assert!(mounts_under("", Path::new(RUN_DIR)).is_empty());
    }

    #[test]
    fn errors_show_the_inner_command() {
        let argv = mount_ro_argv(Path::new("/s"), Path::new("/run/rynas/rollback/x"));
        let e = rynas_tasks::CmdError::Failed(rynas_tasks::CmdOutput {
            code: Some(32),
            stdout: String::new(),
            stderr: "mount: /run/rynas/rollback/x: special device /s does not exist.\n".into(),
        });
        assert_eq!(
            failure(&shown(&argv), &e),
            "mount -o bind,ro /s /run/rynas/rollback/x: exit code 32: mount: /run/rynas/rollback/x: special device /s does not exist."
        );
    }

    #[test]
    fn missing_modules_stop_before_the_boot_entry() {
        let mut f = Fake::new(None);
        f.kernel.modules = false;
        let e = rollback_root(&f, Some(&f), &plan(), &|_| {}).unwrap_err();
        assert!(e.contains("/lib/modules/7.3.0-1-synodeb"), "{e}");
        assert!(!f.calls().iter().any(|c| c.starts_with("boot_sync")));
    }

    #[test]
    fn a_failed_swap_is_undone_and_the_boot_entry_synced_back() {
        // the second rename fails: the first is undone, restore points move back, the copy
        // is deleted, the boot entry goes back to the running system
        let f = Fake::new(Some("rename /run/rynas/rollback/top: @rootfs.rynas-new"));
        let e = rollback_root(&f, Some(&f), &plan(), &|_| {}).unwrap_err();
        assert!(e.contains("left as it was"), "{e}");
        let c = f.calls();
        let stamp = stamp_seconds(plan().now_s);
        let i = c
            .iter()
            .position(|x| x.starts_with("rename /run/rynas/rollback/top: @rootfs.rynas-new"))
            .unwrap();
        assert_eq!(
            c[i + 1],
            format!("rename /run/rynas/rollback/top: @rootfs.rynas-rollback-{stamp} -> @rootfs")
        );
        assert!(
            c[i + 2].starts_with("move /run/rynas/rollback/top/@rootfs.rynas-new-")
                && c[i + 2].ends_with("/.snapshots/rootfs/rootfs.20261101T0300")
        );
        assert!(c[i + 3].ends_with("/.snapshots/rootfs/rootfs.20261020T0300"));
        assert_eq!(
            c[i + 4],
            format!("delete /run/rynas/rollback/top/@rootfs.rynas-new-{stamp}")
        );
        assert_eq!(c[i + 5], "unmount /run/rynas/rollback/top");
        assert_eq!(c[i + 6], "boot_sync /");
        assert!(c[i + 7].starts_with("unmount /run/rynas/rollback/rootfs."));
    }

    #[test]
    fn mounts_run_outside_the_sandbox() {
        let at = |c: &Cmd| {
            let s = c.display();
            let (head, tail) = s.split_once(" -- ").expect("systemd-run ... -- argv");
            assert!(head.starts_with("systemd-run --wait --pipe --collect"), "{s}");
            assert!(head.contains(&format!("--unit={UNIT_PREFIX}-")), "{s}");
            assert!(head.contains("--property=RuntimeMaxSec=30"), "{s}");
            tail.to_string()
        };
        let snap = Path::new("/.snapshots/rootfs/rootfs.20261101T0300");
        let mnt = Path::new("/run/rynas/rollback/rootfs.20261101T0300");
        assert_eq!(
            at(&outside(mount_ro_argv(snap, mnt))),
            "mount -o bind,ro /.snapshots/rootfs/rootfs.20261101T0300 /run/rynas/rollback/rootfs.20261101T0300"
        );
        let top = Path::new("/run/rynas/rollback/top");
        assert_eq!(
            at(&outside(mount_top_argv("/dev/nvme0n1p2", top))),
            "mount -t btrfs -o subvolid=5,noatime /dev/nvme0n1p2 /run/rynas/rollback/top"
        );
        assert_eq!(at(&outside(umount_argv(top))), "umount /run/rynas/rollback/top");
        assert_eq!(at(&outside(umount_lazy_argv(top))), "umount -l /run/rynas/rollback/top");
        // a path that looks like an option stays a separate argument after `--`
        let odd = mount_ro_argv(Path::new("/s/-o x"), mnt);
        assert_eq!(odd[3], OsString::from("/s/-o x"));
    }

    #[test]
    fn refused_without_a_boot_layer() {
        let e = rollback_root(&NoRoot, None, &plan(), &|_| {}).unwrap_err();
        assert_eq!(e, NO_BOOT_LAYER);
    }

    #[test]
    fn kernel_of_a_tree() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        assert_eq!(snapshot_kernel(r), Kernel::default());
        std::fs::create_dir_all(r.join("boot")).unwrap();
        for k in ["7.2.0-1-synodeb", "7.10.0-1-synodeb", "7.9.1-2-synodeb"] {
            std::fs::write(r.join("boot").join(format!("vmlinuz-{k}")), "").unwrap();
        }
        assert_eq!(
            snapshot_kernel(r),
            Kernel {
                release: Some("7.10.0-1-synodeb".into()),
                modules: false
            }
        );
        // merged /usr with the relative link
        std::fs::create_dir_all(r.join("usr/lib/modules/7.10.0-1-synodeb")).unwrap();
        std::os::unix::fs::symlink("usr/lib", r.join("lib")).unwrap();
        assert!(snapshot_kernel(r).modules);
        // Debian's /vmlinuz link wins over the newest image
        std::os::unix::fs::symlink("boot/vmlinuz-7.2.0-1-synodeb", r.join("vmlinuz")).unwrap();
        assert_eq!(
            snapshot_kernel(r).release.as_deref(),
            Some("7.2.0-1-synodeb")
        );
        assert!(!snapshot_kernel(r).modules);
        assert!(!valid_release("../x") && !valid_release("") && valid_release("7.3.0-1-synodeb"));
    }

    #[test]
    fn dry_root_reads_the_kernel_of_the_source() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("boot")).unwrap();
        std::fs::write(d.path().join("boot/vmlinuz-7.3.0-1-synodeb"), "").unwrap();
        std::fs::create_dir_all(d.path().join("usr/lib/modules/7.3.0-1-synodeb")).unwrap();
        let mut p = plan();
        p.snapshot = d.path().to_path_buf();
        p.run_dir = d.path().join("run");
        struct Logged;
        impl BootSync for Logged {
            fn tool(&self) -> &str {
                "boot-tool"
            }
            fn sync_root(&self, _: &Path) -> Result<String, String> {
                Ok("dry".into())
            }
            fn undo_hint(&self) -> String {
                String::new()
            }
        }
        let r = rollback_root(&DryRoot::new(), Some(&Logged), &p, &|_| {}).unwrap();
        assert_eq!(r.kernel, "7.3.0-1-synodeb");
        assert!(
            !d.path().join("run").exists(),
            "nothing created in dry mode"
        );
    }
}
