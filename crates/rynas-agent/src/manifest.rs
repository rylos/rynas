//! The layout manifest (`/.rynas/manifest.json`, `rynas_api_types::pcagent::Manifest`):
//! disks by model, serial and size with their GPT, filesystems with UUIDs, labels, mounts and
//! btrfs subvolumes, LUKS parameters (never a header or a key slot), md/LVM reports, swap,
//! fstab, crypttab, the kernel command line and the boot loader. Written into the root
//! filesystem (temporary file + rename) before every run, so every version carries the
//! layout of its own moment; also by `rynas-agent manifest`, which PCs that push with btrbk
//! run before btrbk (the snippet the NAS shows for such a folder; [`add_btrbk`]); the agent's
//! own btrfs sender adds its block with [`add_block`] (boot copies included). [`lock`] keeps
//! runs one at a time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rynas_api_types::pcagent::*;

use crate::gpt::{Gpt, guid_text};
use crate::mounts::{self, Mount};
use crate::probe::{self, Block};

fn read(p: &str) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn os_release() -> BTreeMap<String, String> {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_else(|_| read("/usr/lib/os-release"));
    text.lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| ["ID", "ID_LIKE", "VERSION_ID", "PRETTY_NAME"].contains(k))
        .map(|(k, v)| (k.to_string(), v.trim_matches('"').to_string()))
        .collect()
}

pub fn hostname() -> String {
    read("/proc/sys/kernel/hostname").trim().to_string()
}

/// Where the ESP is mounted, if anywhere.
pub fn esp_mount(all: &[Mount]) -> Option<String> {
    ["/boot/efi", "/efi", "/boot"]
        .iter()
        .find(|p| all.iter().any(|m| m.path == **p && m.fstype == "vfat"))
        .map(|p| p.to_string())
}

/// The boot loader and friends, from the files of `root` (`/` or a restored tree).
pub fn boot_info(root: &Path, esp: Option<&str>, cmdline: &str) -> BootInfo {
    let at = |p: &str| root.join(p.trim_start_matches('/'));
    let esp_dir = esp.map(at);
    let efi = esp_dir.as_ref().map(|e| e.join("EFI"));
    let exists = |p: &Path| p.exists();
    let sdboot = efi.as_ref().is_some_and(|e| exists(&e.join("systemd/systemd-bootx64.efi")) || exists(&e.join("systemd/systemd-bootaa64.efi")));
    let grub = exists(&at("/boot/grub/grub.cfg")) || exists(&at("/boot/grub2/grub.cfg"))
        || efi.as_ref().is_some_and(|e| std::fs::read_dir(e).map(|rd| rd.flatten().any(|d| d.path().join("grub.cfg").exists() || d.path().join("grubx64.efi").exists())).unwrap_or(false));
    let refind = efi.as_ref().is_some_and(|e| exists(&e.join("refind")));
    let loader = if sdboot && esp_dir.as_ref().is_some_and(|e| e.join("loader/entries").exists() || e.join("loader/loader.conf").exists()) {
        Loader::SystemdBoot
    } else if grub {
        Loader::Grub
    } else if sdboot {
        Loader::SystemdBoot
    } else if refind {
        Loader::Refind
    } else {
        Loader::Unknown
    };
    let mut efi_id = None;
    let mut shim = false;
    if let Some(e) = &efi
        && let Ok(rd) = std::fs::read_dir(e)
    {
        for d in rd.flatten() {
            let n = d.file_name().to_string_lossy().into_owned();
            if ["BOOT", "Boot", "boot", "Linux", "systemd", "refind"].contains(&n.as_str()) {
                continue;
            }
            let p = d.path();
            if p.join("shimx64.efi").exists() || p.join("shimaa64.efi").exists() {
                shim = true;
            }
            if p.join("grubx64.efi").exists() || p.join("grub.cfg").exists() || p.join("shimx64.efi").exists() || p.join("grubaa64.efi").exists() {
                efi_id = Some(n);
            }
        }
    }
    let uki = efi.as_ref().is_some_and(|e| {
        std::fs::read_dir(e.join("Linux")).map(|rd| rd.flatten().any(|f| f.path().extension().is_some_and(|x| x == "efi"))).unwrap_or(false)
    });
    let initramfs = if exists(&at("/etc/mkinitcpio.conf")) && (exists(&at("/usr/bin/mkinitcpio")) || exists(&at("/bin/mkinitcpio"))) {
        InitramfsTool::Mkinitcpio
    } else if exists(&at("/usr/sbin/update-initramfs")) {
        InitramfsTool::UpdateInitramfs
    } else if exists(&at("/usr/bin/dracut")) || exists(&at("/usr/sbin/dracut")) {
        InitramfsTool::Dracut
    } else {
        InitramfsTool::Unknown
    };
    let grub_cmdline = std::fs::read_to_string(at("/etc/default/grub")).ok().and_then(|t| {
        t.lines()
            .find_map(|l| l.trim().strip_prefix("GRUB_CMDLINE_LINUX="))
            .map(|v| v.trim_matches('"').trim_matches('\'').to_string())
    });
    let mut kernels: Vec<String> = std::fs::read_dir(at("/usr/lib/modules"))
        .or_else(|_| std::fs::read_dir(at("/lib/modules")))
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    kernels.sort();
    BootInfo {
        uefi: Path::new("/sys/firmware/efi").exists(),
        loader,
        shim,
        uki,
        esp: esp.map(str::to_string),
        efi_id,
        initramfs,
        cmdline: cmdline.trim().to_string(),
        kernel_cmdline: std::fs::read_to_string(at("/etc/kernel/cmdline")).ok().map(|s| s.trim().to_string()),
        grub_cmdline,
        kernels,
    }
}

