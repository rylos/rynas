//! Carries out a [`Plan`]: shared by every restore source.
//!
//! GPT → LUKS → LVM groups and volumes (then LUKS inside volumes) → mkfs with the original
//! IDs → btrfs subvolumes and the default one → the data (the source fills the mounted tree, or a btrfs pool before the tree is mounted) → ID fixes
//! → chroot: `/etc/kernel/cmdline` from the manifest **before** any boot tool (`kernel-install`
//! in a chroot would read the live system's `/proc/cmdline`), the initramfs, the boot loader
//! and the removable EFI fallback → unmount → report.

use std::path::{Path, PathBuf};
use std::process::Command;

use rynas_api_types::pcagent::*;

use super::paths::{in_pool, resolve_in_root, safe_rel, under};
use super::plan::{Plan, mkfs_argv};
use super::source::{Fill, PoolTarget, RestoreSource, Version};

/// Outside `/run`, which the chroot step binds recursively.
pub const TARGET: &str = "/mnt/rynas-restore";
const TOPS: &str = "/mnt/rynas-restore-top";

/// The restore log: printed, and kept for the NAS and the target's `/var/log`.
pub struct Log {
    pub lines: Vec<String>,
    pub echo: Box<dyn Fn(&str)>,
    /// Checks that failed after the restore (IDs, boot files): no reboot is offered, the
    /// user is told what to look at.
    pub problems: Vec<String>,
}

impl Log {
    pub fn new(echo: Box<dyn Fn(&str)>) -> Log {
        Log { lines: Vec::new(), echo, problems: Vec::new() }
    }
    /// A failed check: logged, and remembered in [`Log::problems`].
    pub fn problem(&mut self, s: impl Into<String>) {
        let s = s.into();
        self.line(format!("check failed: {s}"));
        self.problems.push(s);
    }
    pub fn line(&mut self, s: impl Into<String>) {
        let s = s.into();
        (self.echo)(&s);
        self.lines.push(s);
    }
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

/// Why a command failed: its stderr, or else how it ended (exit status or signal, a
/// program killed by SIGILL prints nothing) and the end of its stdout.
fn failed(prog: &str, o: &std::process::Output) -> String {
    let err = String::from_utf8_lossy(&o.stderr);
    if !err.trim().is_empty() {
        return format!("{prog} failed: {}", err.trim());
    }
    format!("{prog} failed: {}{}", ended(o.status), stdout_tail(&String::from_utf8_lossy(&o.stdout)))
}

/// How a process ended: `exit status N` or `killed by signal N (NAME)`.
fn ended(s: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (s.code(), s.signal()) {
        (Some(c), _) => format!("exit status {c}"),
        (None, Some(sig)) => {
            let name = match sig {
                4 => " (SIGILL: an instruction this CPU does not have)",
                6 => " (SIGABRT)",
                7 => " (SIGBUS)",
                9 => " (SIGKILL)",
                11 => " (SIGSEGV)",
                _ => "",
            };
            format!("killed by signal {sig}{name}")
        }
        _ => "no exit status".into(),
    }
}

/// The last lines of a command's stdout, for an error that has no stderr.
fn stdout_tail(out: &str) -> String {
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return String::new();
    }
    format!("; last output: {}", lines[lines.len().saturating_sub(5)..].join(" | "))
}

/// Whether the restored system's programs run here (`chroot <target> true`): binaries built
/// for a newer CPU level (x86-64-v3) die with SIGILL on an older CPU, so nothing of the boot
/// step can run. `Err` says why.
fn target_runs(log: &mut Log) -> Result<(), String> {
    let t = if in_target("/usr/bin/true").exists() { "/usr/bin/true" } else { "/bin/true" };
    chroot(log, &[t]).map(|_| ())
}

pub(super) fn run(log: &mut Log, argv: &[&str]) -> Result<String, String> {
    run_env(log, argv, &[])
}

/// [`run`] with extra environment variables for that command only.
fn run_env(log: &mut Log, argv: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
    let shown: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    log.line(format!("$ {}{}", if shown.is_empty() { String::new() } else { format!("{} ", shown.join(" ")) }, argv.join(" ")));
    let o = Command::new(argv[0])
        .args(&argv[1..])
        .envs(env.iter().copied())
        .output()
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    if !o.status.success() {
        let msg = failed(argv[0], &o);
        log.line(msg.clone());
        return Err(msg);
    }
    if !err.trim().is_empty() {
        for l in err.lines().take(20) {
            log.line(format!("  {l}"));
        }
    }
    Ok(out)
}

fn run_input(log: &mut Log, argv: &[&str], input: &[u8]) -> Result<(), String> {
    use std::io::Write;
    log.line(format!("$ {}", argv.join(" ")));
    let mut c = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    if let Some(mut s) = c.stdin.take() {
        s.write_all(input).map_err(|e| e.to_string())?;
    }
    let o = c.wait_with_output().map_err(|e| e.to_string())?;
    if !o.status.success() {
        let msg = failed(argv[0], &o);
        log.line(msg.clone());
        return Err(msg);
    }
    Ok(())
}

fn chroot(log: &mut Log, argv: &[&str]) -> Result<String, String> {
    let mut v = vec!["chroot", TARGET];
    v.extend_from_slice(argv);
    // the restored system's LVM reads its own configuration: a live LVM_SYSTEM_DIR names a
    // folder of the live system
    if std::env::var_os("LVM_SYSTEM_DIR").is_some() {
        return run_env(log, &v, &[("LVM_SYSTEM_DIR", "/etc/lvm")]);
    }
    run(log, &v)
}

/// GRUB's configuration made again inside the target (`update-grub`, `grub-mkconfig` or
/// `grub2-mkconfig`), with os-prober off for this run only (`GRUB_DISABLE_OS_PROBER=true` in
/// its environment, and the target's `30_os-prober` not executable meanwhile): it would add
/// the live system to the PC's menu. The target's `/etc/default/grub` is not changed.
fn grub_mkconfig(log: &mut Log) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let argv: &[&str] = if chroot_has("update-grub") {
        &["update-grub"]
    } else if chroot_has("grub-mkconfig") {
        &["grub-mkconfig", "-o", "/boot/grub/grub.cfg"]
    } else {
        &["grub2-mkconfig", "-o", "/boot/grub2/grub.cfg"]
    };
    let mut v = vec!["chroot", TARGET];
    v.extend_from_slice(argv);
    // grub-mkconfig sources /etc/default/grub (and grub.d/*.cfg) after its environment, so
    // a `GRUB_DISABLE_OS_PROBER=false` there wins over the variable: the os-prober script
    // itself is made non-executable for this run, and its mode put back after
    let prober = in_target("/etc/grub.d/30_os-prober");
    let mode = std::fs::symlink_metadata(&prober).ok().filter(|m| m.is_file()).map(|m| m.permissions().mode()).filter(|m| m & 0o111 != 0);
    if let Some(m) = mode {
        match std::fs::set_permissions(&prober, std::fs::Permissions::from_mode(m & !0o111)) {
            Ok(()) => log.line("/etc/grub.d/30_os-prober: not executable while GRUB's menu is made (the live system stays out of it)"),
            Err(e) => log.line(format!("/etc/grub.d/30_os-prober: {e} (os-prober may add the live system to the menu)")),
        }
    }
    let r = run_env(log, &v, &[("GRUB_DISABLE_OS_PROBER", "true")]);
    if let Some(m) = mode
        && let Err(e) = std::fs::set_permissions(&prober, std::fs::Permissions::from_mode(m))
    {
        log.problem(format!("/etc/grub.d/30_os-prober: its mode could not be put back ({e}): chmod {m:o} it"));
    }
    r
}

