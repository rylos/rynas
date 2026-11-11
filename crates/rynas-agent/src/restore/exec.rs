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

pub const TARGET: &str = "/run/rynas-agent/target";
const TOPS: &str = "/run/rynas-agent/top";

/// The restore log: printed, and kept for the NAS and the target's `/var/log`.
pub struct Log {
    pub lines: Vec<String>,
    pub echo: Box<dyn Fn(&str)>,
}

impl Log {
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

/// Replaces every old ID with the new one in a text file of the target.
fn rewrite(log: &mut Log, p: &Path, map: &[(String, String)]) {
    let Ok(text) = std::fs::read_to_string(p) else { return };
    let mut t = text.clone();
    for (old, new) in map {
        t = t.replace(old.as_str(), new).replace(&old.to_uppercase(), &new.to_uppercase());
    }
    if t != text {
        if std::fs::write(p, &t).is_ok() {
            log.line(format!("rewrote IDs in {}", p.display()));
        } else {
            log.line(format!("could not rewrite {}", p.display()));
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
    if src.fill() == Fill::BtrfsPool {
        log.line(format!("receiving the subvolumes of version {}", v.id));
        src.fill_pool(v, &pools, &progress)?;
        if let Some((f, a)) = plan.filesystems.iter().filter(|a| a.fstype == "btrfs").map(|a| (&m.filesystems[a.fs], a)).next()
            && let Some(d) = &f.default_subvol
            && let Some(pool) = pools.iter().find(|p| p.uuid == a.uuid)
        {
            run(log, &["btrfs", "subvolume", "set-default", &pool.top_level.join(d).to_string_lossy()])?;
        }
    }
    // 5. the tree as fstab says (shallow mount points first)
    let mut mounts: Vec<(String, &super::plan::FsAction, Option<String>)> = Vec::new();
    for a in &plan.filesystems {
        for me in &m.filesystems[a.fs].mounts {
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
    for d in ["proc", "sys", "dev", "run", "tmp"] {
        let _ = std::fs::create_dir_all(in_target(d));
    }
    let _ = std::fs::set_permissions(in_target("tmp"), std::os::unix::fs::PermissionsExt::from_mode(0o1777));
    // 7. ID fixes
    if !plan.id_map.is_empty() {
        for f in ["/etc/fstab", "/etc/crypttab", "/etc/kernel/cmdline", "/etc/default/grub"] {
            rewrite(log, &in_target(f), &plan.id_map);
        }
        if let Some(esp) = &m.boot.esp
            && let Ok(rd) = std::fs::read_dir(in_target(esp).join("loader/entries"))
        {
            for e in rd.flatten() {
                rewrite(log, &e.path(), &plan.id_map);
            }
        }
        let root_new = plan.filesystems.iter().find(|a| m.filesystems[a.fs].mounts.iter().any(|x| x.path == "/")).map(|a| a.uuid.clone()).unwrap_or_default();
        let fstab = std::fs::read_to_string(in_target("/etc/fstab")).unwrap_or_default();
        let active = fstab.lines().filter(|l| !l.trim_start().starts_with('#')).any(|l| l.contains(&root_new));
        if !root_new.is_empty() && !active && fstab.contains("UUID=") {
            log.line("warning: the root filesystem's new UUID is on no active fstab line: check /etc/fstab");
        }
    }
    if std::fs::read_to_string(in_target("/etc/selinux/config")).is_ok_and(|t| t.lines().any(|l| l.trim() == "SELINUX=enforcing")) {
        let _ = std::fs::write(in_target("/.autorelabel"), b"");
        log.line("SELinux enforcing: /.autorelabel set");
    }
    // 8. boot
    boot(m, plan, log)?;
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

fn boot(m: &Manifest, plan: &Plan, log: &mut Log) -> Result<(), String> {
    bind_system(log)?;
    let uefi_live = Path::new("/sys/firmware/efi").exists();
    // the PC's command line, before any tool that writes boot entries
    let cmdline = kernel_cmdline(m);
    let mut cmdline_mapped = cmdline.clone();
    for (o, n) in &plan.id_map {
        cmdline_mapped = cmdline_mapped.replace(o.as_str(), n);
    }
    if m.boot.kernel_cmdline.is_some() || matches!(m.boot.loader, Loader::SystemdBoot) || m.boot.uki {
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