/// Where a device sits, in the manifest's terms.
fn source_of(b: &Block, disks: &[Block], luks: &[Luks]) -> FsSource {
    if let (Some(n), Some(parent)) = (b.partition, &b.parent)
        && let Some(i) = disks.iter().position(|d| &d.name == parent)
    {
        return FsSource::Partition { disk: i, number: n };
    }
    if let Some(u) = &b.dm_uuid {
        if u.starts_with("CRYPT-LUKS") {
            let mapper = b.dm_name.clone().unwrap_or_default();
            if let Some(i) = luks.iter().position(|l| l.mapper == mapper) {
                return FsSource::Luks { luks: i };
            }
        }
        if u.starts_with("LVM-") {
            let name = b.dm_name.clone().unwrap_or_default();
            // vg-lv with doubled dashes inside names
            let (vg, lv) = split_lvm_name(&name);
            return FsSource::Lvm { vg, lv };
        }
    }
    if b.name.starts_with("md") {
        return FsSource::Md { name: b.name.clone() };
    }
    if let Some(i) = disks.iter().position(|d| d.name == b.name) {
        return FsSource::Disk { disk: i };
    }
    FsSource::Other
}

/// `vg-lv` as device-mapper names it (`-` inside a name is doubled).
pub fn split_lvm_name(n: &str) -> (String, String) {
    let b = n.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'-' {
            if b.get(i + 1) == Some(&b'-') {
                i += 2;
                continue;
            }
            return (n[..i].replace("--", "-"), n[i + 1..].replace("--", "-"));
        }
        i += 1;
    }
    (n.to_string(), String::new())
}

/// btrfs subvolumes below a mounted btrfs filesystem's top level, by walking for subvolume
/// roots (inode 256) from each mount: the mounted ones and the nested ones found inside them.
fn subvolumes(fs_mounts: &[&Mount]) -> Vec<Subvolume> {
    use std::os::unix::fs::MetadataExt;
    let mut out: Vec<Subvolume> = Vec::new();
    for m in fs_mounts {
        let base = mounts::subvol(m);
        if !out.iter().any(|s| s.path == base) && !base.is_empty() {
            out.push(Subvolume { path: base.clone(), captured: true, ..Default::default() });
        }
        for (rel, _) in crate::btrfs::nested(Path::new(&m.path), 6) {
            let path = if base.is_empty() { rel.clone() } else { format!("{base}/{rel}") };
            if !out.iter().any(|s| s.path == path) {
                let excluded = crate::btrfs::excluded_subvolume(&rel);
                out.push(Subvolume { path, captured: !excluded, ..Default::default() });
            }
        }
        let _ = std::fs::metadata(&m.path).map(|md| md.ino());
    }
    for s in &mut out {
        s.nocow = false;
    }
    out
}