/// `tool` is on this live system (`PATH` or the usual folders).
pub fn has(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false)
        || ["/usr/sbin", "/sbin", "/usr/bin", "/bin"].iter().any(|d| Path::new(d).join(tool).is_file())
}

/// A path of the restored system, its symlinks followed inside [`TARGET`]
/// ([`resolve_in_root`]): what the restore reads, writes or mounts there never lands on the
/// live system. A symlink loop gives a path that does not exist (the step then fails).
fn in_target(p: &str) -> PathBuf {
    resolve_in_root(Path::new(TARGET), p).unwrap_or_else(|_| Path::new(TARGET).join(".rynas-restore-symlink-loop"))
}

/// The progress line of the data step.
fn print_progress(done: u64, total: u64) {
    if total > 0 && done <= total {
        eprint!("\r  {} / {} MiB ({}%)   ", done >> 20, total >> 20, done * 100 / total);
    } else if done > 0 {
        // the size is not known (btrfs snapshots without quota data): what came so far
        eprint!("\r  {} MiB received   ", done >> 20);
    }
}

fn chroot_has(tool: &str) -> bool {
    ["/usr/sbin", "/sbin", "/usr/bin", "/bin"].iter().any(|d| in_target(&format!("{d}/{tool}")).is_file())
}

/// The disk is safe to wipe: nothing of it mounted, used as swap or held by a mapping, and
/// not the disk of the live system.
pub fn disk_in_use(name: &str) -> Option<String> {
    let sys = PathBuf::from("/sys/block").join(name);
    let mut devs = vec![name.to_string()];
    if let Ok(rd) = std::fs::read_dir(&sys) {
        for e in rd.flatten() {
            if e.path().join("partition").exists() {
                devs.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    let mi = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let swaps = std::fs::read_to_string("/proc/swaps").unwrap_or_default();
    for d in &devs {
        let num = std::fs::read_to_string(PathBuf::from("/sys/class/block").join(d).join("dev")).unwrap_or_default();
        let num = num.trim();
        if !num.is_empty() && mi.lines().any(|l| l.split_whitespace().nth(2) == Some(num)) {
            return Some(format!("/dev/{d} is mounted"));
        }
        if swaps.lines().any(|l| l.starts_with(&format!("/dev/{d} "))) {
            return Some(format!("/dev/{d} is in use as swap"));
        }
        let holders = PathBuf::from("/sys/class/block").join(d).join("holders");
        let held: Vec<String> = std::fs::read_dir(&holders).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        if let Some(h) = held.first() {
            // an LVM group of the old system, often activated by the live system on its own
            let dm = PathBuf::from("/sys/class/block").join(h).join("dm");
            let uuid = std::fs::read_to_string(dm.join("uuid")).unwrap_or_default();
            let name = std::fs::read_to_string(dm.join("name")).unwrap_or_default();
            if uuid.starts_with("LVM-") {
                let (vg, _) = crate::manifest::split_lvm_name(name.trim());
                return Some(format!("/dev/{d} is used by the active LVM volume group {vg}: deactivate it first (vgchange -an {vg})"));
            }
            return Some(format!("/dev/{d} is held by a device mapping (LUKS, LVM, md)"));
        }
    }
    // an inactive LVM group with PVs here and on another disk: wiping this one breaks it there
    if let Some((vg, others)) = super::lvm::split_groups(&super::lvm::live_pvs(), name).into_iter().next() {
        return Some(format!(
            "/dev/{name} holds physical volumes of the LVM volume group {vg}, which also has physical volumes on {}: wiping it would break that group; move its data off this disk first (pvmove, vgreduce), or detach the other disks if the group is not needed",
            others.iter().map(|d| format!("/dev/{d}")).collect::<Vec<_>>().join(", ")
        ));
    }
    // claims that are neither in mountinfo nor in holders (a member of a btrfs mounted from
    // another disk, a ZFS vdev, another restore): Linux refuses an exclusive open of a block
    // device somebody holds
    for d in &devs {
        use std::os::unix::fs::OpenOptionsExt;
        let r = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_EXCL).open(format!("/dev/{d}"));
        if let Err(e) = r
            && e.raw_os_error() == Some(libc::EBUSY)
        {
            return Some(format!("/dev/{d} is in use (a filesystem of several disks, ZFS, or another program)"));
        }
    }
    None
}

fn mount(log: &mut Log, dev: &str, at: &Path, fstype: &str, opts: &str) -> Result<(), String> {
    std::fs::create_dir_all(at).map_err(|e| format!("{}: {e}", at.display()))?;
    let at = at.to_string_lossy().into_owned();
    if opts.is_empty() {
        run(log, &["mount", "-t", fstype, dev, &at]).map(|_| ())
    } else {
        run(log, &["mount", "-t", fstype, "-o", opts, dev, &at]).map(|_| ())
    }
}

fn umount_all(log: &mut Log) {
    let _ = run(log, &["umount", "-R", TARGET]);
    // only what is mounted (the probe and add folders usually are not: no noise in the log)
    let mi = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mounted: Vec<String> = mi.lines().filter_map(|l| l.split_whitespace().nth(4)).map(crate::mounts::unescape).collect();
    if let Ok(rd) = std::fs::read_dir(TOPS) {
        for e in rd.flatten() {
            let p = e.path().to_string_lossy().into_owned();
            if mounted.contains(&p) {
                let _ = run(log, &["umount", &p]);
            }
        }
    }
}

/// Changes the file `rel` of the restored system with `fix` (when that changes anything).
fn rewrite(log: &mut Log, rel: &str, fix: &dyn Fn(&str) -> String) {
    rewrite_in(log, Path::new(TARGET), rel, fix)
}

/// [`rewrite`] for a system whose root is `root` (symlinks followed inside it).
pub fn rewrite_in(log: &mut Log, root: &Path, rel: &str, fix: &dyn Fn(&str) -> String) {
    let Ok(p) = resolve_in_root(root, rel) else {
        log.problem(format!("{rel}: a symlink loop in the restored system"));
        return;
    };
    let Ok(text) = std::fs::read_to_string(&p) else { return };
    let t = fix(&text);
    if t != text {
        match std::fs::write(&p, &t) {
            Ok(()) => log.line(format!("fixed {rel} (IDs, subvolumes)")),
            Err(e) => log.problem(format!("{rel}: could not write the fixes: {e}")),
        }
    }
}

/// After a [`Fill::BtrfsPool`] source: the manifest's subvolumes that are not there yet and
/// not left out on purpose (nor below one left out) are made empty (the excluded ones the
/// restore makes again: snapper's `.snapshots`, btrbk's snapshot folders; NOCOW where the
/// manifest says so), parents first. Container engines' layers and other tools' snapshots
/// are not made again ([`crate::btrfs::recreated_on_restore`]).
pub fn finish_pool(log: &mut Log, top: &Path, subvolumes: &[Subvolume], skipped: &[String]) -> Result<(), String> {
    let mut subs: Vec<&Subvolume> = subvolumes.iter().collect();
    subs.sort_by_key(|s| s.path.matches('/').count());
    for s in subs {
        let rel = safe_rel(&s.path).ok_or_else(|| format!("{}: not a plain subvolume path", s.path))?;
        if rel.is_empty() || skipped.iter().any(|k| !k.trim_matches('/').is_empty() && under(k, &rel)) || !crate::btrfs::recreated_on_restore(&rel) {
            continue;
        }
        let p = in_pool(top, &rel)?;
        if std::fs::symlink_metadata(&p).is_ok() {
            // an empty folder left by the parent's snapshot becomes the subvolume
            if !crate::btrfs::is_empty_plain_dir(&p) {
                continue;
            }
            std::fs::remove_dir(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        crate::btrfs::create(&p)?;
        log.line(format!("{rel}: made empty (not in the backup)"));
        if s.nocow {
            match crate::btrfs::set_nocow(&p) {
                Ok(()) => log.line(format!("{rel}: NOCOW")),
                Err(e) => log.line(format!("warning: {e}")),
            }
        }
    }
    Ok(())
}

/// Copies the files of `from` into `to` (folders made, existing files replaced; symlinks
/// and special files skipped: a FAT ESP has none).
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<(u64, u64)> {
    let (mut files, mut bytes) = (0, 0);
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let t = e.file_type()?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if t.is_dir() {
            let (f, b) = copy_tree(&src, &dst)?;
            files += f;
            bytes += b;
        } else if t.is_file() {
            bytes += std::fs::copy(&src, &dst)?;
            files += 1;
        }
    }
    Ok((files, bytes))
}

/// A boot folder with kernels at its top (`vmlinuz-*`, `initramfs-*`): a copy of `/boot`
/// (Arch with the ESP at `/boot`), not of an ESP mounted at `/boot/efi` or `/efi`.
fn holds_kernels(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|rd| {
        rd.flatten().any(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("vmlinuz") || n.starts_with("initramfs-")
        })
    })
}

/// A swap file of the manifest made again (it is never in a backup): NOCOW, allocated,
/// `mkswap` with its UUID.
fn make_swapfiles(log: &mut Log, m: &Manifest) {
    for s in m.swap.iter().filter(|s| s.file && s.size_bytes > 0 && s.path.starts_with('/')) {
        let p = in_target(&s.path);
        if p.exists() {
            continue;
        }
        let made = (|| -> Result<(), String> {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
            }
            let f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p).map_err(|e| e.to_string())?;
            // NOCOW before any data (btrfs refuses a copy-on-write swap file)
            let _ = crate::btrfs::set_nocow(&p);
            // SAFETY: fallocate(2) on our own descriptor
            if unsafe { libc::fallocate(f.as_raw_fd(), 0, 0, s.size_bytes as libc::off_t) } != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            drop(f);
            let path = p.to_string_lossy().into_owned();
            let mut argv = vec!["mkswap"];
            if !s.uuid.is_empty() {
                argv.extend(["-U", &s.uuid]);
            }
            argv.push(&path);
            run(log, &argv).map(|_| ())
        })();
        match made {
            Ok(()) => log.line(format!("swap file {} made again ({} MiB)", s.path, s.size_bytes >> 20)),
            Err(e) => {
                let _ = std::fs::remove_file(&p);
                log.line(format!("warning: swap file {}: {e}", s.path));
            }
        }
    }
}

