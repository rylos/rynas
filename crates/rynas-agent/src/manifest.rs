//! The layout manifest (`/.rynas/manifest.json`, `rynas_api_types::pcagent::Manifest`):
//! disks by model, serial and size with their GPT, filesystems with UUIDs, labels, mounts and
//! btrfs subvolumes, LUKS parameters (never a header or a key slot), md/LVM reports, swap,
//! fstab, crypttab, the kernel command line and the boot loader. Written into the root
//! filesystem (temporary file + rename) before every run, so every version carries the
//! layout of its own moment; also by `rynas-agent manifest` (and, later, the btrfs method's
//! hooks).

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

/// Writes the manifest into `/.rynas/manifest.json` of the root filesystem.
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
    Ok(p)
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
}