/// Collects the manifest of this machine. `captures` says how each mount point is read in
/// the run being prepared (empty for `rynas-agent manifest`).
pub fn collect(captures: &BTreeMap<String, CaptureMethod>) -> Manifest {
    let all = mounts::read();
    let backed = mounts::backup_mounts(&all);
    let disk_blocks = probe::disks();
    let mut disks = Vec::new();
    for d in &disk_blocks {
        let (model, serial, wwn) = probe::identity(d);
        let table = std::fs::File::open(probe::dev_path(&d.name)).ok().and_then(|f| {
            let g = Gpt::read(&f, u64::from(d.logical_sector)).ok()??;
            let parts = probe::partitions(d);
            Some(PartTable {
                kind: "gpt".into(),
                disk_guid: guid_text(&g.disk_guid),
                first_lba: 0,
                last_lba: g.last_usable(d.size_bytes),
                partitions: g
                    .used()
                    .map(|(n, e)| Partition {
                        number: n as u32,
                        start_lba: e.first_lba,
                        size_lba: e.last_lba - e.first_lba + 1,
                        type_guid: guid_text(&e.type_guid),
                        part_guid: guid_text(&e.guid),
                        name: e.name(),
                        attrs: e.attributes,
                        dev: parts.iter().find(|p| p.partition == Some(n as u32)).map(|p| p.name.clone()).unwrap_or_default(),
                    })
                    .collect(),
            })
        });
        disks.push(Disk {
            name: d.name.clone(),
            model,
            serial,
            wwn,
            size_bytes: d.size_bytes,
            logical_sector: d.logical_sector,
            table,
        });
    }
    // LUKS containers: every open crypt mapping
    let mut luks = Vec::new();
    for e in std::fs::read_dir("/sys/block").map(|r| r.flatten().collect::<Vec<_>>()).unwrap_or_default() {
        let Some(b) = std::fs::canonicalize(e.path()).ok().and_then(|p| probe::block_at(&p)) else { continue };
        if !b.dm_uuid.as_deref().is_some_and(|u| u.starts_with("CRYPT-LUKS")) {
            continue;
        }
        let Some(slave) = b.slaves.first() else { continue };
        let Some(under) = probe::block_of_path(&probe::dev_path(slave)) else { continue };
        let Some(p) = probe::probe(&probe::dev_path(slave)) else { continue };
        let Some((version, cipher, bits, pbkdf)) = p.luks.clone() else { continue };
        luks.push(Luks {
            device: format!("/dev/{slave}"),
            source: source_of(&under, &disk_blocks, &[]),
            uuid: p.uuid,
            label: p.label,
            version,
            cipher,
            key_bits: bits,
            pbkdf,
            mapper: b.dm_name.clone().unwrap_or_default(),
        });
    }
    // filesystems: one per device, with every mount of it
    let mut filesystems: Vec<Filesystem> = Vec::new();
    for m in &backed {
        let Some(b) = probe::block_of_devnum(&m.dev).or_else(|| probe::block_of_path(Path::new(&m.source))) else { continue };
        let dev = m.source.clone();
        if let Some(f) = filesystems.iter_mut().find(|f| f.device == dev) {
            f.mounts.push(MountEntry {
                path: m.path.clone(),
                subvol: (m.fstype == "btrfs").then(|| mounts::subvol(m)),
                options: m.options.clone(),
            });
            continue;
        }
        let p = probe::probe(Path::new(&dev)).unwrap_or_default();
        let (size, used) = statvfs(&m.path);
        filesystems.push(Filesystem {
            device: dev,
            source: source_of(&b, &disk_blocks, &luks),
            fstype: m.fstype.clone(),
            uuid: p.uuid,
            label: p.label,
            mounts: vec![MountEntry {
                path: m.path.clone(),
                subvol: (m.fstype == "btrfs").then(|| mounts::subvol(m)),
                options: m.options.clone(),
            }],
            size_bytes: size,
            used_bytes: used,
            capture: captures.get(&m.path).copied(),
            features: p.features,
            subvolumes: Vec::new(),
            default_subvol: None,
        });
    }
    for f in &mut filesystems {
        if f.fstype == "btrfs" {
            let ms: Vec<&Mount> = backed.iter().filter(|m| m.source == f.device).collect();
            f.subvolumes = subvolumes(&ms);
            f.default_subvol = ms.first().and_then(|m| crate::btrfs::default_subvol_path(Path::new(&m.path)));
        }
        // a mount point read in more than one way keeps the first
        if f.capture.is_none() {
            f.capture = f.mounts.iter().find_map(|m| captures.get(&m.path).copied());
        }
    }
    // swap
    let mut swap = Vec::new();
    for l in read("/proc/swaps").lines().skip(1) {
        let mut it = l.split_whitespace();
        let (Some(path), Some(kind), Some(size)) = (it.next(), it.next(), it.next()) else { continue };
        let path = mounts::unescape(path);
        let file = kind == "file";
        let p = if file { None } else { probe::probe(Path::new(&path)) };
        let src = if file { FsSource::Other } else { probe::block_of_path(Path::new(&path)).map(|b| source_of(&b, &disk_blocks, &luks)).unwrap_or_default() };
        swap.push(Swap {
            path,
            file,
            uuid: p.as_ref().map(|p| p.uuid.clone()).unwrap_or_default(),
            label: p.map(|p| p.label).unwrap_or_default(),
            size_bytes: size.parse::<u64>().unwrap_or(0) * 1024,
            source: src,
        });
    }
    let lvm_report = (filesystems.iter().any(|f| matches!(f.source, FsSource::Lvm { .. })))
        .then(|| {
            std::process::Command::new("lvs")
                .args(["--reportformat", "json", "-o", "vg_name,lv_name,lv_size,lv_attr,pool_lv,devices", "--units", "b"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        })
        .flatten();
    let md: Vec<String> = std::fs::read_dir("/sys/block")
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("md")).collect::<Vec<_>>())
        .unwrap_or_default()
        .iter()
        .filter_map(|n| {
            std::process::Command::new("mdadm")
                .args(["--detail", "--export", &format!("/dev/{n}")])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        })
        .collect();
    let cmdline = read("/proc/cmdline");
    let esp = esp_mount(&all);
    Manifest {
        format: MANIFEST_FORMAT,
        written_at_ms: crate::state::now_ms(),
        agent_version: env!("CARGO_PKG_VERSION").into(),
        hostname: hostname(),
        machine_id: read("/etc/machine-id").trim().to_string(),
        arch: std::env::consts::ARCH.into(),
        os_release: os_release(),
        boot: boot_info(Path::new("/"), esp.as_deref(), &cmdline),
        disks,
        filesystems,
        luks,
        swap,
        lvm_report,
        md,
        fstab: read("/etc/fstab"),
        crypttab: std::fs::read_to_string("/etc/crypttab").ok(),
        // the btrfs method fills it (`rynas-agent manifest` on a btrfs PC, a later step)
        btrbk: None,
    }
}