/// The kernel command line for `/etc/kernel/cmdline`: the PC's own file, or its
/// `/proc/cmdline` without what the boot loader adds.
pub fn kernel_cmdline(m: &Manifest) -> String {
    if let Some(k) = &m.boot.kernel_cmdline
        && !k.trim().is_empty()
    {
        return k.trim().to_string();
    }
    m.boot
        .cmdline
        .split_whitespace()
        .filter(|t| !t.starts_with("BOOT_IMAGE=") && !t.starts_with("initrd=") && !t.starts_with("systemd.machine_id="))
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct Options<'a> {
    pub passphrase: Option<&'a str>,
    /// An EFI boot entry in the firmware of the machine running the restore.
    pub nvram: Nvram,
}

/// Whether the restore writes an EFI boot entry into the firmware (NVRAM) of the machine
/// running it; the removable path EFI/BOOT boots the disk either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nvram {
    /// Yes, unless a target disk is on USB (it goes into another PC).
    Auto,
    /// Yes (the TUI's choice).
    Yes,
    /// No: the disk goes into another PC (`--no-nvram`, or the TUI's choice).
    No,
}

impl Nvram {
    /// `--no-nvram` on the command line, otherwise [`Nvram::Auto`].
    pub fn from_args(args: &[String]) -> Nvram {
        if args.iter().any(|a| a == "--no-nvram") { Nvram::No } else { Nvram::Auto }
    }
}

/// `/dev/<name>` hangs off a USB bus.
pub fn usb_disk(name: &str) -> bool {
    std::fs::canonicalize(format!("/sys/block/{name}")).is_ok_and(|p| p.to_string_lossy().contains("/usb"))
}

/// The whole restore. On error the target stays unmounted and the log says where it stopped.
pub fn execute(src: &mut dyn RestoreSource, v: &Version, m: &Manifest, plan: &Plan, opts: &Options, log: &mut Log) -> Result<(), String> {
    let r = execute_inner(src, v, m, plan, opts, log);
    if r.is_err() {
        umount_all(log);
        close_all(log, plan, false);
    }
    r
}

/// Closes what the restore opened, from the top down: LUKS inside logical volumes, the
/// volume groups (`done`: with the PC's system ID set, [`super::lvm::finish`]), LUKS on
/// partitions.
fn close_all(log: &mut Log, plan: &Plan, done: bool) {
    for l in plan.luks.iter().filter(|l| l.on_lvm) {
        let _ = run(log, &["cryptsetup", "close", &l.mapper]);
    }
    for vg in &plan.lvm {
        if done {
            super::lvm::finish(log, vg);
        } else {
            super::lvm::deactivate(log, vg);
        }
    }
    for l in plan.luks.iter().filter(|l| !l.on_lvm) {
        let _ = run(log, &["cryptsetup", "close", &l.mapper]);
    }
}

