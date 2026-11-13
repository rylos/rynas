//! Carries out a [`Plan`]: shared by every restore source.
//!
//! GPT → LUKS → mkfs with the original IDs → btrfs subvolumes and the default one → the data
//! (the source fills the mounted tree, or a btrfs pool before the tree is mounted) → ID fixes
//! → chroot: `/etc/kernel/cmdline` from the manifest **before** any boot tool (`kernel-install`
//! in a chroot would read the live system's `/proc/cmdline`), the initramfs, the boot loader
//! and the removable EFI fallback → unmount → report.

use std::path::{Path, PathBuf};
use std::process::Command;

use rynas_api_types::pcagent::*;

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

fn run(log: &mut Log, argv: &[&str]) -> Result<String, String> {
    log.line(format!("$ {}", argv.join(" ")));
    let o = Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    if !o.status.success() {
        let msg = format!("{} failed: {}", argv[0], err.trim());
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
        let msg = format!("{} failed: {}", argv[0], String::from_utf8_lossy(&o.stderr).trim());
        log.line(msg.clone());
        return Err(msg);
    }
    Ok(())
}

/// Runs `argv` inside the target (chroot).
fn chroot(log: &mut Log, argv: &[&str]) -> Result<String, String> {
    let mut v = vec!["chroot", TARGET];
    v.extend_from_slice(argv);
    run(log, &v)
}

fn has(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false)
        || ["/usr/sbin", "/sbin", "/usr/bin", "/bin"].iter().any(|d| Path::new(d).join(tool).is_file())
}

fn in_target(p: &str) -> PathBuf {
    Path::new(TARGET).join(p.trim_start_matches('/'))
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
        if std::fs::read_dir(&holders).map(|mut r| r.next().is_some()).unwrap_or(false) {
            return Some(format!("/dev/{d} is held by a device mapping (LUKS, LVM, md)"));
        }
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
    if let Ok(rd) = std::fs::read_dir(TOPS) {
        for e in rd.flatten() {
            let _ = run(log, &["umount", &e.path().to_string_lossy()]);
        }
    }
}

fn rewrite(log: &mut Log, p: &Path, map: &[(String, String)]) {
    let Ok(text) = std::fs::read_to_string(p) else { return };
    let t = super::plan::rewrite_ids(&text, map);
    if t != text {
        if std::fs::write(p, &t).is_ok() {
            log.line(format!("rewrote IDs in {}", p.display()));
        } else {
            log.line(format!("could not rewrite {}", p.display()));
        }
    }
}