/// (size, used) of the filesystem at `path`.
pub fn statvfs(path: &str) -> (u64, u64) {
    let Ok(c) = std::ffi::CString::new(path) else { return (0, 0) };
    // SAFETY: statvfs fills the struct it is given
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) != 0 {
            return (0, 0);
        }
        let f = s.f_frsize;
        (s.f_blocks * f, (s.f_blocks - s.f_bfree) * f)
    }
}

/// Writes the manifest into `/.rynas/manifest.json` of the root filesystem (temporary file,
/// fsync, rename, fsync of the folder).
pub fn write(m: &Manifest, root: &Path) -> Result<PathBuf, String> {
    let dir = root.join(".rynas");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let p = dir.join("manifest.json");
    let readme = dir.join("README");
    if !readme.exists() {
        let _ = std::fs::write(
            &readme,
            "Written by rynas-agent before every backup: the layout of this PC (disks, partitions,\nfilesystems, boot loader) for a bare-metal restore. Safe to read; it is rewritten each run.\n",
        );
    }
    crate::state::write_atomic(&p, &serde_json::to_vec_pretty(m).map_err(|e| e.to_string())?, 0o600)?;
    if let Ok(d) = std::fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(p)
}

/// The legacy btrbk PC's part of the manifest: the btrbk block read from `btrbk.conf`
/// (none when it sends nothing), then [`add_block`].
pub fn add_btrbk(m: &mut Manifest, all: &[Mount], conf: &str, root: &Path, copy: bool) -> Vec<String> {
    let Some(b) = crate::btrbk::manifest_block(&crate::btrbk::parse_conf(conf), all) else {
        return Vec::new();
    };
    add_block(m, all, b, root, copy)
}