/// The signatures of the partitions the disk has now (filesystems, LVM, LUKS, md), wiped
/// before its new table is written: the new partitions often start where the old ones did,
/// and an old LVM label there would be auto-activated by udev before the restore wipes it.
fn wipe_old_partitions(log: &mut Log, disk: &str) {
    let Ok(rd) = std::fs::read_dir(PathBuf::from("/sys/block").join(disk)) else { return };
    let mut parts: Vec<String> = rd.flatten().filter(|e| e.path().join("partition").exists()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    parts.sort();
    for p in parts {
        let _ = run(log, &["wipefs", "-a", "-q", &format!("/dev/{p}")]);
    }
}

fn execute_inner(src: &mut dyn RestoreSource, v: &Version, m: &Manifest, plan: &Plan, opts: &Options, log: &mut Log) -> Result<(), String> {
    // before anything is written: a LUKS container cannot be made without its passphrase
    if !plan.luks.is_empty() && opts.passphrase.is_none_or(str::is_empty) {
        return Err("a LUKS container needs a passphrase (--passphrase-file); nothing was written".into());
    }
    // 1. disks
    for d in &plan.disks {
        let dev = format!("/dev/{}", d.target.name);
        if let Some(why) = disk_in_use(&d.target.name) {
            return Err(format!("{dev}: {why}"));
        }
        wipe_old_partitions(log, &d.target.name);
        run(log, &["wipefs", "-a", "-q", &dev])?;
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&dev).map_err(|e| format!("{dev}: {e}"))?;
        d.gpt.write(&f, d.target.size_bytes, true).map_err(|e| format!("writing the GPT of {dev}: {e}"))?;
        drop(f);
        log.line(format!("GPT written to {dev}"));
        let _ = run(log, &["blockdev", "--rereadpt", &dev]);
        if has("partx") {
            let _ = run(log, &["partx", "-u", &dev]);
        }
    }
    if has("udevadm") {
        let _ = run(log, &["udevadm", "settle", "--timeout=30"]);
    }
    // the partition nodes
    for d in &plan.disks {
        for (n, _) in d.gpt.used() {
            let p = PathBuf::from("/dev").join(super::plan::part_name(&d.target.name, n as u32));
            let mut ok = false;
            for _ in 0..100 {
                if p.exists() {
                    ok = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if !ok {
                return Err(format!("{} did not appear after writing the partition table", p.display()));
            }
            let _ = run(log, &["wipefs", "-a", "-q", &p.to_string_lossy()]);
        }
    }
    // 2. LUKS on partitions, the LVM groups, LUKS inside logical volumes
    let luks = |log: &mut Log, l: &super::plan::LuksAction| -> Result<(), String> {
        let pass = opts.passphrase.ok_or("a LUKS container needs a passphrase")?;
        let mut argv = vec!["cryptsetup", "luksFormat", "--batch-mode", "--key-file", "-"];
        let ty = format!("luks{}", l.version);
        argv.extend(["--type", &ty, "--uuid", &l.uuid]);
        let bits = l.key_bits.to_string();
        if !l.cipher.is_empty() {
            argv.extend(["--cipher", &l.cipher]);
        }
        if l.key_bits > 0 {
            argv.extend(["--key-size", &bits]);
        }
        argv.push(&l.partition);
        run_input(log, &argv, pass.as_bytes())?;
        run_input(log, &["cryptsetup", "open", "--key-file", "-", &l.partition, &l.mapper], pass.as_bytes())
    };
    for l in plan.luks.iter().filter(|l| !l.on_lvm) {
        luks(log, l)?;
    }
    for vg in &plan.lvm {
        super::lvm::create(log, vg)?;
    }
    for l in plan.luks.iter().filter(|l| l.on_lvm) {
        luks(log, l)?;
    }
    // 3. filesystems and swap
    for a in &plan.filesystems {
        let argv = mkfs_argv(a)?;
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        run(log, &refs)?;
    }
    for s in &plan.swap {
        let mut argv = vec!["mkswap"];
        if !s.uuid.is_empty() {
            argv.extend(["-U", &s.uuid]);
        }
        if !s.label.is_empty() {
            argv.extend(["-L", &s.label]);
        }
        argv.push(&s.device);
        run(log, &argv)?;
    }
    // 4. btrfs, at the top level: the file method makes the subvolumes (parents first) and
    // fills them later; the btrfs method receives them now (and makes the rest empty)
    let mut pools = Vec::new();
    let btrfs_fs: Vec<&super::plan::FsAction> = plan.filesystems.iter().filter(|a| a.fstype == "btrfs").collect();
    for (i, a) in btrfs_fs.iter().enumerate() {
        let f = &m.filesystems[a.fs];
        let top = PathBuf::from(TOPS).join(i.to_string());
        mount(log, &a.device, &top, "btrfs", "subvolid=5")?;
        let mut subs = f.subvolumes.clone();
        subs.sort_by_key(|s| s.path.matches('/').count());
        if src.fill() == Fill::MountedTree {
            for s in &subs {
                let p = in_pool(&top, &s.path)?;
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
                }
                if !p.exists() {
                    run(log, &["btrfs", "subvolume", "create", &p.to_string_lossy()])?;
                }
            }
        }
        pools.push(PoolTarget { uuid: f.uuid.clone(), top_level: top, subvolumes: subs });
    }
    let skipped = src.skipped_subvolumes();
    if src.fill() == Fill::BtrfsPool {
        log.line(format!("receiving the subvolumes of version {}", v.id));
        src.fill_pool(v, &pools, &print_progress, log)?;
        for pool in &pools {
            finish_pool(log, &pool.top_level, &pool.subvolumes, &skipped)?;
        }
    }
    // the default subvolume of every btrfs filesystem
    for (a, pool) in btrfs_fs.iter().zip(&pools) {
        if let Some(d) = &m.filesystems[a.fs].default_subvol {
            run(log, &["btrfs", "subvolume", "set-default", &in_pool(&pool.top_level, d)?.to_string_lossy()])?;
        }
    }
    // 5. the tree as fstab says (shallow mount points first)
    let mut mounts: Vec<(String, &super::plan::FsAction, Option<String>)> = Vec::new();
    let left_out = |s: &Option<String>| s.as_deref().is_some_and(|s| skipped.iter().any(|k| !k.trim_matches('/').is_empty() && under(k, s)));
    for a in &plan.filesystems {
        for me in &m.filesystems[a.fs].mounts {
            if a.fstype == "btrfs" && left_out(&me.subvol) {
                log.line(format!("{}: not restored, not mounted", me.path));
                continue;
            }
            mounts.push((me.path.clone(), a, me.subvol.clone()));
        }
    }
    mounts.sort_by_key(|(p, _, _)| if p == "/" { 0 } else { p.matches('/').count() });
    for (path, a, subvol) in &mounts {
        let at = in_target(path);
        let opts = match (a.fstype.as_str(), subvol.as_deref().map(safe_rel)) {
            ("btrfs", Some(Some(s))) if !s.is_empty() => format!("subvol=/{s}"),
            ("btrfs", Some(None)) => return Err(format!("{path}: the subvolume {} is not a plain path", subvol.as_deref().unwrap_or(""))),
            ("btrfs", _) => "subvolid=5".to_string(),
            // the ESP with the PC's masks (bootctl refuses a world-readable random seed)
            ("vfat", _) => super::fstab::fat_options(&m.fstab, path),
            _ => String::new(),
        };
        mount(log, &a.device, &at, &a.fstype, &opts)?;
    }
    if !mounts.iter().any(|(p, _, _)| p == "/") {
        return Err("the manifest has no root filesystem among the restored ones".into());
    }
    // 6. the data
    if src.fill() == Fill::MountedTree {
        log.line(format!("restoring the files of version {} ({} files, {} MiB)", v.id, v.files, v.bytes >> 20));
        src.fill_tree(v, Path::new(TARGET), &print_progress)?;
        eprintln!();
        log.line("files restored");
    }
    // the boot filesystems a pool restore leaves empty (the ESP): the PC's own copy inside
    // the restored root first, so the kernels are there; the boot step then regenerates what
    // it can, and the copy stands where it cannot
    let mut boot_files = src.fill() == Fill::MountedTree;
    for (dir, at) in src.boot_copies(m) {
        let (from, to) = (in_target(&dir), in_target(&at));
        if !from.is_dir() {
            log.line(format!("warning: the boot copy {dir} is not in the restored system"));
            continue;
        }
        // a copy that holds kernels is a copy of /boot: on an ESP mounted elsewhere it would
        // put the boot folder's layout where the boot loader does not look for it
        if at.trim_end_matches('/') != "/boot" && holds_kernels(&from) {
            log.line(format!("warning: {dir} holds kernels (a copy of /boot), not the files of {at}: not laid on {at}; the boot step makes them"));
            continue;
        }
        match copy_tree(&from, &to) {
            Ok((files, bytes)) => {
                log.line(format!("{dir} → {at}: {files} files, {} MiB", bytes >> 20));
                boot_files |= files > 0;
            }
            Err(e) => log.line(format!("warning: copying {dir} to {at}: {e}")),
        }
    }
    make_swapfiles(log, m);
    for d in ["proc", "sys", "dev", "run", "tmp"] {
        let _ = std::fs::create_dir_all(in_target(d));
    }
    let _ = std::fs::set_permissions(in_target("tmp"), std::os::unix::fs::PermissionsExt::from_mode(0o1777));
    add_crypttab_keys(log, plan, opts.passphrase);
    // 7. ID fixes, the lines of subvolumes not restored, then the checks
    fix_texts(log, m, plan, &skipped);
    if std::fs::read_to_string(in_target("/etc/selinux/config")).is_ok_and(|t| t.lines().any(|l| l.trim() == "SELINUX=enforcing")) {
        let _ = std::fs::write(in_target("/.autorelabel"), b"");
        log.line("SELinux enforcing: /.autorelabel set");
    }
    lvm_conf_filters(log, m, plan);
    lvm_initramfs(log, m, plan);
    // 8. boot
    boot(m, plan, log, boot_files, opts.nvram)?;
    // 9. log into the target, unmount
    let ts = crate::state::now_ms() / 1000;
    let _ = std::fs::create_dir_all(in_target("/var/log"));
    let _ = std::fs::write(in_target(&format!("/var/log/rynas-restore-{ts}.log")), log.text());
    let _ = run(log, &["sync"]);
    umount_all(log);
    close_all(log, plan, true);
    log.line("restore finished");
    Ok(())
}

/// A restored container that `/etc/crypttab` opens with a key file (unattended boot, the key
/// often embedded in the initramfs) gets that file, found in the restored system, as a second
/// key: the new container has only the passphrase typed for the restore, and the initramfs
/// tries the key file without asking for a passphrase (seen on Debian in the B4 VMs).
fn add_crypttab_keys(log: &mut Log, plan: &Plan, passphrase: Option<&str>) {
    let (Some(pass), Ok(tab)) = (passphrase, std::fs::read_to_string(in_target("/etc/crypttab"))) else {
        return;
    };
    for line in tab.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 || f[0].starts_with('#') || !f[2].starts_with('/') {
            continue;
        }
        let Some(l) = plan.luks.iter().find(|l| l.mapper == f[0]) else { continue };
        let key = in_target(f[2]);
        // a regular file inside the restored system, never a link out of it
        if !std::fs::symlink_metadata(&key).is_ok_and(|md| md.is_file()) {
            log.problem(format!("{}: crypttab opens it with {}, which is not in the restored system: add a key for it after the first boot", f[0], f[2]));
            continue;
        }
        let part = match keyfile_args(f.get(3).copied().unwrap_or("")) {
            Ok(a) => a,
            Err(e) => {
                log.problem(format!("{}: crypttab {e}: add a key for it after the first boot", f[0]));
                continue;
            }
        };
        let k = key.to_string_lossy().into_owned();
        let mut argv = vec!["cryptsetup", "luksAddKey", "--batch-mode", "--key-file", "-"];
        argv.extend(part.iter().map(String::as_str));
        argv.extend([l.partition.as_str(), k.as_str()]);
        match run_input(log, &argv, pass.as_bytes()) {
            Ok(()) => log.line(format!("{}: the key file {} added as a second key (crypttab)", f[0], f[2])),
            Err(e) => log.problem(format!("{}: adding the key file {}: {e}", f[0], f[2])),
        }
    }
}