/// After a [`Fill::BtrfsPool`] source: the manifest's subvolumes that are not there yet and
/// not left out on purpose are made empty (excluded ones such as logs, caches, snapper's
/// `.snapshots`; NOCOW where the manifest says so), parents first. Container engines'
/// layers and other tools' snapshots are not made again.
pub fn finish_pool(log: &mut Log, top: &Path, subvolumes: &[Subvolume], skipped: &[String]) -> Result<(), String> {
    let mut subs: Vec<&Subvolume> = subvolumes.iter().collect();
    subs.sort_by_key(|s| s.path.matches('/').count());
    for s in subs {
        let rel = s.path.trim_start_matches('/');
        if rel.is_empty() || skipped.iter().any(|k| k == rel || rel.starts_with(&format!("{k}/"))) {
            continue;
        }
        if ["docker/btrfs", "containers/storage", "timeshift-btrfs", crate::btrfs::SNAP_DIR].iter().any(|x| rel.contains(x)) {
            continue;
        }
        let p = top.join(rel);
        if std::fs::symlink_metadata(&p).is_ok() {
            // an empty folder left by the parent's snapshot becomes the subvolume
            let empty = p.is_dir() && crate::btrfs::subvol_info(&p).is_none() && std::fs::read_dir(&p).map(|mut r| r.next().is_none()).unwrap_or(false);
            if !empty {
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
}

/// The whole restore. On error the target stays unmounted and the log says where it stopped.
pub fn execute(src: &mut dyn RestoreSource, v: &Version, m: &Manifest, plan: &Plan, opts: &Options, log: &mut Log) -> Result<(), String> {
    let r = execute_inner(src, v, m, plan, opts, log);
    if r.is_err() {
        umount_all(log);
        for l in &plan.luks {
            let _ = run(log, &["cryptsetup", "close", &l.mapper]);
        }
    }
    r
}

fn execute_inner(src: &mut dyn RestoreSource, v: &Version, m: &Manifest, plan: &Plan, opts: &Options, log: &mut Log) -> Result<(), String> {
    // 1. disks
    for d in &plan.disks {
        let dev = format!("/dev/{}", d.target.name);
        if let Some(why) = disk_in_use(&d.target.name) {
            return Err(format!("{dev}: {why}"));
        }
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
    // 2. LUKS
    for l in &plan.luks {
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
        run_input(log, &["cryptsetup", "open", "--key-file", "-", &l.partition, &l.mapper], pass.as_bytes())?;
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
    // 4. btrfs: subvolumes (parents first) and the default, at the top level
    let mut pools = Vec::new();
    for (i, a) in plan.filesystems.iter().enumerate().filter(|(_, a)| a.fstype == "btrfs") {
        let f = &m.filesystems[a.fs];
        let top = PathBuf::from(TOPS).join(i.to_string());
        mount(log, &a.device, &top, "btrfs", "subvolid=5")?;
        let mut subs = f.subvolumes.clone();
        subs.sort_by_key(|s| s.path.matches('/').count());
        if src.fill() == Fill::MountedTree {
            for s in &subs {
                let p = top.join(&s.path);
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
                }
                if !p.exists() {
                    run(log, &["btrfs", "subvolume", "create", &p.to_string_lossy()])?;
                }
            }
            if let Some(d) = &f.default_subvol {
                run(log, &["btrfs", "subvolume", "set-default", &top.join(d).to_string_lossy()])?;
            }
        }
        pools.push(PoolTarget {
            uuid: a.uuid.clone(),
            top_level: top,
            subvolumes: subs,
        });
    }
    let progress = |done: u64, total: u64| {
        if total > 0 {
            eprint!("\r  {} / {} MiB ({}%)   ", done >> 20, total >> 20, done * 100 / total.max(1));
        }
    };
    let skipped = src.skipped_subvolumes();
    if src.fill() == Fill::BtrfsPool {
        log.line(format!("receiving the subvolumes of version {}", v.id));
        let r = src.fill_pool(v, &pools, &progress);
        // already printed by the source
        log.lines.extend(src.drain_log());
        r?;
        for pool in &pools {
            finish_pool(log, &pool.top_level, &pool.subvolumes, &skipped)?;
        }
        if let Some((f, a)) = plan.filesystems.iter().filter(|a| a.fstype == "btrfs").map(|a| (&m.filesystems[a.fs], a)).next()
            && let Some(d) = &f.default_subvol
            && let Some(pool) = pools.iter().find(|p| p.uuid == a.uuid)
        {
            run(log, &["btrfs", "subvolume", "set-default", &pool.top_level.join(d).to_string_lossy()])?;
        }
    }
    // 5. the tree as fstab says (shallow mount points first)
    let mut mounts: Vec<(String, &super::plan::FsAction, Option<String>)> = Vec::new();
    let left_out = |s: &Option<String>| {
        s.as_deref().map(|s| s.trim_start_matches('/')).is_some_and(|s| skipped.iter().any(|k| k == s || s.starts_with(&format!("{k}/"))))
    };
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
        let opts = match (a.fstype.as_str(), subvol) {
            ("btrfs", Some(s)) if !s.is_empty() => format!("subvol=/{s}"),
            ("btrfs", _) => "subvolid=5".to_string(),
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
        src.fill_tree(v, Path::new(TARGET), &progress)?;
        eprintln!();
        log.line("files restored");
    }
    // the boot filesystems a pool restore leaves empty (the ESP): the PC's own copy inside
    // the restored root first, so the kernels are there; the boot step then regenerates what
    // it can, and the copy stands where it cannot
    let mut boot_files = src.fill() == Fill::MountedTree;
    if src.fill() == Fill::BtrfsPool {
        for (dir, at) in src.boot_copies(m) {
            let (from, to) = (in_target(&dir), in_target(&at));
            if !from.is_dir() {
                log.line(format!("warning: the boot copy {dir} is not in the restored system"));
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
    }
    make_swapfiles(log, m);
    for d in ["proc", "sys", "dev", "run", "tmp"] {
        let _ = std::fs::create_dir_all(in_target(d));
    }
    let _ = std::fs::set_permissions(in_target("tmp"), std::os::unix::fs::PermissionsExt::from_mode(0o1777));
    // 7. ID fixes, the lines of subvolumes not restored, then the checks
    fix_texts(log, m, plan, &skipped);
    if std::fs::read_to_string(in_target("/etc/selinux/config")).is_ok_and(|t| t.lines().any(|l| l.trim() == "SELINUX=enforcing")) {
        let _ = std::fs::write(in_target("/.autorelabel"), b"");
        log.line("SELinux enforcing: /.autorelabel set");
    }
    // 8. boot
    boot(m, plan, log, boot_files)?;
    // 9. log into the target, unmount
    let ts = crate::state::now_ms() / 1000;
    let _ = std::fs::create_dir_all(in_target("/var/log"));
    let _ = std::fs::write(in_target(&format!("/var/log/rynas-restore-{ts}.log")), log.text());
    let _ = run(log, &["sync"]);
    umount_all(log);
    for l in &plan.luks {
        let _ = run(log, &["cryptsetup", "close", &l.mapper]);
    }
    log.line("restore finished");
    Ok(())
}

/// The text fixes after the data: fstab lines of subvolumes not restored commented (with
/// [`super::plan::RESTORE_MARK`]), the old IDs replaced when new ones were made (fstab,
/// crypttab, `/etc/kernel/cmdline`, `/etc/default/grub`, the loader entries), then the
/// checks: the restored root's ID on the active fstab line of `/`, no old ID left.
fn fix_texts(log: &mut Log, m: &Manifest, plan: &Plan, skipped: &[String]) {
    let fstab_p = in_target("/etc/fstab");
    if !skipped.is_empty()
        && let Ok(t) = std::fs::read_to_string(&fstab_p)
    {
        let c = super::plan::comment_unrestored(&t, skipped);
        if c != t {
            match std::fs::write(&fstab_p, &c) {
                Ok(()) => log.line(format!("/etc/fstab: lines of {} commented ({})", skipped.join(", "), super::plan::RESTORE_MARK.trim())),
                Err(e) => log.problem(format!("/etc/fstab: {e}")),
            }
        }
    }
    let mut entries: Vec<PathBuf> = Vec::new();
    if let Some(esp) = &m.boot.esp
        && let Ok(rd) = std::fs::read_dir(in_target(esp).join("loader/entries"))
    {
        entries.extend(rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "conf")));
    }
    if plan.id_map.is_empty() {
        return;
    }
    let texts = ["/etc/fstab", "/etc/crypttab", "/etc/kernel/cmdline", "/etc/default/grub"];
    for f in texts {
        rewrite(log, &in_target(f), &plan.id_map);
    }
    for e in &entries {
        rewrite(log, e, &plan.id_map);
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
    let fstab = std::fs::read_to_string(&fstab_p).unwrap_or_default();
    let mut checked: Vec<(String, String)> = Vec::new();
    for f in ["/etc/kernel/cmdline", "/etc/default/grub"] {
        if let Ok(t) = std::fs::read_to_string(in_target(f)) {
            checked.push((f.to_string(), t));
        }
    }
    for e in &entries {
        if let Ok(t) = std::fs::read_to_string(e) {
            checked.push((e.display().to_string(), t));
        }
    }
    for p in super::plan::sanity(&fstab, &root_ids, &checked, &plan.id_map) {
        log.problem(p);
    }
}

/// Which of `paths` (from the top level) exist in the btrfs filesystem on `device`: mounted
/// read-only at its top level for the look, then unmounted.
pub fn existing_paths(device: &str, paths: &[String]) -> Result<Vec<String>, String> {
    let at = PathBuf::from(TOPS).join("probe");
    let mut log = Log::new(Box::new(|_| {}));
    mount(&mut log, device, &at, "btrfs", "ro,subvolid=5")?;
    let found = paths.iter().filter(|p| at.join(p.trim_start_matches('/')).exists()).cloned().collect();
    let _ = run(&mut log, &["umount", &at.to_string_lossy()]);
    Ok(found)
}

/// "Add subvolumes" (btrfs method): nothing is formatted. The PC's pool on `device` is
/// mounted at its top level, the source receives the chosen subvolumes into it, and the
/// fstab lines an earlier restore commented for them are active again.
pub fn execute_add(src: &mut dyn RestoreSource, v: &Version, m: &Manifest, device: &str, restored: &[String], log: &mut Log) -> Result<(), String> {
    let name = device.trim_start_matches("/dev/");
    if let Some(why) = disk_in_use(name) {
        return Err(format!("{device}: {why}"));
    }
    let top = PathBuf::from(TOPS).join("add");
    mount(log, device, &top, "btrfs", "subvolid=5")?;
    let r = (|| -> Result<(), String> {
        let root = m.root().ok_or("the manifest has no root filesystem")?;
        let pools = vec![PoolTarget { uuid: root.uuid.clone(), top_level: top.clone(), subvolumes: root.subvolumes.clone() }];
        let progress = |done: u64, total: u64| {
            if total > 0 {
                eprint!("\r  {} / {} MiB ({}%)   ", done >> 20, total >> 20, done * 100 / total.max(1));
            }
        };
        log.line(format!("adding to the pool on {device} from version {}", v.id));
        let r = src.fill_pool(v, &pools, &progress);
        log.lines.extend(src.drain_log());
        r?;
        let root_path = m.btrbk.as_ref().and_then(|b| b.subvolumes.iter().find(|s| s.mount == "/")).map(|s| s.path.trim_start_matches('/').to_string());
        if let Some(rp) = root_path {
            let f = top.join(rp).join("etc/fstab");
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

fn boot(m: &Manifest, plan: &Plan, log: &mut Log, boot_files: bool) -> Result<(), String> {
    bind_system(log)?;
    let uefi_live = Path::new("/sys/firmware/efi").exists();
    let (initramfs_tool, loader) = detect_boot(m);
    if (initramfs_tool, loader) != (m.boot.initramfs, m.boot.loader) {
        log.line(format!("found in the restored system: initramfs {initramfs_tool:?}, boot loader {loader:?}"));
    }
    let mut m = m.clone();
    m.boot.initramfs = initramfs_tool;
    m.boot.loader = loader;
    let m = &m;
    // the PC's command line, before any tool that writes boot entries
    let cmdline = kernel_cmdline(m);
    let mut cmdline_mapped = cmdline.clone();
    for (o, n) in &plan.id_map {
        cmdline_mapped = cmdline_mapped.replace(o.as_str(), n);
    }
    if !cmdline_mapped.trim().is_empty() && (m.boot.kernel_cmdline.is_some() || matches!(m.boot.loader, Loader::SystemdBoot) || m.boot.uki) {
        let _ = std::fs::create_dir_all(in_target("/etc/kernel"));
        std::fs::write(in_target("/etc/kernel/cmdline"), format!("{cmdline_mapped}\n")).map_err(|e| format!("/etc/kernel/cmdline: {e}"))?;
        log.line(format!("/etc/kernel/cmdline: {cmdline_mapped}"));
    }
    // the initramfs (also covers new hardware)
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
            let mk = if chroot_has("update-grub") {
                chroot(log, &["update-grub"])
            } else if chroot_has("grub-mkconfig") {
                chroot(log, &["grub-mkconfig", "-o", "/boot/grub/grub.cfg"])
            } else {
                chroot(log, &["grub2-mkconfig", "-o", "/boot/grub2/grub.cfg"])
            };
            mk?;
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
            if chroot_has("update-grub") {
                chroot(log, &["update-grub"])?;
            } else if chroot_has("grub-mkconfig") {
                chroot(log, &["grub-mkconfig", "-o", "/boot/grub/grub.cfg"])?;
            } else {
                chroot(log, &["grub2-mkconfig", "-o", "/boot/grub2/grub.cfg"])?;
            }
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
}