/// `/etc/crypttab` lines that open a device with a key file: (name, key file). `none`, `-`,
/// an empty field, a password prompt (`/dev/…` key devices, `/dev/random`/`urandom` for swap
/// and tmp) are not key files.
pub fn crypttab_keyfiles(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let key = *f.get(2)?;
            let none = key == "none" || key == "-" || key.starts_with("/dev/") || key.starts_with("/proc/");
            (!none).then(|| (f[0].to_string(), key.to_string()))
        })
        .collect()
}

/// The btrfs method's block (btrbk's or the agent's): the captured marks and, with `copy`,
/// the boot filesystems outside the sent subvolumes copied into `<root>/.rynas/boot/`. A
/// failed copy keeps the previous record (marked `stale`, with the error) and its folder;
/// only a copy whose mount is gone is removed. Returns the problems met (a failed copy
/// never stops the manifest).
pub fn add_block(m: &mut Manifest, all: &[Mount], mut b: BtrbkLayout, root: &Path, copy: bool) -> Vec<String> {
    use crate::{bootcopy, btrbk};
    btrbk::mark_captured(m, &b, all);
    let mut problems = crypttab_keyfiles(m.crypttab.as_deref().unwrap_or(""))
        .into_iter()
        .map(|(name, key)| {
            format!(
                "crypttab: {name} opens with the key file {key}: a bare-metal restore needs that key (keep a copy apart from the PC); an initramfs that embeds it carries it into every root snapshot and boot copy"
            )
        })
        .collect::<Vec<_>>();
    if copy {
        let previous: Vec<BootCopy> = std::fs::read(root.join(".rynas/manifest.json"))
            .ok()
            .and_then(|t| serde_json::from_slice::<Manifest>(&t).ok())
            .and_then(|old| old.btrbk)
            .map(|o| o.boot_copies)
            .unwrap_or_default();
        let boot_dir = root.join(".rynas/boot");
        for mount in btrbk::boot_mounts(all, &b.subvolumes) {
            let src = root.join(mount.trim_start_matches('/'));
            let prev = previous.iter().find(|p| p.mount == mount);
            match bootcopy::copy_boot(&src, &mount, &boot_dir, "/.rynas/boot", prev) {
                Ok(c) => b.boot_copies.push(c),
                Err(e) => {
                    problems.push(format!("copy of {mount}: {e}"));
                    if let Some(p) = prev {
                        b.boot_copies.push(BootCopy { stale: true, error: Some(e), ..p.clone() });
                    }
                }
            }
        }
        bootcopy::prune_copies(&boot_dir, &b.boot_copies);
    }
    m.btrbk = Some(b);
    problems
}