/// The part of a key file crypttab uses (its options `keyfile-offset=` and `keyfile-size=`)
/// as `cryptsetup luksAddKey` options for the new key, so the key added is the bytes the
/// initramfs will read. A value that is not a number is an error (no key added).
pub fn keyfile_args(opts: &str) -> Result<Vec<String>, String> {
    let mut v = Vec::new();
    for o in opts.split(',') {
        let Some((k, val)) = o.trim().split_once('=') else { continue };
        let flag = match k {
            "keyfile-offset" => "--new-keyfile-offset",
            "keyfile-size" => "--new-keyfile-size",
            _ => continue,
        };
        if val.is_empty() || !val.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("option {k}={val} is not a number"));
        }
        v.extend([flag.to_string(), val.to_string()]);
    }
    Ok(v)
}

/// The text fixes after the data: fstab lines of subvolumes not restored commented (with
/// [`super::plan::RESTORE_MARK`]), the old IDs replaced when new ones were made (fstab,
/// crypttab, `/etc/kernel/cmdline`, `/etc/default/grub`, the loader entries), then the
/// checks: the restored root's ID on the active fstab line of `/`, no old ID left.
fn fix_texts(log: &mut Log, m: &Manifest, plan: &Plan, skipped: &[String]) {
    let map = &plan.id_map;
    rewrite(log, "/etc/fstab", &|t| super::plan::fix_fstab(t, m, skipped, map));
    if !skipped.is_empty() {
        log.line(format!("/etc/fstab: lines of {} commented ({})", skipped.join(", "), super::plan::RESTORE_MARK.trim()));
    }
    rewrite(log, "/etc/crypttab", &|t| super::plan::rewrite_ids(t, map));
    let esp = m.boot.esp.clone().unwrap_or_else(|| "/boot/efi".into());
    let entries_dir = format!("{}/loader/entries", esp.trim_end_matches('/'));
    let mut cmdlines: Vec<String> = vec!["/etc/kernel/cmdline".into(), "/etc/default/grub".into()];
    if let Ok(rd) = std::fs::read_dir(in_target(&entries_dir)) {
        cmdlines.extend(rd.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.ends_with(".conf")).map(|n| format!("{entries_dir}/{n}")));
    }
    for f in &cmdlines {
        rewrite(log, f, &|t| super::plan::fix_cmdline(t, m, map));
    }
    // the checks on what is there now
    let root = plan.filesystems.iter().find(|a| m.filesystems[a.fs].mounts.iter().any(|x| x.path == "/"));
    let mut root_ids: Vec<String> = root.map(|a| vec![a.uuid.clone()]).unwrap_or_default();
    if let Some(a) = root
        && let FsSource::Partition { disk, number } = &m.filesystems[a.fs].source
        && let Some(d) = plan.disks.iter().find(|d| d.source == *disk)
        && let Some(e) = d.gpt.entries.get(*number as usize - 1)
    {
        root_ids.push(crate::gpt::guid_text(&e.guid));
    }
    let fstab = std::fs::read_to_string(in_target("/etc/fstab")).unwrap_or_default();
    let checked: Vec<(String, String)> = cmdlines.iter().filter_map(|f| std::fs::read_to_string(in_target(f)).ok().map(|t| (f.clone(), t))).collect();
    for p in super::plan::sanity(&fstab, &root_ids, &checked, map) {
        log.problem(p);
    }
}

