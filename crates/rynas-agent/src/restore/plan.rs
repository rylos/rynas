//! The restore plan, from a manifest and the target disks: what goes where, checked before
//! anything is written, and shown as readable steps (ReaR-style).
//!
//! - Each source disk with a partition table maps to one target disk.
//! - The GPT keeps the label-id, numbers, types, PARTUUIDs, names and attributes; the
//!   partition that ends last grows or shrinks to the end of the target disk, the others keep
//!   their size. A smaller disk is refused only when the data does not fit.
//! - Filesystems keep their UUIDs and labels unless another attached disk has them (then new
//!   ones, and the restore rewrites fstab, crypttab, the loader entries and the cmdline).
//! - LUKS containers are made again with the same UUID and cipher (a new passphrase).
//! - LVM and md are not recreated by this version (preflight says so).

use rynas_api_types::pcagent::*;

use crate::gpt::{Entry, Gpt, name_bytes, parse_guid};


/// A target disk as the live system sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetDisk {
    pub name: String,
    pub size_bytes: u64,
    pub logical_sector: u32,
    pub model: String,
    pub serial: String,
}

/// What one filesystem becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsAction {
    /// Index in `manifest.filesystems`.
    pub fs: usize,
    pub fstype: String,
    /// `/dev/…` it is made on (a partition, or `/dev/mapper/<name>` of a LUKS container).
    pub device: String,
    pub uuid: String,
    pub label: String,
    /// The original UUID when a new one had to be made.
    pub old_uuid: Option<String>,
    /// FAT: 16 or 32 bits, as the original (0: not FAT).
    pub fat_bits: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuksAction {
    pub luks: usize,
    /// The partition it is made on.
    pub partition: String,
    pub mapper: String,
    pub uuid: String,
    pub version: u32,
    pub cipher: String,
    pub key_bits: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapAction {
    pub device: String,
    pub uuid: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct DiskPlan {
    /// Index in `manifest.disks`.
    pub source: usize,
    pub target: TargetDisk,
    pub gpt: Gpt,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub disks: Vec<DiskPlan>,
    pub luks: Vec<LuksAction>,
    pub filesystems: Vec<FsAction>,
    pub swap: Vec<SwapAction>,
    /// Old → new IDs (UUIDs and PARTUUIDs) for the text fixes.
    pub id_map: Vec<(String, String)>,
    /// Readable notes (what changes, what the user must do afterwards).
    pub notes: Vec<String>,
}

/// The kernel name of partition `n` of disk `disk` (`sda1`, `nvme0n1p1`, `mmcblk0p1`).
pub fn part_name(disk: &str, n: u32) -> String {
    if disk.chars().last().is_some_and(|c| c.is_ascii_digit()) {
        format!("{disk}p{n}")
    } else {
        format!("{disk}{n}")
    }
}

fn random_uuid() -> String {
    let g = crate::gpt::random_guid();
    let h: String = g.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn random_fat_id() -> String {
    let g = crate::gpt::random_guid();
    format!("{:02X}{:02X}-{:02X}{:02X}", g[0], g[1], g[2], g[3])
}

/// Bytes a filesystem needs on a target: its used bytes plus room (10 %, at least 256 MiB).
fn needed(used: u64) -> u64 {
    used + (used / 10).max(256 << 20)
}

/// Builds the plan. `in_use(uuid)` says whether another attached device has that ID.
pub fn build(m: &Manifest, targets: &[(usize, TargetDisk)], in_use: &dyn Fn(&str) -> bool) -> Result<Plan, String> {
    let mut notes = Vec::new();
    let reasons = m.ready_reasons();
    if reasons.contains(&ReadyReason::ManualLayout) {
        return Err("this PC used LVM or md: this version of the restore recreates GPT, LUKS and filesystems only; recreate the volumes by hand, then restore the files from the web interface".into());
    }
    let mut id_map: Vec<(String, String)> = Vec::new();
    let mut disks = Vec::new();
    for (src, t) in targets {
        let d = m.disks.get(*src).ok_or("no such source disk")?;
        let table = d.table.as_ref().ok_or_else(|| format!("disk {} has no partition table in the manifest", d.name))?;
        if table.kind != "gpt" {
            return Err("only GPT disks are restored by this version".into());
        }
        let sector = u64::from(t.logical_sector);
        if u64::from(d.logical_sector) != sector {
            return Err(format!("{}: {}-byte sectors, the target has {sector}", d.name, d.logical_sector));
        }
        let mut g = Gpt::new(sector);
        if let Some(guid) = parse_guid(&table.disk_guid) {
            if in_use(&table.disk_guid.to_lowercase()) {
                notes.push("the disk GUID is in use on another disk: a new one is made".into());
            } else {
                g.disk_guid = guid;
            }
        }
        let max_n = table.partitions.iter().map(|p| p.number).max().unwrap_or(0) as usize;
        if max_n > g.entries.len() {
            g.entries.resize(max_n, g.entries[0].clone());
        }
        let last_usable = g.last_usable(t.size_bytes);
        let last_part = table.partitions.iter().max_by_key(|p| p.start_lba + p.size_lba).map(|p| p.number);
        for p in &table.partitions {
            let ty = parse_guid(&p.type_guid).ok_or_else(|| format!("partition {}: bad type GUID", p.number))?;
            let mut guid = parse_guid(&p.part_guid).unwrap_or_else(crate::gpt::random_guid);
            if in_use(&p.part_guid.to_lowercase()) {
                let fresh = crate::gpt::random_guid();
                let new = crate::gpt::guid_text(&fresh);
                id_map.push((p.part_guid.to_lowercase(), new.to_lowercase()));
                guid = fresh;
            }
            let mut last = p.start_lba + p.size_lba - 1;
            if Some(p.number) == last_part {
                last = last_usable;
            }
            if p.start_lba > last_usable || last > last_usable {
                return Err(format!(
                    "partition {} starts or ends beyond the end of {} ({} bytes)",
                    p.number, t.name, t.size_bytes
                ));
            }
            g.entries[p.number as usize - 1] = Entry {
                type_guid: ty,
                guid,
                first_lba: p.start_lba,
                last_lba: last,
                attributes: p.attrs,
                name: name_bytes(&p.name),
            };
        }
        // the data must fit the partition that changed size
        if let Some(n) = last_part {
            let e = &g.entries[n as usize - 1];
            let new_bytes = (e.last_lba - e.first_lba + 1) * sector;
            let used: u64 = m
                .filesystems
                .iter()
                .filter(|f| fs_on_partition(m, f, *src, n))
                .map(|f| f.used_bytes)
                .sum();
            if used > 0 && needed(used) > new_bytes {
                return Err(format!(
                    "{} is too small: partition {n} would have {} MiB, the data needs about {} MiB",
                    t.name,
                    new_bytes >> 20,
                    needed(used) >> 20
                ));
            }
        }
        disks.push(DiskPlan {
            source: *src,
            target: t.clone(),
            gpt: g,
        });
    }
    // where a manifest partition lands
    let target_part = |disk: usize, number: u32| -> Option<String> {
        disks.iter().find(|d| d.source == disk).map(|d| format!("/dev/{}", part_name(&d.target.name, number)))
    };
    let mut luks = Vec::new();
    for (i, l) in m.luks.iter().enumerate() {
        let FsSource::Partition { disk, number } = l.source else {
            return Err(format!("LUKS container {} is not on a partition", l.device));
        };
        let Some(part) = target_part(disk, number) else { continue };
        let mut uuid = l.uuid.clone();
        if in_use(&uuid.to_lowercase()) {
            let new = random_uuid();
            id_map.push((uuid.to_lowercase(), new.clone()));
            notes.push(format!("LUKS {}: UUID in use on another disk, new {new}", l.uuid));
            uuid = new;
        }
        luks.push(LuksAction {
            luks: i,
            partition: part,
            mapper: if l.mapper.is_empty() { format!("rynas-luks{i}") } else { l.mapper.clone() },
            uuid,
            version: l.version.max(1),
            cipher: l.cipher.clone(),
            key_bits: l.key_bits,
        });
        notes.push("LUKS: TPM2/FIDO2 tokens are not restored; enroll them again after the first boot".into());
    }
    let mut filesystems = Vec::new();
    for (i, f) in m.filesystems.iter().enumerate() {
        if f.capture == Some(CaptureMethod::Skipped) {
            notes.push(format!("{}: was not in the backup, left out", f.mounts.first().map(|x| x.path.as_str()).unwrap_or(&f.device)));
            continue;
        }
        let device = match &f.source {
            FsSource::Partition { disk, number } => target_part(*disk, *number),
            FsSource::Luks { luks: li } => luks.iter().find(|a| a.luks == *li).map(|a| format!("/dev/mapper/{}", a.mapper)),
            _ => None,
        };
        let Some(device) = device else {
            notes.push(format!("{} ({}): not on a restored disk, left out", f.device, f.fstype));
            continue;
        };
        let (uuid, old) = if f.uuid.is_empty() {
            (String::new(), None)
        } else if in_use(&f.uuid.to_lowercase()) {
            let new = if f.fstype == "vfat" { random_fat_id() } else { random_uuid() };
            id_map.push((f.uuid.clone(), new.clone()));
            (new, Some(f.uuid.clone()))
        } else {
            (f.uuid.clone(), None)
        };
        if let Some(o) = &old {
            notes.push(format!("{}: UUID {o} is in use on another disk, the new one is {uuid}", f.fstype));
        }
        filesystems.push(FsAction {
            fs: i,
            fstype: f.fstype.clone(),
            device,
            uuid,
            label: f.label.clone(),
            old_uuid: old,
            fat_bits: if f.fstype != "vfat" {
                0
            } else if f.features.iter().any(|x| x == "fat16") {
                16
            } else {
                32
            },
        });
    }
    let mut swap = Vec::new();
    for s in m.swap.iter().filter(|s| !s.file) {
        if let FsSource::Partition { disk, number } = s.source
            && let Some(dev) = target_part(disk, number)
        {
            swap.push(SwapAction {
                device: dev,
                uuid: s.uuid.clone(),
                label: s.label.clone(),
            });
        }
    }
    if m.disks.iter().enumerate().any(|(i, d)| d.table.is_some() && !targets.iter().any(|(s, _)| *s == i))
        && m.filesystems.iter().any(|f| matches!(f.source, FsSource::Partition { disk, .. } if !targets.iter().any(|(s, _)| *s == disk)) && f.capture != Some(CaptureMethod::Skipped))
    {
        notes.push("some disks of the PC get no target: their filesystems are left out".into());
    }
    Ok(Plan {
        disks,
        luks,
        filesystems,
        swap,
        id_map,
        notes,
    })
}

/// `f` lives on partition `n` of disk `disk` (directly or inside LUKS).
fn fs_on_partition(m: &Manifest, f: &Filesystem, disk: usize, n: u32) -> bool {
    match &f.source {
        FsSource::Partition { disk: d, number } => *d == disk && *number == n,
        FsSource::Luks { luks } => m
            .luks
            .get(*luks)
            .is_some_and(|l| matches!(l.source, FsSource::Partition { disk: d, number } if d == disk && number == n)),
        _ => false,
    }
}

/// The mkfs command of an action (argv).
pub fn mkfs_argv(a: &FsAction) -> Result<Vec<String>, String> {
    let mut v: Vec<String> = match a.fstype.as_str() {
        "ext2" | "ext3" | "ext4" => {
            let mut v = vec![format!("mkfs.{}", a.fstype), "-F".into(), "-q".into()];
            if !a.uuid.is_empty() {
                v.extend(["-U".into(), a.uuid.clone()]);
            }
            if !a.label.is_empty() {
                v.extend(["-L".into(), a.label.clone()]);
            }
            v
        }
        "xfs" => {
            let mut v = vec!["mkfs.xfs".into(), "-f".into(), "-q".into()];
            if !a.uuid.is_empty() {
                v.extend(["-m".into(), format!("uuid={}", a.uuid)]);
            }
            if !a.label.is_empty() {
                v.extend(["-L".into(), a.label.clone()]);
            }
            v
        }
        "btrfs" => {
            let mut v = vec!["mkfs.btrfs".into(), "-f".into(), "-q".into()];
            if !a.uuid.is_empty() {
                v.extend(["-U".into(), a.uuid.clone()]);
            }
            if !a.label.is_empty() {
                v.extend(["-L".into(), a.label.clone()]);
            }
            v
        }
        "vfat" => {
            let bits = if a.fat_bits == 16 { "16" } else { "32" };
            let mut v = vec!["mkfs.vfat".into(), "-F".into(), bits.into()];
            if !a.uuid.is_empty() {
                v.extend(["-i".into(), a.uuid.replace('-', "")]);
            }
            if !a.label.is_empty() {
                v.extend(["-n".into(), a.label.clone()]);
            }
            v
        }
        "f2fs" => {
            let mut v = vec!["mkfs.f2fs".into(), "-f".into(), "-q".into()];
            if !a.uuid.is_empty() {
                v.extend(["-U".into(), a.uuid.clone()]);
            }
            if !a.label.is_empty() {
                v.extend(["-l".into(), a.label.clone()]);
            }
            v
        }
        other => return Err(format!("{other}: not a filesystem this version restores")),
    };
    v.push(a.device.clone());
    Ok(v)
}

/// The packages a missing tool comes in (Debian/Ubuntu, Fedora, Arch names).
pub fn package_of(tool: &str) -> &'static str {
    match tool {
        "mkfs.ext2" | "mkfs.ext3" | "mkfs.ext4" => "e2fsprogs",
        "mkfs.xfs" => "xfsprogs",
        "mkfs.btrfs" | "btrfs" => "btrfs-progs",
        "mkfs.vfat" => "dosfstools",
        "mkfs.f2fs" => "f2fs-tools",
        "mkswap" | "mount" | "umount" | "wipefs" => "util-linux",
        "cryptsetup" => "cryptsetup",
        "udevadm" => "systemd / udev",
        _ => "the distribution's package",
    }
}

/// The tools the plan runs on the live system.
pub fn tools(p: &Plan) -> Vec<String> {
    let mut v: Vec<String> = vec!["mount".into(), "umount".into(), "wipefs".into()];
    for a in &p.filesystems {
        if let Ok(argv) = mkfs_argv(a) {
            v.push(argv[0].clone());
        }
        if a.fstype == "btrfs" {
            v.push("btrfs".into());
        }
    }
    if !p.luks.is_empty() {
        v.push("cryptsetup".into());
    }
    if !p.swap.is_empty() {
        v.push("mkswap".into());
    }
    v.sort();
    v.dedup();
    v
}

/// The plan as readable steps.
pub fn describe(m: &Manifest, p: &Plan) -> Vec<String> {
    let mut out = Vec::new();
    for d in &p.disks {
        let src = &m.disks[d.source];
        out.push(format!(
            "# disk {} ({} {}, {} GiB) → /dev/{} ({} {}, {} GiB): ERASED",
            src.name,
            src.model,
            src.serial,
            src.size_bytes >> 30,
            d.target.name,
            d.target.model,
            d.target.serial,
            d.target.size_bytes >> 30
        ));
        out.push(format!("wipefs -a /dev/{}", d.target.name));
        out.push(format!("write GPT label-id {}", crate::gpt::guid_text(&d.gpt.disk_guid)));
        for (n, e) in d.gpt.used() {
            out.push(format!(
                "  partition {n}: start {} size {} MiB type {} PARTUUID {} {}",
                e.first_lba,
                ((e.last_lba - e.first_lba + 1) * d.gpt.sector) >> 20,
                crate::gpt::guid_text(&e.type_guid),
                crate::gpt::guid_text(&e.guid),
                e.name()
            ));
        }
    }
    for l in &p.luks {
        out.push(format!(
            "cryptsetup luksFormat --type luks{} --uuid {} --cipher {} --key-size {} {} && cryptsetup open {} {}",
            l.version, l.uuid, l.cipher, l.key_bits, l.partition, l.partition, l.mapper
        ));
    }
    for a in &p.filesystems {
        if let Ok(argv) = mkfs_argv(a) {
            out.push(argv.join(" "));
        }
        let f = &m.filesystems[a.fs];
        for s in f.subvolumes.iter() {
            out.push(format!("  btrfs subvolume create {}{}", s.path, if s.captured { "" } else { " (empty)" }));
        }
        if let Some(d) = &f.default_subvol {
            out.push(format!("  btrfs subvolume set-default {d}"));
        }
    }
    for s in &p.swap {
        out.push(format!("mkswap -U {} {}", s.uuid, s.device));
    }
    out.push("mount the filesystems as the PC's fstab says, below /run/rynas-agent/target".into());
    out.push("restore the version's files (owners, modes, ACLs, xattrs, hard links)".into());
    if !p.id_map.is_empty() {
        out.push(format!("rewrite {} changed IDs in fstab, crypttab, loader entries and the cmdline", p.id_map.len()));
    }
    out.push(format!("chroot: /etc/kernel/cmdline from the manifest, initramfs ({:?}), boot loader ({:?}), removable EFI fallback", m.boot.initramfs, m.boot.loader));
    for n in &p.notes {
        out.push(format!("note: {n}"));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const BIOS_BOOT: &str = "21686148-6449-6E6F-744E-656564454649";
    const ESP: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";

    pub fn sample() -> Manifest {
        let disk = Disk {
            name: "vda".into(),
            model: "QEMU".into(),
            serial: "src-disk0".into(),
            size_bytes: 4 << 30,
            logical_sector: 512,
            table: Some(PartTable {
                kind: "gpt".into(),
                disk_guid: "1E2B8F6D-3643-4D8D-AAC5-AB7EDEBECACC".into(),
                partitions: vec![
                    Partition { number: 14, start_lba: 2048, size_lba: 6144, type_guid: BIOS_BOOT.into(), part_guid: "2BDA7513-0000-4000-8000-000000000001".into(), ..Default::default() },
                    Partition { number: 15, start_lba: 8192, size_lba: 253952, type_guid: ESP.into(), part_guid: "0397CC02-0000-4000-8000-000000000002".into(), ..Default::default() },
                    Partition { number: 1, start_lba: 262144, size_lba: 8126431, type_guid: "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709".into(), part_guid: "3612A9BF-0000-4000-8000-000000000003".into(), ..Default::default() },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        Manifest {
            format: 1,
            hostname: "pc".into(),
            boot: BootInfo { uefi: true, loader: Loader::Grub, ..Default::default() },
            disks: vec![disk],
            filesystems: vec![
                Filesystem {
                    device: "/dev/vda1".into(),
                    source: FsSource::Partition { disk: 0, number: 1 },
                    fstype: "ext4".into(),
                    uuid: "2d5cddec-8777-4aef-9b8d-1d7093e60982".into(),
                    mounts: vec![MountEntry { path: "/".into(), ..Default::default() }],
                    used_bytes: 1 << 30,
                    capture: Some(CaptureMethod::Hot),
                    ..Default::default()
                },
                Filesystem {
                    device: "/dev/vda15".into(),
                    source: FsSource::Partition { disk: 0, number: 15 },
                    fstype: "vfat".into(),
                    uuid: "91BC-A784".into(),
                    mounts: vec![MountEntry { path: "/boot/efi".into(), ..Default::default() }],
                    used_bytes: 10 << 20,
                    capture: Some(CaptureMethod::Files),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn target(size: u64) -> TargetDisk {
        TargetDisk { name: "vdc".into(), size_bytes: size, logical_sector: 512, model: "QEMU".into(), serial: "t".into() }
    }

    #[test]
    fn smaller_disk_fits() {
        let m = sample();
        let p = build(&m, &[(0, target(3 << 30))], &|_| false).unwrap();
        let g = &p.disks[0].gpt;
        let last = &g.entries[0];
        assert_eq!(last.first_lba, 262144);
        assert_eq!(last.last_lba, g.last_usable(3 << 30));
        assert_eq!(g.entries[13].last_lba, 2048 + 6144 - 1);
        assert_eq!(p.filesystems.len(), 2);
        assert_eq!(p.filesystems[0].device, "/dev/vdc1");
        assert_eq!(p.filesystems[0].uuid, "2d5cddec-8777-4aef-9b8d-1d7093e60982");
        assert!(p.id_map.is_empty());
        let argv = mkfs_argv(&p.filesystems[1]).unwrap();
        assert_eq!(argv, vec!["mkfs.vfat", "-F", "32", "-i", "91BCA784", "/dev/vdc15"]);
    }

    #[test]
    fn too_small_and_conflicts() {
        let m = sample();
        let e = build(&m, &[(0, target(1 << 30))], &|_| false).unwrap_err();
        assert!(e.contains("too small"), "{e}");
        let p = build(&m, &[(0, target(8 << 30))], &|u| u == "2d5cddec-8777-4aef-9b8d-1d7093e60982").unwrap();
        assert_eq!(p.filesystems[0].old_uuid.as_deref(), Some("2d5cddec-8777-4aef-9b8d-1d7093e60982"));
        assert_eq!(p.id_map.len(), 1);
        assert!(!describe(&m, &p).is_empty());
    }

    #[test]
    fn names() {
        assert_eq!(part_name("sda", 1), "sda1");
        assert_eq!(part_name("nvme0n1", 2), "nvme0n1p2");
    }

    #[test]
    fn lvm_refused() {
        let mut m = sample();
        m.lvm_report = Some("{}".into());
        assert!(build(&m, &[(0, target(8 << 30))], &|_| false).is_err());
    }
}