/// `<root>/.rynas/.lock` (folder made `0700`), taken exclusively: one manifest, boot copy or
/// backup at a time. Waits up to `wait` for another run to end. The lock goes with the file.
pub fn lock(root: &Path, wait: std::time::Duration) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = root.join(".rynas");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let p = dir.join(".lock");
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&p)
        .map_err(|e| format!("{}: {e}", p.display()))?;
    let end = std::time::Instant::now() + wait;
    loop {
        // SAFETY: flock(2) on our own descriptor
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(f);
        }
        if std::time::Instant::now() >= end {
            return Err(format!("another rynas-agent run holds {} (a backup or a manifest): try again later", p.display()));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lvm_names() {
        assert_eq!(split_lvm_name("vg0-root"), ("vg0".into(), "root".into()));
        assert_eq!(split_lvm_name("my--vg-lv--home"), ("my-vg".into(), "lv-home".into()));
    }

    #[test]
    fn crypttab_key_files_are_noticed() {
        let t = "# comment\nroot UUID=1 none luks\nhome UUID=2 /etc/keys/home.key luks\nswap /dev/sda3 /dev/urandom swap\ndata UUID=3 - luks\nusb UUID=4 /dev/disk/by-label/KEY luks # key on a stick\nbad\n";
        assert_eq!(crypttab_keyfiles(t), [("home".to_string(), "/etc/keys/home.key".to_string())]);
        assert!(crypttab_keyfiles("").is_empty());
    }

    #[test]
    fn boot_of_a_tree() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        std::fs::create_dir_all(r.join("boot/efi/EFI/debian")).unwrap();
        std::fs::write(r.join("boot/efi/EFI/debian/shimx64.efi"), b"").unwrap();
        std::fs::create_dir_all(r.join("boot/grub")).unwrap();
        std::fs::write(r.join("boot/grub/grub.cfg"), b"").unwrap();
        std::fs::create_dir_all(r.join("etc/default")).unwrap();
        std::fs::write(r.join("etc/default/grub"), "GRUB_CMDLINE_LINUX=\"quiet\"\n").unwrap();
        std::fs::create_dir_all(r.join("usr/sbin")).unwrap();
        std::fs::write(r.join("usr/sbin/update-initramfs"), b"").unwrap();
        let b = boot_info(r, Some("/boot/efi"), "root=UUID=x ro");
        assert_eq!(b.loader, Loader::Grub);
        assert!(b.shim);
        assert_eq!(b.efi_id.as_deref(), Some("debian"));
        assert_eq!(b.initramfs, InitramfsTool::UpdateInitramfs);
        assert_eq!(b.grub_cmdline.as_deref(), Some("quiet"));
    }

    /// The btrfs method's manifest from fixture inputs (mount table, btrbk.conf, a boot
    /// tree), compared with the golden file the NAS-side parser test reads
    /// (`rynasd::pc_backup` tests). `RYNAS_UPDATE_GOLDEN=1` rewrites it.
    #[test]
    fn btrbk_manifest_matches_what_the_nas_reads() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        std::fs::create_dir_all(r.join("boot/EFI/systemd")).unwrap();
        std::fs::create_dir_all(r.join("boot/loader/entries")).unwrap();
        std::fs::write(r.join("boot/EFI/systemd/systemd-bootx64.efi"), vec![0u8; 4096]).unwrap();
        std::fs::write(r.join("boot/loader/entries/arch.conf"), "linux /vmlinuz-linux\n").unwrap();
        std::fs::write(r.join("boot/vmlinuz-linux"), vec![1u8; 8192]).unwrap();
        std::fs::create_dir_all(r.join("usr/lib/modules/6.17.7-arch1-1")).unwrap();
        std::fs::create_dir_all(r.join("usr/bin")).unwrap();
        std::fs::write(r.join("usr/bin/mkinitcpio"), b"").unwrap();
        std::fs::create_dir_all(r.join("etc")).unwrap();
        std::fs::write(r.join("etc/mkinitcpio.conf"), b"").unwrap();
        let all = mounts::parse(
            "22 1 0:25 /@ / rw,noatime shared:1 - btrfs /dev/nvme0n1p2 rw,subvolid=256,subvol=/@
40 22 0:25 /@home /home rw,noatime shared:2 - btrfs /dev/nvme0n1p2 rw,subvolid=257,subvol=/@home
42 22 0:25 /@log /var/log rw,noatime shared:4 - btrfs /dev/nvme0n1p2 rw,subvolid=258,subvol=/@log
45 22 259:1 / /boot rw,relatime shared:7 - vfat /dev/nvme0n1p1 rw,fmask=0077,dmask=0077
",
        );
        let conf = "snapshot_dir /.btrbk_snapshots\nsubvolume /\n  snapshot_name @\n  target ssh://nas.example.lan/volume1/backup/pc-1\nsubvolume /home\n  snapshot_name @home\n  target ssh://nas.example.lan/volume1/backup/pc-1\n";
        let sub = |path: &str, id| Subvolume { path: path.into(), id, captured: true, nocow: false };
        let mut m = Manifest {
            format: MANIFEST_FORMAT,
            written_at_ms: 1_762_905_600_000,
            agent_version: "0.5.1".into(),
            hostname: "pc-1".into(),
            arch: "x86_64".into(),
            boot: BootInfo { uefi: true, ..boot_info(r, Some("/boot"), "root=UUID=11111111-2222-3333-4444-555555555555 rootflags=subvol=@ rw") },
            disks: vec![Disk {
                name: "nvme0n1".into(),
                model: "Example NVMe 1TB".into(),
                serial: "S123456789".into(),
                size_bytes: 1_000_204_886_016,
                logical_sector: 512,
                table: Some(PartTable {
                    kind: "gpt".into(),
                    disk_guid: "0D4B3F6A-0000-4000-8000-000000000001".into(),
                    first_lba: 0,
                    last_lba: 1_953_525_134,
                    partitions: vec![
                        Partition { number: 1, start_lba: 2048, size_lba: 2_097_152, type_guid: "C12A7328-F81F-11D2-BA4B-00A0C93EC93B".into(), part_guid: "0D4B3F6A-0000-4000-8000-000000000002".into(), name: "EFI".into(), attrs: 0, dev: "nvme0n1p1".into() },
                        Partition { number: 2, start_lba: 2_099_200, size_lba: 1_951_425_935, type_guid: "0FC63DAF-8483-4772-8E79-3D69D8477DE4".into(), part_guid: "0D4B3F6A-0000-4000-8000-000000000003".into(), name: "root".into(), attrs: 0, dev: "nvme0n1p2".into() },
                    ],
                }),
                ..Default::default()
            }],
            filesystems: vec![Filesystem {
                device: "/dev/nvme0n1p2".into(),
                source: FsSource::Partition { disk: 0, number: 2 },
                fstype: "btrfs".into(),
                uuid: "11111111-2222-3333-4444-555555555555".into(),
                mounts: vec![
                    MountEntry { path: "/".into(), subvol: Some("@".into()), options: "rw,noatime".into() },
                    MountEntry { path: "/home".into(), subvol: Some("@home".into()), options: "rw,noatime".into() },
                    MountEntry { path: "/var/log".into(), subvol: Some("@log".into()), options: "rw,noatime".into() },
                ],
                subvolumes: vec![sub("@", 256), sub("@home", 257), sub("@log", 258), sub("@/.btrbk_snapshots/@home.20261111T0300", 300)],
                ..Default::default()
            }],
            fstab: "UUID=11111111-2222-3333-4444-555555555555 / btrfs subvol=/@,noatime 0 0\n".into(),
            ..Default::default()
        };
        let problems = add_btrbk(&mut m, &all, conf, r, true);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(m.boot.loader, Loader::SystemdBoot);
        assert_eq!(m.boot.initramfs, InitramfsTool::Mkinitcpio);
        let b = m.btrbk.as_mut().unwrap();
        assert_eq!(b.boot_copies.len(), 1);
        assert_eq!((b.boot_copies[0].mount.as_str(), b.boot_copies[0].files, b.boot_copies[0].bytes), ("/boot", 3, 4096 + 8192 + 21));
        assert!(r.join(".rynas/boot/boot/vmlinuz-linux").exists());
        // the signature holds file times: fixed for the golden file
        b.boot_copies[0].signature = "fixture".into();
        // @log is recreated empty, local btrbk snapshots are not part of the layout
        let caps: Vec<_> = m.filesystems[0].subvolumes.iter().map(|s| (s.path.as_str(), s.captured)).collect();
        assert_eq!(caps, [("@", true), ("@home", true), ("@log", false)]);
        // written atomically: no temporary file left
        let p = write(&m, r).unwrap();
        assert!(!r.join(".rynas/manifest.tmp").exists());
        let back: Manifest = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
        assert_eq!(back, m);
        let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../rynasd/tests/golden/pc-manifest-b2.json");
        let text = serde_json::to_string_pretty(&m).unwrap() + "\n";
        if std::env::var_os("RYNAS_UPDATE_GOLDEN").is_some() {
            std::fs::write(&golden, &text).unwrap();
        }
        let want: Manifest = serde_json::from_str(&std::fs::read_to_string(&golden).unwrap()).unwrap();
        assert_eq!(want, m, "golden file differs: RYNAS_UPDATE_GOLDEN=1 rewrites it");
    }

    /// The agent's own block: a failed boot copy keeps the last good record, marked stale,
    /// and its folder.
    #[test]
    fn a_failed_boot_copy_keeps_the_previous_record() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        std::fs::create_dir_all(r.join("boot/EFI/BOOT")).unwrap();
        std::fs::write(r.join("boot/EFI/BOOT/BOOTX64.EFI"), vec![7u8; 100]).unwrap();
        let all = mounts::parse(
            "22 1 0:25 /@ / rw shared:1 - btrfs /dev/vda2 rw,subvolid=256,subvol=/@
45 22 259:1 / /boot rw shared:7 - vfat /dev/vda1 rw
",
        );
        let block = BtrbkLayout {
            snapshot_dir: AGENT_SNAPSHOT_DIR.into(),
            subvolumes: vec![BtrbkSubvolume { name: "@".into(), path: "@".into(), mount: "/".into() }],
            agent: true,
            ..Default::default()
        };
        let mut m = Manifest { format: MANIFEST_FORMAT, ..Default::default() };
        assert!(add_block(&mut m, &all, block.clone(), r, true).is_empty());
        write(&m, r).unwrap();
        let good = m.btrbk.as_ref().unwrap().boot_copies[0].clone();
        assert_eq!(good.files, 1);
        // the ESP is not there any more (an empty folder): the copy is refused
        std::fs::remove_dir_all(r.join("boot/EFI")).unwrap();
        let mut m2 = Manifest { format: MANIFEST_FORMAT, ..Default::default() };
        let problems = add_block(&mut m2, &all, block, r, true);
        assert_eq!(problems.len(), 1);
        let b = m2.btrbk.unwrap();
        assert!(b.agent);
        assert_eq!(b.boot_copies.len(), 1);
        let c = &b.boot_copies[0];
        assert!(c.stale && c.error.as_deref().is_some_and(|e| e.contains("empty")));
        assert_eq!((c.files, &c.signature), (good.files, &good.signature));
        assert!(r.join(".rynas/boot/boot/EFI/BOOT/BOOTX64.EFI").exists());
    }

    #[test]
    fn one_run_at_a_time() {
        let t = tempfile::tempdir().unwrap();
        let first = lock(t.path(), std::time::Duration::ZERO).unwrap();
        let e = lock(t.path(), std::time::Duration::from_millis(300)).unwrap_err();
        assert!(e.contains("another rynas-agent run"));
        drop(first);
        assert!(lock(t.path(), std::time::Duration::ZERO).is_ok());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(t.path().join(".rynas")).unwrap().permissions().mode() & 0o777, 0o700);
    }
}