/// A restored system on LVM needs LVM in its initramfs: the lvm2 package in the system
/// (dracut and initramfs-tools take their LVM module or hook from it) and, for mkinitcpio, the
/// `lvm2` hook in `HOOKS`, added when missing in the file that sets `HOOKS` last
/// (`/etc/mkinitcpio.conf`, or a drop-in of `/etc/mkinitcpio.conf.d`). Text only: it holds
/// also when the restored system's programs cannot run here.
fn lvm_initramfs(log: &mut Log, m: &Manifest, plan: &Plan) {
    if plan.lvm.is_empty() {
        return;
    }
    let names = plan.lvm.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ");
    if !chroot_has("lvm") {
        log.problem(format!("the restored system has no lvm2 (no lvm program): it cannot activate {names} at boot; install lvm2 in it (chroot) and rebuild the initramfs before rebooting"));
    }
    if detect_boot(m).0 != InitramfsTool::Mkinitcpio {
        return;
    }
    let mut files = vec!["/etc/mkinitcpio.conf".to_string()];
    if let Ok(rd) = std::fs::read_dir(in_target("/etc/mkinitcpio.conf.d")) {
        let mut d: Vec<String> = rd.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.ends_with(".conf")).map(|n| format!("/etc/mkinitcpio.conf.d/{n}")).collect();
        d.sort();
        files.extend(d);
    }
    let texts: Vec<String> = files.iter().map(|f| std::fs::read_to_string(in_target(f)).unwrap_or_default()).collect();
    if super::lvm::hooks_have_lvm(&texts) {
        log.line("mkinitcpio: HOOKS has the lvm2 hook");
        return;
    }
    let Some(i) = super::lvm::hooks_file(&texts) else {
        log.problem("mkinitcpio: no HOOKS=(…) found in the restored system: add the lvm2 hook by hand and rebuild the initramfs".to_string());
        return;
    };
    let f = &files[i];
    match super::lvm::add_lvm_hook(&texts[i]) {
        Some(t) => match std::fs::write(in_target(f), t) {
            Ok(()) => log.line(format!("{f}: the lvm2 hook added to HOOKS (the volumes are on LVM)")),
            Err(e) => log.problem(format!("{f}: could not add the lvm2 hook: {e}")),
        },
        None => log.line(format!("{f}: HOOKS has the lvm2 hook")),
    }
}

/// The restored system's LVM device filters (`filter`/`global_filter` in `/etc/lvm/lvm.conf`
/// and `lvmlocal.conf`) with the old disk names changed to the new ones, before the
/// initramfs (which carries lvm.conf) is made; a filter by disk IDs cannot be rewritten and
/// is reported.
fn lvm_conf_filters(log: &mut Log, m: &Manifest, plan: &Plan) {
    if plan.lvm.is_empty() {
        return;
    }
    let renames = super::plan::disk_renames(m, &plan.disks);
    for f in ["/etc/lvm/lvm.conf", "/etc/lvm/lvmlocal.conf"] {
        let Ok(text) = std::fs::read_to_string(in_target(f)) else { continue };
        for s in super::lvm::filter_settings(&text).iter().filter(|s| super::lvm::filter_by_identity(s)) {
            log.problem(format!("{f}: the device filter `{s}` names disks by their IDs, which the new disk does not have: change it, or LVM may not find the volume groups at boot"));
        }
        let Some(new) = super::lvm::rewrite_filters(&text, &renames) else { continue };
        match std::fs::write(in_target(f), &new) {
            Ok(()) => {
                for (a, b) in super::lvm::filter_settings(&text).iter().zip(super::lvm::filter_settings(&new)) {
                    if *a != b {
                        log.line(format!("{f}: device filter rewritten for the new disk: `{a}` → `{b}`"));
                    }
                }
            }
            Err(e) => log.problem(format!("{f}: the device filter names the old disk and could not be rewritten ({e}): change it, or LVM may not find the volume groups at boot")),
        }
    }
}

/// LVM's devices file (`/etc/lvm/devices/system.devices`, the default on newer Fedora and
/// RHEL) lists the PC's disks by their IDs, so LVM would ignore the new disk. The PC's file is
/// kept as `system.devices.rynas-old`; the new one keeps the lines of the PC's other disks
/// (a group on a second disk stays visible at boot) without the lines of the restored PVs,
/// and `vgimportdevices` in the chroot adds the restored groups with the new disk's IDs. If
/// that fails for a group, there is no devices file at all (LVM then looks at every disk)
/// rather than one that hides a group.
fn lvm_devices_file(log: &mut Log, plan: &Plan) {
    let f = in_target("/etc/lvm/devices/system.devices");
    if plan.lvm.is_empty() || !f.is_file() {
        return;
    }
    // a system whose LVM does not use the file (use_devicesfile=0) gets nothing changed
    if let Ok(o) = chroot(log, &["lvm", "lvmconfig", "--typeconfig", "full", "devices/use_devicesfile"])
        && o.split_whitespace().any(|w| w == "use_devicesfile=0")
    {
        log.line("/etc/lvm/devices/system.devices: the restored system's LVM does not use it (use_devicesfile=0), left as it is");
        return;
    }
    let old = f.with_file_name("system.devices.rynas-old");
    let text = match std::fs::read_to_string(&f) {
        Ok(t) => t,
        Err(e) => {
            log.problem(format!("/etc/lvm/devices/system.devices: {e}: LVM may ignore the new disk; run vgimportdevices --all after the first boot"));
            return;
        }
    };
    if let Err(e) = std::fs::copy(&f, &old) {
        log.problem(format!("/etc/lvm/devices/system.devices: keeping the PC's copy: {e}; the file is left as it was: LVM may ignore the new disk, run vgimportdevices --all after the first boot"));
        return;
    }
    let pvids: Vec<String> = plan.lvm.iter().flat_map(|vg| vg.pvs.iter().flat_map(|p| [p.orig_uuid.clone(), p.uuid.clone().unwrap_or_default()])).collect();
    let (kept, dropped) = super::lvm::devices_file_without(&text, &pvids);
    let no_file = |log: &mut Log, why: String| match std::fs::remove_file(&f).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }) {
        Ok(()) => log.line(format!("warning: {why}; /etc/lvm/devices/system.devices removed (LVM looks at every disk; the PC's is system.devices.rynas-old): run vgimportdevices --all after the first boot to have one again")),
        Err(e) => log.problem(format!("{why}; /etc/lvm/devices/system.devices could not be removed ({e}): LVM may ignore the restored groups at boot")),
    };
    if let Err(e) = std::fs::write(&f, kept) {
        no_file(log, format!("/etc/lvm/devices/system.devices: {e}"));
        return;
    }
    log.line(format!("/etc/lvm/devices/system.devices: the PC's kept as system.devices.rynas-old; {dropped} lines of the restored PVs taken out, the other disks' kept"));
    for vg in &plan.lvm {
        // through `lvm`: not every distribution links each LVM command as a program
        if let Err(e) = chroot(log, &["lvm", "vgimportdevices", &vg.name]) {
            no_file(log, format!("vgimportdevices {}: {e}", vg.name));
            return;
        }
    }
}

/// What a look at the btrfs filesystem on a device found.
#[derive(Debug, Clone, Default)]
pub struct PoolProbe {
    pub uuid: Option<String>,
    /// Which of the asked paths (from the top level) exist.
    pub present: Vec<String>,
}

/// The btrfs filesystem on `device`: mounted read-only at its top level without replaying
/// its log (a look, also for a dry run, writes nothing), then unmounted.
pub fn probe_pool(device: &str, paths: &[String]) -> Result<PoolProbe, String> {
    let at = PathBuf::from(TOPS).join("probe");
    let mut log = Log::new(Box::new(|_| {}));
    // kernels before 5.9 know only the older name of the option
    mount(&mut log, device, &at, "btrfs", "ro,rescue=nologreplay,subvolid=5").or_else(|_| mount(&mut log, device, &at, "btrfs", "ro,nologreplay,subvolid=5"))?;
    let present = paths.iter().filter(|p| in_pool(&at, p).is_ok_and(|x| x.exists())).cloned().collect();
    let uuid = crate::btrfs::fs_uuid(&at);
    let _ = run(&mut log, &["umount", &at.to_string_lossy()]);
    Ok(PoolProbe { uuid, present })
}

/// "Add subvolumes" goes only into the PC's own filesystem (the manifest's root UUID)
/// unless `other_ok` says otherwise.
pub fn check_same_pool(m: &Manifest, device: &str, uuid: Option<&str>, other_ok: bool) -> Result<(), String> {
    let want = m.root().map(|f| f.uuid.as_str()).filter(|u| !u.is_empty());
    match (want, uuid) {
        (Some(w), Some(got)) if !w.eq_ignore_ascii_case(got) && !other_ok => Err(format!(
            "{device} holds the btrfs filesystem {got}, not the PC's ({w}): choose the PC's own filesystem, or allow another one (--other-filesystem)"
        )),
        _ => Ok(()),
    }
}

/// "Add subvolumes" (btrfs method): nothing is formatted. The PC's filesystem on `device`
/// (checked by UUID, [`check_same_pool`]) is mounted at its top level, the source receives
/// the chosen subvolumes into it, and the fstab lines an earlier restore commented for them
/// are active again.
pub fn execute_add(src: &mut dyn RestoreSource, v: &Version, m: &Manifest, device: &str, restored: &[String], other_ok: bool, log: &mut Log) -> Result<(), String> {
    let name = device.trim_start_matches("/dev/");
    if let Some(why) = disk_in_use(name) {
        return Err(format!("{device}: {why}"));
    }
    let top = PathBuf::from(TOPS).join("add");
    mount(log, device, &top, "btrfs", "subvolid=5")?;
    let r = (|| -> Result<(), String> {
        check_same_pool(m, device, crate::btrfs::fs_uuid(&top).as_deref(), other_ok)?;
        let root = m.root().ok_or("the manifest has no root filesystem")?;
        let pools = vec![PoolTarget { uuid: root.uuid.clone(), top_level: top.clone(), subvolumes: root.subvolumes.clone() }];
        log.line(format!("adding to the filesystem on {device} from version {}", v.id));
        src.fill_pool(v, &pools, &print_progress, log)?;
        let root_path = m.btrbk.as_ref().and_then(|b| b.subvolumes.iter().find(|s| s.mount == "/")).map(|s| s.path.clone());
        if let Some(rp) = root_path {
            let f = resolve_in_root(&in_pool(&top, &rp)?, "/etc/fstab")?;
            if let Ok(t) = std::fs::read_to_string(&f) {
                let u = super::plan::uncomment_restored(&t, restored);
                if u != t {
                    std::fs::write(&f, &u).map_err(|e| format!("{}: {e}", f.display()))?;
                    log.line(format!("/etc/fstab: the lines of {} are active again", restored.join(", ")));
                }
            }
        }
        Ok(())
    })();
    let _ = run(log, &["sync"]);
    let _ = run(log, &["umount", &top.to_string_lossy()]);
    if r.is_ok() {
        log.line("subvolumes added");
    }
    r
}

fn bind_system(log: &mut Log) -> Result<(), String> {
    for d in ["dev", "proc", "sys", "run"] {
        let at = in_target(d);
        let _ = std::fs::create_dir_all(&at);
        run(log, &["mount", "--rbind", &format!("/{d}"), &at.to_string_lossy()])?;
        let _ = run(log, &["mount", "--make-rslave", &at.to_string_lossy()]);
    }
    Ok(())
}

/// Copies `from` (inside the ESP) to the removable path when that is missing.
fn removable_fallback(log: &mut Log, esp: &Path, files: &[(&str, &str)]) {
    let boot = esp.join("EFI/BOOT");
    let _ = std::fs::create_dir_all(&boot);
    for (from, to) in files {
        let (f, t) = (esp.join(from), boot.join(to));
        if f.is_file() && !t.exists() && std::fs::copy(&f, &t).is_ok() {
            log.line(format!("removable fallback: {} → EFI/BOOT/{to}", from));
        }
    }
}

/// The initramfs tool and the boot loader of the restored system when the manifest does not
/// say (a legacy snapshot): what is installed in it, and what the ESP holds.
fn detect_boot(m: &Manifest) -> (InitramfsTool, Loader) {
    let tool = match m.boot.initramfs {
        InitramfsTool::Unknown if chroot_has("mkinitcpio") => InitramfsTool::Mkinitcpio,
        InitramfsTool::Unknown if chroot_has("dracut") => InitramfsTool::Dracut,
        InitramfsTool::Unknown if chroot_has("update-initramfs") => InitramfsTool::UpdateInitramfs,
        t => t,
    };
    let esp = in_target(m.boot.esp.as_deref().unwrap_or("/boot/efi"));
    let loader = match m.boot.loader {
        Loader::Unknown if esp.join("loader").is_dir() || esp.join("EFI/systemd").is_dir() => Loader::SystemdBoot,
        Loader::Unknown if chroot_has("grub-install") || chroot_has("grub2-install") => Loader::Grub,
        l => l,
    };
    (tool, loader)
}

fn boot(m: &Manifest, plan: &Plan, log: &mut Log, boot_files: bool, nvram: Nvram) -> Result<(), String> {
    bind_system(log)?;
    // the data is written: a system whose programs do not run on this CPU keeps it, and
    // only the boot step is skipped (reported, so no reboot is offered)
    if let Err(e) = target_runs(log) {
        log.problem(format!(
            "the restored system's programs do not run on this CPU (built for a newer CPU level?): boot the restored disk on the PC itself; the boot step (initramfs, boot loader, boot entries) was skipped ({e})"
        ));
        return Ok(());
    }
    // EFI boot entries go into the firmware of the machine running the restore, which is
    // the restored PC itself except when the disk is on USB or the user said otherwise
    // (`--no-nvram`, the TUI's choice); the removable path EFI/BOOT boots it either way
    let usb = nvram == Nvram::Auto && plan.disks.iter().any(|d| usb_disk(&d.target.name));
    let no_nvram = nvram == Nvram::No;
    let uefi_live = Path::new("/sys/firmware/efi").exists() && !no_nvram && !usb;
    if m.boot.uefi && !uefi_live {
        let why = if no_nvram { "not chosen" } else if usb { "the disk is on USB" } else { "the live system did not boot in UEFI mode" };
        log.line(format!("no EFI boot entry written ({why}): the disk boots through the removable path EFI/BOOT"));
    }
    let (initramfs_tool, loader) = detect_boot(m);
    if (initramfs_tool, loader) != (m.boot.initramfs, m.boot.loader) {
        log.line(format!("found in the restored system: initramfs {initramfs_tool:?}, boot loader {loader:?}"));
    }
    let mut m = m.clone();
    m.boot.initramfs = initramfs_tool;
    m.boot.loader = loader;
    let m = &m;
    // the PC's command line, before any tool that writes boot entries
    // (the same fixes as the dry run shows: IDs, the root's subvolume ID)
    let cmdline_mapped = super::plan::fix_cmdline(&kernel_cmdline(m), m, &plan.id_map);
    if !cmdline_mapped.trim().is_empty() && (m.boot.kernel_cmdline.is_some() || matches!(m.boot.loader, Loader::SystemdBoot) || m.boot.uki) {
        let _ = std::fs::create_dir_all(in_target("/etc/kernel"));
        std::fs::write(in_target("/etc/kernel/cmdline"), format!("{cmdline_mapped}\n")).map_err(|e| format!("/etc/kernel/cmdline: {e}"))?;
        log.line(format!("/etc/kernel/cmdline: {cmdline_mapped}"));
    }
    lvm_devices_file(log, plan);
    // the initramfs (also covers new hardware; a UKI is made again by mkinitcpio -P)
    let initramfs = match m.boot.initramfs {
        InitramfsTool::UpdateInitramfs => chroot(log, &["update-initramfs", "-u", "-k", "all"]),
        InitramfsTool::Mkinitcpio => chroot(log, &["mkinitcpio", "-P"]),
        InitramfsTool::Dracut => chroot(log, &["dracut", "--regenerate-all", "--force"]),
        InitramfsTool::Unknown => Ok(String::new()),
    };
    if let Err(e) = initramfs {
        log.line(format!("warning: initramfs not regenerated ({e}); the restored one stays"));
    }
    let esp_rel = m.boot.esp.clone().unwrap_or_else(|| "/boot/efi".into());
    let esp = in_target(&esp_rel);
    let no_vars: Vec<&str> = if uefi_live { vec![] } else { vec!["--no-nvram"] };
    match (m.boot.loader, m.boot.uefi) {
        (Loader::Grub, true) => {
            let tool = if chroot_has("grub-install") { "grub-install" } else { "grub2-install" };
            let id = m.boot.efi_id.clone().unwrap_or_else(|| "rynas".into());
            let arch_target = if std::env::consts::ARCH == "aarch64" { "arm64-efi" } else { "x86_64-efi" };
            let target_arg = format!("--target={arch_target}");
            let esp_arg = format!("--efi-directory={esp_rel}");
            let id_arg = format!("--bootloader-id={id}");
            let mut argv = vec![tool, target_arg.as_str(), esp_arg.as_str(), id_arg.as_str()];
            argv.extend(no_vars.iter());
            if let Err(e) = chroot(log, &argv) {
                // Fedora ships GRUB and shim prebuilt in the ESP (restored as files)
                if esp.join(format!("EFI/{id}")).is_dir() {
                    log.line(format!("grub-install failed ({e}); the restored EFI files stay"));
                } else {
                    return Err(e);
                }
            }
            grub_mkconfig(log)?;
            let dir = format!("EFI/{id}");
            if m.boot.shim {
                removable_fallback(log, &esp, &[(&format!("{dir}/shimx64.efi"), "BOOTX64.EFI"), (&format!("{dir}/grubx64.efi"), "grubx64.efi"), (&format!("{dir}/mmx64.efi"), "mmx64.efi")]);
            } else {
                removable_fallback(log, &esp, &[(&format!("{dir}/grubx64.efi"), "BOOTX64.EFI"), (&format!("{dir}/grubaa64.efi"), "BOOTAA64.EFI")]);
            }
        }
        (Loader::Grub, false) => {
            let tool = if chroot_has("grub-install") { "grub-install" } else { "grub2-install" };
            for d in &plan.disks {
                chroot(log, &[tool, "--target=i386-pc", &format!("/dev/{}", d.target.name)])?;
            }
            grub_mkconfig(log)?;
        }
        (Loader::SystemdBoot, _) => {
            let esp_arg = format!("--esp-path={esp_rel}");
            let mut argv = vec!["bootctl", "install", esp_arg.as_str()];
            if !uefi_live {
                argv.push("--no-variables");
            }
            if let Err(e) = chroot(log, &argv) {
                if esp.join("EFI/systemd").is_dir() {
                    log.line(format!("bootctl install failed ({e}); the restored EFI files stay"));
                } else {
                    return Err(e);
                }
            }
            // entries made by kernel-install (machine-id layout) are made again with the
            // PC's command line; hand-written entries (restored with the ESP) stay
            let mid = std::fs::read_to_string(in_target("/etc/machine-id")).unwrap_or_default().trim().to_string();
            let ki = !mid.is_empty()
                && (esp.join(&mid).is_dir()
                    || std::fs::read_dir(esp.join("loader/entries"))
                        .map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().starts_with(&mid)))
                        .unwrap_or(false))
                && chroot_has("kernel-install");
            if ki {
                for k in &m.boot.kernels {
                    let img = ["/usr/lib/modules/{k}/vmlinuz", "/lib/modules/{k}/vmlinuz", "/boot/vmlinuz-{k}"]
                        .iter()
                        .map(|p| p.replace("{k}", k))
                        .find(|p| in_target(p).is_file());
                    if let Some(img) = img
                        && let Err(e) = chroot(log, &["kernel-install", "add", k, &img])
                    {
                        log.line(format!("warning: kernel-install {k}: {e}"));
                    }
                }
            }
            removable_fallback(log, &esp, &[("EFI/systemd/systemd-bootx64.efi", "BOOTX64.EFI"), ("EFI/systemd/systemd-bootaa64.efi", "BOOTAA64.EFI")]);
        }
        (Loader::Refind, _) | (Loader::Unknown, _) => {
            log.line("warning: the boot loader is unknown or rEFInd: reinstall it by hand if the PC does not boot (the ESP files were restored)");
        }
    }
    // the boot files must be there: an empty ESP does not boot
    if m.boot.uefi {
        let efi = esp.join("EFI");
        let has_efi = std::fs::read_dir(&efi).map(|mut r| r.next().is_some()).unwrap_or(false);
        if !has_efi {
            log.problem(format!("{esp_rel} has no EFI boot files: reinstall the boot loader from the restored system before rebooting"));
        } else if !boot_files && matches!(m.boot.loader, Loader::Unknown | Loader::Refind) {
            log.problem("the boot loader is unknown and no boot files were restored: check the ESP before rebooting".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmdlines() {
        let mut m = super::super::plan::tests::sample();
        m.boot.cmdline = "BOOT_IMAGE=/vmlinuz-6.12 root=PARTUUID=x ro quiet initrd=\\initrd.img".into();
        assert_eq!(kernel_cmdline(&m), "root=PARTUUID=x ro quiet");
        m.boot.kernel_cmdline = Some("root=UUID=y rw\n".into());
        assert_eq!(kernel_cmdline(&m), "root=UUID=y rw");
    }

    #[test]
    fn a_silent_failure_says_how_it_ended() {
        let o = Command::new("sh").args(["-c", "echo one; echo two; kill -ILL $$"]).output().unwrap();
        let m = failed("chroot", &o);
        assert!(m.contains("killed by signal 4 (SIGILL"), "{m}");
        assert!(m.ends_with("last output: one | two"), "{m}");
        let o = Command::new("sh").args(["-c", "exit 3"]).output().unwrap();
        assert_eq!(failed("x", &o), "x failed: exit status 3");
        let o = Command::new("sh").args(["-c", "echo bad >&2; exit 1"]).output().unwrap();
        assert_eq!(failed("x", &o), "x failed: bad");
    }

    #[test]
    fn crypttab_key_offsets_go_to_the_new_key() {
        assert!(keyfile_args("luks,discard").unwrap().is_empty());
        assert!(keyfile_args("").unwrap().is_empty());
        assert_eq!(keyfile_args("luks,keyfile-offset=512,keyfile-size=4096").unwrap(), ["--new-keyfile-offset", "512", "--new-keyfile-size", "4096"]);
        assert!(keyfile_args("keyfile-size=4k").is_err());
        assert!(keyfile_args("keyfile-offset=").is_err());
    }
}
