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
//! - LVM: linear volumes on PVs that are partitions or LUKS containers, with the same names,
//!   extent size, sizes and UUIDs ([`super::lvm`]); LUKS inside a volume too.
//! - md is not recreated by this version (the plan refuses it).

use rynas_api_types::pcagent::*;

use super::fstab::{self, FstabLine};
use super::paths::under;
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
    /// The partition it is made on (or `/dev/<vg>/<lv>` of a logical volume).
    pub partition: String,
    pub mapper: String,
    pub uuid: String,
    pub version: u32,
    pub cipher: String,
    pub key_bits: u32,
    /// Inside a logical volume: made after the LVM step (the others before it).
    pub on_lvm: bool,
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
    /// LVM volume groups made again (after the LUKS containers on partitions).
    pub lvm: Vec<super::lvm::VgAction>,
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
pub(crate) fn needed(used: u64) -> u64 {
    used + (used / 10).max(256 << 20)
}

/// Builds the plan. `in_use(uuid)` says whether another attached device has that ID
/// (`vg:<name>`: whether an LVM volume group of that name is on another attached disk).
pub fn build(m: &Manifest, targets: &[(usize, TargetDisk)], in_use: &dyn Fn(&str) -> bool) -> Result<Plan, String> {
    let mut notes = Vec::new();
    if !m.md.is_empty() || m.filesystems.iter().any(|f| matches!(f.source, FsSource::Md { .. })) {
        return Err("this PC used md (software RAID): this version of the restore recreates GPT, LUKS, LVM and filesystems, not md arrays; recreate them by hand, then restore the files from the web interface".into());
    }
    let layout = match &m.lvm_report {
        Some(r) => LvmLayout::parse(r)?,
        // LVM in use without a report (it could not be read at backup time)
        None if !m.lvm_unsupported().is_empty() => {
            return Err(format!("this PC used LVM, but {}: recreate the volumes by hand, then restore the files from the web interface", m.lvm_unsupported().join("; ")));
        }
        None => LvmLayout::default(),
    };
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
    // LUKS on partitions, then the LVM groups (their PVs may be such containers), then LUKS
    // inside logical volumes
    let mut luks = Vec::new();
    let luks_action = |i: usize, l: &Luks, partition: String, on_lvm: bool, id_map: &mut Vec<(String, String)>, notes: &mut Vec<String>| {
        let mut uuid = l.uuid.clone();
        if in_use(&uuid.to_lowercase()) {
            let new = random_uuid();
            id_map.push((uuid.to_lowercase(), new.clone()));
            notes.push(format!("LUKS {}: UUID in use on another disk, new {new}", l.uuid));
            uuid = new;
        }
        notes.push("LUKS: TPM2/FIDO2 tokens are not restored; enroll them again after the first boot".into());
        LuksAction {
            luks: i,
            partition,
            mapper: if l.mapper.is_empty() { format!("rynas-luks{i}") } else { l.mapper.clone() },
            uuid,
            version: l.version.max(1),
            cipher: l.cipher.clone(),
            key_bits: l.key_bits,
            on_lvm,
        }
    };
    for (i, l) in m.luks.iter().enumerate() {
        match l.source {
            FsSource::Partition { disk, number } => {
                if let Some(part) = target_part(disk, number) {
                    luks.push(luks_action(i, l, part, false, &mut id_map, &mut notes));
                }
            }
            FsSource::Lvm { .. } => {}
            _ => return Err(format!("LUKS container {} is not on a partition or a logical volume", l.device)),
        }
    }
    let lvm = {
        let part_bytes = |disk: usize, number: u32| -> Option<u64> {
            let d = disks.iter().find(|d| d.source == disk)?;
            let e = d.gpt.entries.get(number as usize - 1)?;
            Some((e.last_lba - e.first_lba + 1) * d.gpt.sector)
        };
        let orig_bytes = |disk: usize, number: u32| -> u64 {
            m.disks.get(disk).and_then(|d| d.table.as_ref().map(|t| (t, u64::from(d.logical_sector)))).and_then(|(t, s)| t.partitions.iter().find(|p| p.number == number).map(|p| p.size_lba * s)).unwrap_or(0)
        };
        let pv_target = |dev: &str| -> super::lvm::PvTarget {
            use super::lvm::PvTarget;
            for (di, d) in m.disks.iter().enumerate() {
                if format!("/dev/{}", d.name) == dev {
                    return PvTarget::Unsupported(format!("a physical volume on the whole disk {}, without a partition table: not recreated by this version", d.name));
                }
                let Some(t) = &d.table else { continue };
                if let Some(p) = t.partitions.iter().find(|p| format!("/dev/{}", part_name(&d.name, p.number)) == dev) {
                    return match (target_part(di, p.number), part_bytes(di, p.number)) {
                        (Some(device), Some(bytes)) => PvTarget::On { device, bytes, orig_bytes: orig_bytes(di, p.number) },
                        _ => PvTarget::NoTarget,
                    };
                }
            }
            if let Some((i, l)) = m.luks.iter().enumerate().find(|(_, l)| l.device == dev || (!l.mapper.is_empty() && format!("/dev/mapper/{}", l.mapper) == dev)) {
                let FsSource::Partition { disk, number } = l.source else {
                    return PvTarget::Unsupported("a physical volume in a LUKS container that is not on a partition".into());
                };
                return match (luks.iter().find(|a| a.luks == i), part_bytes(disk, number)) {
                    (Some(a), Some(bytes)) => PvTarget::On {
                        device: format!("/dev/mapper/{}", a.mapper),
                        bytes: bytes.saturating_sub(super::lvm::LUKS_HEADER),
                        orig_bytes: orig_bytes(disk, number).saturating_sub(super::lvm::LUKS_HEADER),
                    },
                    _ => PvTarget::NoTarget,
                };
            }
            if dev.starts_with("/dev/md") {
                return PvTarget::Unsupported("a physical volume on md: not recreated by this version".into());
            }
            PvTarget::Unsupported("not a partition or a LUKS container the manifest knows".into())
        };
        super::lvm::plan(m, &layout, &pv_target, in_use, &mut notes)?
    };
    let lv_device = |src: &FsSource| -> Option<String> {
        let FsSource::Lvm { vg, lv } = src else { return None };
        lvm.iter().find(|a| a.name == *vg).filter(|a| a.lvs.iter().any(|l| l.name == *lv)).map(|_| format!("/dev/{vg}/{lv}"))
    };
    for (i, l) in m.luks.iter().enumerate() {
        if let Some(dev) = lv_device(&l.source) {
            luks.push(luks_action(i, l, dev, true, &mut id_map, &mut notes));
        }
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
            s @ FsSource::Lvm { .. } => lv_device(s),
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
        let dev = match &s.source {
            FsSource::Partition { disk, number } => target_part(*disk, *number),
            src @ FsSource::Lvm { .. } => lv_device(src),
            _ => None,
        };
        if let Some(dev) = dev {
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
        lvm,
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
        "pvcreate" | "vgcreate" | "lvcreate" | "vgchange" | "vgs" | "vgcfgbackup" | "vgcfgrestore" => "lvm2",
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
    v.extend(super::lvm::tools(&p.lvm).into_iter().map(String::from));
    v.sort();
    v.dedup();
    v
}

/// The plan as readable steps.
pub fn describe(m: &Manifest, p: &Plan, pool: bool) -> Vec<String> {
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
    let luks_line = |l: &LuksAction| {
        format!(
            "cryptsetup luksFormat --type luks{} --uuid {} --cipher {} --key-size {} {} && cryptsetup open {} {}",
            l.version, l.uuid, l.cipher, l.key_bits, l.partition, l.partition, l.mapper
        )
    };
    out.extend(p.luks.iter().filter(|l| !l.on_lvm).map(luks_line));
    for vg in &p.lvm {
        out.extend(super::lvm::describe(vg));
    }
    out.extend(p.luks.iter().filter(|l| l.on_lvm).map(luks_line));
    for a in &p.filesystems {
        if let Ok(argv) = mkfs_argv(a) {
            out.push(argv.join(" "));
        }
        let f = &m.filesystems[a.fs];
        if !pool {
            for s in f.subvolumes.iter() {
                out.push(format!("  btrfs subvolume create {}{}", s.path, if s.captured { "" } else { " (empty)" }));
            }
        }
        if let Some(d) = &f.default_subvol {
            out.push(format!("  btrfs subvolume set-default {d}"));
        }
    }
    for s in &p.swap {
        out.push(format!("mkswap -U {} {}", s.uuid, s.device));
    }
    out.push("mount the filesystems as the PC's fstab says, below /mnt/rynas-restore".into());
    if pool {
        out.push("receive the chosen subvolumes from the NAS (below), make the ones not in the backup empty, lay down the boot copies".into());
    } else {
        out.push("restore the version's files (owners, modes, ACLs, xattrs, hard links)".into());
    }
    if !p.id_map.is_empty() {
        out.push(format!("rewrite {} changed IDs in fstab, crypttab, loader entries and the cmdline", p.id_map.len()));
    }
    if !p.lvm.is_empty() {
        out.push(format!("LVM in the initramfs: lvm2 in the restored system{}", if m.boot.initramfs == InitramfsTool::Mkinitcpio { ", the lvm2 hook in mkinitcpio's HOOKS (added if missing)" } else { "" }));
    }
    out.push(format!("chroot: /etc/kernel/cmdline from the manifest, initramfs ({:?}), boot loader ({:?}), removable EFI fallback", m.boot.initramfs, m.boot.loader));
    for n in &p.notes {
        out.push(format!("note: {n}"));
    }
    out
}


// ------------------------------------------------------------------ text fixes

/// Marker of an fstab line the restore commented out (a subvolume that was not restored); a
/// later "add subvolumes" restore takes it away again.
pub const RESTORE_MARK: &str = "# rynas-restore: ";

/// Every old ID replaced by its new one (as written, and in upper case: FAT serials and
/// GPT GUIDs are often upper case).
pub fn rewrite_ids(text: &str, map: &[(String, String)]) -> String {
    let mut t = text.to_string();
    for (old, new) in map {
        t = t.replace(old.as_str(), new).replace(&old.to_uppercase(), &new.to_uppercase());
    }
    t
}

/// Comments out the fstab lines of subvolumes that were not restored (`skipped`: paths
/// from the top level), and of the subvolumes below them, with [`RESTORE_MARK`].
pub fn comment_unrestored(fstab: &str, skipped: &[String]) -> String {
    fstab::map_lines(fstab, |l| match FstabLine::parse(l).filter(|f| f.fstype == "btrfs").and_then(|f| f.subvol()) {
        Some(s) if !s.is_empty() && skipped.iter().any(|k| !k.trim_matches('/').is_empty() && under(k, &s)) => format!("{RESTORE_MARK}{l}"),
        _ => l.to_string(),
    })
}

/// Uncomments the lines [`comment_unrestored`] commented for the subvolumes now restored.
pub fn uncomment_restored(fstab: &str, restored: &[String]) -> String {
    fstab::map_lines(fstab, |l| match l.strip_prefix(RESTORE_MARK) {
        Some(orig) if FstabLine::parse(orig).and_then(|f| f.subvol()).is_some_and(|s| restored.iter().any(|k| k.trim_matches('/') == s)) => orig.to_string(),
        _ => l.to_string(),
    })
}

/// (old ID, path from the top level) of the subvolumes of the btrfs filesystem an fstab
/// spec names (`UUID=`, `LABEL=`, `PARTUUID=`, a device; the only btrfs filesystem when the
/// spec says nothing the manifest knows).
pub fn subvol_ids(m: &Manifest, spec: &str) -> Vec<(u64, String)> {
    let line = FstabLine { spec: spec.to_string(), path: String::new(), fstype: String::new(), options: String::new() };
    let part_guid = |f: &Filesystem| match &f.source {
        FsSource::Partition { disk, number } => m.disks.get(*disk).and_then(|d| d.table.as_ref()).and_then(|t| t.partitions.iter().find(|p| p.number == *number)).map(|p| p.part_guid.clone()),
        _ => None,
    };
    let btrfs = || m.filesystems.iter().filter(|f| f.fstype == "btrfs");
    let named = btrfs().find(|f| {
        let eq = |a: &str, b: Option<String>| b.is_some_and(|b| !a.is_empty() && a.eq_ignore_ascii_case(&b));
        eq(&f.uuid, line.id("UUID=")) || eq(&f.label, line.id("LABEL=")) || part_guid(f).is_some_and(|g| eq(&g, line.id("PARTUUID="))) || (!f.device.is_empty() && f.device == spec)
    });
    let fs = named.or_else(|| if btrfs().count() == 1 { btrfs().next() } else { None });
    fs.map(|f| f.subvolumes.iter().filter(|s| s.id != 0).map(|s| (s.id, s.path.trim_start_matches('/').to_string())).collect()).unwrap_or_default()
}

/// The subvolume IDs of the root filesystem (for `rootflags=`).
fn root_subvol_ids(m: &Manifest) -> Vec<(u64, String)> {
    m.root().map(|f| f.subvolumes.iter().filter(|s| s.id != 0).map(|s| (s.id, s.path.trim_start_matches('/').to_string())).collect()).unwrap_or_default()
}

/// Every fix of `/etc/fstab` after a restore, in order (the real run and the dry run): the
/// subvolume IDs out (matched by the **old** specs), the lines of subvolumes not restored
/// commented, then the old IDs replaced by the new ones.
pub fn fix_fstab(text: &str, m: &Manifest, skipped: &[String], map: &[(String, String)]) -> String {
    let t = fstab::fix_subvolids(text, &|spec| subvol_ids(m, spec));
    rewrite_ids(&comment_unrestored(&t, skipped), map)
}

/// Every fix of a kernel command line (`/etc/kernel/cmdline`, `/etc/default/grub`, a loader
/// entry): the root's subvolume ID out of `rootflags=`, then the old IDs replaced.
pub fn fix_cmdline(text: &str, m: &Manifest, map: &[(String, String)]) -> String {
    rewrite_ids(&fstab::fix_rootflags(text, &root_subvol_ids(m)), map)
}

/// The checks after the ID fixes: the root filesystem's ID (`root_ids`: its new UUID and
/// PARTUUID) on an active fstab line mounted at `/` (when fstab names `/` by an ID), and no
/// old ID left in the loader entries and command lines (`texts`: name, content).
pub fn sanity(fstab: &str, root_ids: &[String], texts: &[(String, String)], map: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(l) = fstab::parse(fstab).into_iter().find(|l| l.path == "/") {
        let id = l.id("UUID=").or_else(|| l.id("PARTUUID=")).map(|v| v.to_lowercase());
        let ids: Vec<String> = root_ids.iter().filter(|i| !i.is_empty()).map(|i| i.to_lowercase()).collect();
        if let Some(id) = id
            && !ids.contains(&id)
        {
            problems.push(format!("/etc/fstab: the root line names {}, not the restored root filesystem", l.spec));
        }
    } else if !fstab.trim().is_empty() {
        problems.push("/etc/fstab: no active line mounts /".into());
    }
    problems.extend(fstab::stale_subvolids(Some(fstab), None).into_iter().map(|p| format!("/etc/fstab: {p} (the restored subvolumes have new IDs)")));
    for (name, text) in texts {
        let lower = text.to_lowercase();
        for (old, _) in map {
            if lower.contains(&old.to_lowercase()) {
                problems.push(format!("{name}: still names the old ID {old}"));
            }
        }
        problems.extend(fstab::stale_subvolids(None, Some(text)).into_iter().map(|p| format!("{name}: {p} (the restored subvolumes have new IDs)")));
    }
    problems
}

/// A diff of two texts, line by line (`-`/`+` for changed lines only).
pub fn diff_lines(name: &str, before: &str, after: &str) -> Vec<String> {
    let (a, b): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
    let mut out = Vec::new();
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied(), b.get(i).copied());
        if x != y {
            if let Some(x) = x {
                out.push(format!("- {x}"));
            }
            if let Some(y) = y {
                out.push(format!("+ {y}"));
            }
        }
    }
    if out.is_empty() {
        return vec![format!("{name}: no change")];
    }
    let mut v = vec![format!("--- {name}")];
    v.extend(out);
    v
}

/// Every ID of the manifest mapped to a made-up one, as if every one were taken by another
/// disk: what a dry run feeds the real rewrites with.
pub fn mock_ids(m: &Manifest) -> Vec<(String, String)> {
    let mut map: Vec<(String, String)> = Vec::new();
    let mut n = 0u32;
    let mut add = |id: &str, fat: bool| {
        if id.is_empty() || map.iter().any(|(o, _)| o.eq_ignore_ascii_case(id)) {
            return;
        }
        n += 1;
        let new = if fat { format!("0000-{n:04X}") } else { format!("00000000-0000-4000-8000-{n:012x}") };
        map.push((id.to_lowercase(), new));
    };
    for f in &m.filesystems {
        add(&f.uuid, f.fstype == "vfat");
    }
    for d in &m.disks {
        if let Some(t) = &d.table {
            for p in &t.partitions {
                add(&p.part_guid, false);
            }
        }
    }
    for l in &m.luks {
        add(&l.uuid, false);
    }
    for s in &m.swap {
        add(&s.uuid, false);
    }
    // FAT serials are matched as written (upper case)
    for (o, _) in map.iter_mut() {
        if o.len() == 9 && o.as_bytes()[4] == b'-' {
            *o = o.to_uppercase();
        }
    }
    map
}

/// The dry run of the text fixes: fstab, `/etc/kernel/cmdline` (a loader entry's `options`
/// line gets the same) and `GRUB_CMDLINE_LINUX` from the manifest, each through the real
/// fixes ([`fix_fstab`], [`fix_cmdline`]) on a copy with [`mock_ids`], shown as diffs.
pub fn dry_run_rewrites(m: &Manifest, skipped: &[String]) -> Vec<String> {
    let map = mock_ids(m);
    let mut out = vec![format!("dry run of the text fixes, every ID replaced by a made-up one ({} IDs):", map.len())];
    out.extend(diff_lines("/etc/fstab", &m.fstab, &fix_fstab(&m.fstab, m, skipped, &map)));
    let cmdline = super::exec::kernel_cmdline(m);
    if !cmdline.is_empty() {
        out.extend(diff_lines("/etc/kernel/cmdline", &cmdline, &fix_cmdline(&cmdline, m, &map)));
    }
    if let Some(g) = &m.boot.grub_cmdline {
        let line = format!("GRUB_CMDLINE_LINUX=\"{g}\"");
        out.extend(diff_lines("/etc/default/grub", &line, &fix_cmdline(&line, m, &map)));
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
        assert!(!describe(&m, &p, false).is_empty());
        // the GPT disk GUID (label-id) taken by another attached disk: a new one, said so
        let guid = "1e2b8f6d-3643-4d8d-aac5-ab7edebecacc";
        let kept = build(&m, &[(0, target(8 << 30))], &|_| false).unwrap();
        assert_eq!(crate::gpt::guid_text(&kept.disks[0].gpt.disk_guid).to_lowercase(), guid);
        let p = build(&m, &[(0, target(8 << 30))], &|u| u == guid).unwrap();
        assert_ne!(crate::gpt::guid_text(&p.disks[0].gpt.disk_guid).to_lowercase(), guid);
        assert!(p.notes.iter().any(|n| n.contains("disk GUID")), "{:?}", p.notes);
    }

    #[test]
    fn names() {
        assert_eq!(part_name("sda", 1), "sda1");
        assert_eq!(part_name("nvme0n1", 2), "nvme0n1p2");
    }

    #[test]
    fn unreadable_lvm_report_refused() {
        let mut m = sample();
        m.lvm_report = Some("{}".into());
        assert!(build(&m, &[(0, target(8 << 30))], &|_| false).unwrap_err().contains("LVM report"));
        // LVM in use and no report at all
        m.lvm_report = None;
        m.filesystems[0].source = FsSource::Lvm { vg: "vg".into(), lv: "root".into() };
        assert!(build(&m, &[(0, target(8 << 30))], &|_| false).unwrap_err().contains("no LVM report"));
    }


    const FSTAB: &str = "# /etc/fstab\nPARTUUID=3612a9bf-0000-4000-8000-000000000003 / btrfs rw,subvol=/@ 0 0\nUUID=2d5cddec-8777-4aef-9b8d-1d7093e60982 /home btrfs rw,subvol=@home 0 0\nUUID=2d5cddec-8777-4aef-9b8d-1d7093e60982 /home/me/games btrfs rw,subvol=/@games 0 0\nUUID=91BC-A784 /boot vfat rw 0 2\n";

    #[test]
    fn unrestored_lines_are_commented_and_come_back() {
        let c = comment_unrestored(FSTAB, &["@home".into(), "@games".into()]);
        assert!(c.lines().any(|l| l.starts_with(RESTORE_MARK) && l.contains("subvol=@home")));
        assert!(c.lines().any(|l| l.starts_with(RESTORE_MARK) && l.contains("subvol=/@games")));
        assert!(c.lines().any(|l| l.starts_with("PARTUUID=") && l.contains("subvol=/@ ")), "the root line stays");
        assert!(c.ends_with('\n'));
        // adding @games later: only its line comes back
        let u = uncomment_restored(&c, &["@games".into()]);
        assert!(u.lines().any(|l| l.starts_with("UUID=") && l.contains("subvol=/@games")));
        assert!(u.lines().any(|l| l.starts_with(RESTORE_MARK) && l.contains("subvol=@home")));
        assert_eq!(uncomment_restored(&u, &["@home".into()]), FSTAB);
    }

    #[test]
    fn nested_subvolumes_of_a_skipped_one_are_commented_too() {
        let fstab = "UUID=x / btrfs subvol=/@ 0 0\nUUID=x /srv btrfs subvol=/@srv 0 0\nUUID=x /srv/www btrfs subvol=/@srv/www 0 0\nUUID=x /srvx btrfs subvol=/@srvx 0 0\n";
        let c = comment_unrestored(fstab, &["@srv".into()]);
        assert_eq!(c.lines().filter(|l| l.starts_with(RESTORE_MARK)).count(), 2, "{c}");
        assert!(c.contains("\nUUID=x /srvx "), "a name that only starts the same stays");
        // adding @srv back: its line returns, the nested one stays until it is added
        let u = uncomment_restored(&c, &["@srv".into()]);
        assert!(u.contains("\nUUID=x /srv btrfs") && u.contains(&format!("{RESTORE_MARK}UUID=x /srv/www")));
    }

    #[test]
    fn subvolume_ids_and_the_root_line() {
        // genfstab's subvolid= goes before anything else; a subvolid-only line becomes subvol=
        let mut m = sample();
        m.filesystems[0].fstype = "btrfs".into();
        m.filesystems[0].subvolumes = vec![Subvolume { path: "@".into(), id: 256, ..Default::default() }, Subvolume { path: "@home".into(), id: 257, ..Default::default() }];
        let fstab = "UUID=\"2d5cddec-8777-4aef-9b8d-1d7093e60982\" / btrfs rw,subvolid=256,subvol=/@ 0 0\nUUID=2d5cddec-8777-4aef-9b8d-1d7093e60982 /home btrfs rw,subvolid=257 0 0\n";
        let map = vec![("2d5cddec-8777-4aef-9b8d-1d7093e60982".to_string(), "00000000-0000-4000-8000-000000000001".to_string())];
        let t = fix_fstab(fstab, &m, &["@home".into()], &map);
        assert!(t.contains("UUID=\"00000000-0000-4000-8000-000000000001\" / btrfs rw,subvol=/@ 0 0"), "{t}");
        assert!(t.contains(&format!("{RESTORE_MARK}UUID=00000000-0000-4000-8000-000000000001 /home btrfs rw,subvol=/@home")), "{t}");
        // the quoted root line passes the check
        assert!(sanity(&t, &["00000000-0000-4000-8000-000000000001".into()], &[], &map).is_empty(), "{:?}", sanity(&t, &["00000000-0000-4000-8000-000000000001".into()], &[], &map));
        // what is left is reported
        let stale = sanity(fstab, &["2d5cddec-8777-4aef-9b8d-1d7093e60982".into()], &[("/etc/kernel/cmdline".into(), "rootflags=subvolid=256".into())], &[]);
        assert_eq!(stale.len(), 3, "{stale:?}");
        // the boot command line: the same mapping as the dry run (upper case too)
        let c = fix_cmdline("root=UUID=2D5CDDEC-8777-4AEF-9B8D-1D7093E60982 rootflags=subvol=@,subvolid=256 rw", &m, &map);
        assert_eq!(c, "root=UUID=00000000-0000-4000-8000-000000000001 rootflags=subvol=@ rw");
    }

    #[test]
    fn id_rewrites_on_copies_and_the_checks() {
        let map = vec![("2d5cddec-8777-4aef-9b8d-1d7093e60982".to_string(), "00000000-0000-4000-8000-000000000001".to_string()), ("3612a9bf-0000-4000-8000-000000000003".to_string(), "aaaaaaaa-0000-4000-8000-000000000009".to_string())];
        let t = rewrite_ids(FSTAB, &map);
        assert!(!t.contains("2d5cddec") && !t.contains("3612a9bf"));
        assert!(t.contains("PARTUUID=aaaaaaaa-0000-4000-8000-000000000009 / "));
        // upper-case forms too (FAT serials, GUIDs)
        assert_eq!(rewrite_ids("root=PARTUUID=3612A9BF-0000-4000-8000-000000000003", &map), "root=PARTUUID=AAAAAAAA-0000-4000-8000-000000000009");
        // the checks: the new root ID on the active line of /, no old ID in the entries
        assert!(sanity(&t, &["AAAAAAAA-0000-4000-8000-000000000009".into()], &[("entry".into(), "options root=PARTUUID=aaaaaaaa-0000-4000-8000-000000000009".into())], &map).is_empty());
        let bad = sanity(FSTAB, &["aaaaaaaa-0000-4000-8000-000000000009".into()], &[("entry".into(), "options root=PARTUUID=3612A9BF-0000-4000-8000-000000000003".into())], &map);
        assert_eq!(bad.len(), 2, "{bad:?}");
        let commented = comment_unrestored(&t, &["@".into()]);
        assert!(sanity(&commented, &["x".into()], &[], &map).iter().any(|p| p.contains("no active line")));
    }

    #[test]
    fn dry_run_uses_mock_ids_and_shows_diffs() {
        let mut m = sample();
        m.fstab = "UUID=2d5cddec-8777-4aef-9b8d-1d7093e60982 / ext4 defaults 0 1\nUUID=91BC-A784 /boot/efi vfat umask=0077 0 1\n".into();
        m.boot.kernel_cmdline = Some("root=UUID=2d5cddec-8777-4aef-9b8d-1d7093e60982 ro quiet".into());
        m.boot.grub_cmdline = Some("quiet".into());
        let map = mock_ids(&m);
        assert!(map.iter().any(|(o, n)| o == "91BC-A784" && n.len() == 9), "{map:?}");
        assert!(map.iter().any(|(o, _)| o == "3612a9bf-0000-4000-8000-000000000003"));
        let out = dry_run_rewrites(&m, &[]);
        let text = out.join("\n");
        assert!(text.contains("--- /etc/fstab"), "{text}");
        assert!(text.contains("- UUID=2d5cddec-8777-4aef-9b8d-1d7093e60982 / ext4"), "{text}");
        assert!(text.contains("+ UUID=00000000-0000-4000-8000-"), "{text}");
        assert!(text.contains("+ UUID=0000-0"), "{text}");
        assert!(text.contains("--- /etc/kernel/cmdline"), "{text}");
        assert!(text.contains("/etc/default/grub: no change"), "{text}");
    }

    const LVM_TYPE: &str = "E6D6D379-F507-44C2-A23C-238F2A3DF928";
    const GIB: u64 = 1 << 30;
    const VG_UUID: &str = "Vg0AAA-1111-2222-3333-4444-5555-Vg0AAA";
    const PV_UUID: &str = "Pv0AAA-1111-2222-3333-4444-5555-Pv0AAA";
    const ROOT_UUID: &str = "Root00-1111-2222-3333-4444-5555-Root00";
    const HOME_UUID: &str = "Home00-1111-2222-3333-4444-5555-Home00";

    /// The LVM report of the PC below, as agent 0.8.12 records it.
    fn lvm_report(root_attr: &str) -> String {
        format!(
            r#"{{"report":[{{"lv":[
                {{"vg_name":"ArchinstallVg","lv_name":"root","lv_uuid":"{ROOT_UUID}","lv_size":"49392123904B","lv_attr":"{root_attr}","pool_lv":"","origin":"","segtype":"linear","devices":"/dev/nvme0n1p2(0)"}},
                {{"vg_name":"ArchinstallVg","lv_name":"home","lv_uuid":"{HOME_UUID}","lv_size":"449360953344B","lv_attr":"-wi-ao----","pool_lv":"","origin":"","segtype":"linear","devices":"/dev/nvme0n1p2(11776)"}}],
              "vg":[{{"vg_name":"ArchinstallVg","vg_uuid":"{VG_UUID}","vg_extent_size":"4194304B","vg_size":"499021512704B","vg_free":"268435456B"}}],
              "pv":[{{"pv_name":"/dev/nvme0n1p2","pv_uuid":"{PV_UUID}","pv_size":"499031958528B","pe_start":"1048576B","vg_name":"ArchinstallVg"}}]}}]}}"#
        )
    }

    /// The PC of the real case: Arch, UEFI, systemd-boot with a UKI, mkinitcpio; nvme0n1 (a
    /// 500 GB disk) with a 1 GiB ESP at /boot and an LVM partition: ArchinstallVg with root
    /// (46 GiB ext4) and home (418.5 GiB ext4), 256 MiB free; a second disk (NTFS) outside the
    /// backup, listed first.
    pub fn arch_lvm() -> Manifest {
        let size = 500_107_862_016u64;
        let last = size / 512 - 34;
        let ntfs = Disk {
            name: "sda".into(),
            size_bytes: 1_000_204_886_016,
            logical_sector: 512,
            table: Some(PartTable {
                kind: "gpt".into(),
                disk_guid: "AAAAAAAA-0000-4000-8000-00000000000A".into(),
                partitions: vec![Partition { number: 1, start_lba: 2048, size_lba: 1_000_000_000, type_guid: "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7".into(), part_guid: "AAAAAAAA-0000-4000-8000-00000000000B".into(), ..Default::default() }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let nvme = Disk {
            name: "nvme0n1".into(),
            model: "Samsung".into(),
            size_bytes: size,
            logical_sector: 512,
            table: Some(PartTable {
                kind: "gpt".into(),
                disk_guid: "BBBBBBBB-0000-4000-8000-00000000000A".into(),
                partitions: vec![
                    Partition { number: 1, start_lba: 2048, size_lba: 2_097_152, type_guid: ESP.into(), part_guid: "BBBBBBBB-0000-4000-8000-000000000001".into(), ..Default::default() },
                    Partition { number: 2, start_lba: 2_099_200, size_lba: last - 2_099_200 + 1, type_guid: LVM_TYPE.into(), part_guid: "BBBBBBBB-0000-4000-8000-000000000002".into(), ..Default::default() },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        let lv = |name: &str, uuid: &str, path: &str, used: u64| Filesystem {
            device: format!("/dev/mapper/ArchinstallVg-{name}"),
            source: FsSource::Lvm { vg: "ArchinstallVg".into(), lv: name.into() },
            fstype: "ext4".into(),
            uuid: uuid.into(),
            mounts: vec![MountEntry { path: path.into(), ..Default::default() }],
            used_bytes: used,
            capture: Some(CaptureMethod::LvmSnapshot),
            ..Default::default()
        };
        Manifest {
            format: 1,
            hostname: "arch".into(),
            boot: BootInfo {
                uefi: true,
                loader: Loader::SystemdBoot,
                initramfs: InitramfsTool::Mkinitcpio,
                uki: true,
                esp: Some("/boot".into()),
                kernel_cmdline: Some("root=/dev/ArchinstallVg/root zswap.enabled=0 rw rootfstype=ext4".into()),
                ..Default::default()
            },
            disks: vec![ntfs, nvme],
            filesystems: vec![
                Filesystem {
                    device: "/dev/nvme0n1p1".into(),
                    source: FsSource::Partition { disk: 1, number: 1 },
                    fstype: "vfat".into(),
                    uuid: "8A4B-1C2D".into(),
                    mounts: vec![MountEntry { path: "/boot".into(), ..Default::default() }],
                    used_bytes: 120 << 20,
                    capture: Some(CaptureMethod::Files),
                    ..Default::default()
                },
                lv("root", "6f1d1b46-6c52-4cf0-9a8e-2c3d4e5f6a7b", "/", 20 * GIB),
                lv("home", "0e3c7a2b-1d4f-4e5a-8b9c-0d1e2f3a4b5c", "/home", 100 * GIB),
            ],
            lvm_report: Some(lvm_report("-wi-ao----")),
            fstab: "UUID=6f1d1b46-6c52-4cf0-9a8e-2c3d4e5f6a7b / ext4 rw,relatime 0 1\nUUID=0e3c7a2b-1d4f-4e5a-8b9c-0d1e2f3a4b5c /home ext4 rw,relatime 0 2\nUUID=8A4B-1C2D /boot vfat rw,fmask=0022,dmask=0022 0 2\n".into(),
            ..Default::default()
        }
    }

    fn nvme(size: u64) -> TargetDisk {
        TargetDisk { name: "nvme1n1".into(), size_bytes: size, logical_sector: 512, model: "new".into(), serial: "n".into() }
    }

    #[test]
    fn lvm_real_case_same_size() {
        let m = arch_lvm();
        assert_eq!(super::super::source_disk(&m), Some(1), "the disk of the root's volume group, not the first one");
        assert!(!m.ready_reasons().contains(&ReadyReason::ManualLayout));
        let p = build(&m, &[(1, nvme(500_107_862_016))], &|_| false).unwrap();
        assert_eq!(p.lvm.len(), 1);
        let vg = &p.lvm[0];
        assert_eq!((vg.name.as_str(), vg.uuid.as_deref(), vg.extent_bytes), ("ArchinstallVg", Some(VG_UUID), 4 << 20));
        assert_eq!(vg.pvs, [super::super::lvm::PvAction { device: "/dev/nvme1n1p2".into(), uuid: Some(PV_UUID.into()) }]);
        assert_eq!(vg.lvs.iter().map(|l| (l.name.as_str(), l.extents, l.uuid.as_deref())).collect::<Vec<_>>(), [("root", 11776, Some(ROOT_UUID)), ("home", 107136, Some(HOME_UUID))]);
        assert_eq!(vg.orig_free, 64);
        assert!(vg.free() >= 64, "{}", vg.free());
        assert!(vg.available >= 11776 + 107136 + 64);
        let devs: Vec<(&str, &str)> = p.filesystems.iter().map(|a| (a.device.as_str(), a.uuid.as_str())).collect();
        assert_eq!(devs, [("/dev/nvme1n1p1", "8A4B-1C2D"), ("/dev/ArchinstallVg/root", "6f1d1b46-6c52-4cf0-9a8e-2c3d4e5f6a7b"), ("/dev/ArchinstallVg/home", "0e3c7a2b-1d4f-4e5a-8b9c-0d1e2f3a4b5c")]);
        assert!(p.id_map.is_empty());
        let t = tools(&p);
        for x in ["pvcreate", "vgcreate", "lvcreate", "vgchange", "vgcfgbackup", "vgcfgrestore", "mkfs.ext4", "mkfs.vfat"] {
            assert!(t.iter().any(|y| y == x), "{x} in {t:?}");
        }
        assert_eq!(package_of("vgcfgrestore"), "lvm2");
        let text = describe(&m, &p, false).join("\n");
        assert!(text.contains(&format!("pvcreate --uuid {PV_UUID} --norestorefile /dev/nvme1n1p2")), "{text}");
        assert!(text.contains("vgcreate -s 4096k ArchinstallVg /dev/nvme1n1p2"), "{text}");
        assert!(text.contains("lvcreate -l 11776 -n root ArchinstallVg"), "{text}");
        assert!(text.contains("mkfs.ext4 -F -q -U 6f1d1b46-6c52-4cf0-9a8e-2c3d4e5f6a7b /dev/ArchinstallVg/root"), "{text}");
        assert!(text.contains("lvm2 hook"), "{text}");
        // the LVM step comes before the filesystems
        assert!(text.find("vgcreate").unwrap() < text.find("mkfs.ext4").unwrap());
        // the command line names the volume by its path: nothing to rewrite
        assert_eq!(fix_cmdline(m.boot.kernel_cmdline.as_deref().unwrap(), &m, &p.id_map), "root=/dev/ArchinstallVg/root zswap.enabled=0 rw rootfstype=ext4");
    }

    #[test]
    fn lvm_older_manifest_without_uuids() {
        let mut m = arch_lvm();
        m.lvm_report = Some(r#"{"report":[{"lv":[{"vg_name":"ArchinstallVg","lv_name":"root","lv_size":"49392123904B","lv_attr":"-wi-ao----","pool_lv":"","devices":"/dev/nvme0n1p2(0)"},{"vg_name":"ArchinstallVg","lv_name":"home","lv_size":"449360953344B","lv_attr":"-wi-ao----","pool_lv":"","devices":"/dev/nvme0n1p2(11776)"}]}]}"#.into());
        let p = build(&m, &[(1, nvme(500_107_862_016))], &|_| false).unwrap();
        let vg = &p.lvm[0];
        assert_eq!((vg.uuid.as_deref(), vg.extent_bytes, vg.pvs[0].uuid.as_deref()), (None, 4 << 20, None));
        assert_eq!(vg.lvs.iter().map(|l| l.extents).collect::<Vec<_>>(), [11776, 107136]);
        assert!(!vg.sets_ids());
        assert!(vg.orig_free > 0 && vg.free() == vg.orig_free, "free from the partition's size: {} {}", vg.orig_free, vg.free());
        assert!(p.notes.iter().any(|n| n.contains("no LVM UUIDs")), "{:?}", p.notes);
        assert!(!tools(&p).iter().any(|t| t == "vgcfgrestore"));
    }

    #[test]
    fn lvm_smaller_disk_shrinks_what_has_room() {
        let m = arch_lvm();
        let p = build(&m, &[(1, nvme(256_060_514_304))], &|_| false).unwrap();
        let vg = &p.lvm[0];
        let (root, home) = (&vg.lvs[0], &vg.lvs[1]);
        assert!(root.extents < root.orig_extents && home.extents < home.orig_extents, "{vg:?}");
        assert!(root.extents >= root.min_extents && home.extents >= home.min_extents);
        // the data plus room: 20 GiB + 2 GiB, 100 GiB + 10 GiB
        assert_eq!((root.min_extents, home.min_extents), (22 * 256, 110 * 256));
        assert_eq!(vg.free(), vg.orig_free.max(vg.free()));
        assert!(vg.free() >= 64 && vg.lvs.iter().map(|l| l.extents).sum::<u64>() + vg.free() == vg.available);
        assert!(p.notes.iter().any(|n| n.contains("ArchinstallVg/home") && n.contains("→")), "{:?}", p.notes);
        // in proportion: home keeps about nine times root's size
        assert!(home.extents / root.extents == 9, "{} {}", home.extents, root.extents);
        // a disk the data does not fit
        let e = build(&m, &[(1, nvme(128_035_676_160))], &|_| false).unwrap_err();
        assert!(e.contains("too small for volume group ArchinstallVg"), "{e}");
    }

    #[test]
    fn lvm_refusals() {
        let m = arch_lvm();
        let target = [(1, nvme(500_107_862_016))];
        // a thin volume, a snapshot: named
        let mut thin = m.clone();
        thin.lvm_report = Some(lvm_report("Vwi-aotz--"));
        let e = build(&thin, &target, &|_| false).unwrap_err();
        assert!(e.contains("linear LVM volumes only") && e.contains("ArchinstallVg/root: a thin volume"), "{e}");
        assert!(thin.ready_reasons().contains(&ReadyReason::ManualLayout));
        let mut striped = m.clone();
        striped.lvm_report = Some(lvm_report("-wi-ao----").replacen("\"linear\"", "\"striped\"", 1));
        assert!(build(&striped, &target, &|_| false).unwrap_err().contains("striped segments"));
        // md stays refused, and says LVM is fine
        let mut md = m.clone();
        md.md.push("MD_LEVEL=raid1\n".into());
        let e = build(&md, &target, &|_| false).unwrap_err();
        assert!(e.contains("md") && e.contains("LVM"), "{e}");
        // another attached disk has a volume group of the same name: refused (names matter for boot)
        let e = build(&m, &target, &|id| id == "vg:ArchinstallVg").unwrap_err();
        assert!(e.contains("another attached disk") && e.contains("vgrename"), "{e}");
        // a UUID in use: a new one, said so; the names stay
        let p = build(&m, &target, &|id| id == PV_UUID || id == ROOT_UUID).unwrap();
        assert_eq!((p.lvm[0].pvs[0].uuid.as_deref(), p.lvm[0].lvs[0].uuid.as_deref(), p.lvm[0].uuid.as_deref()), (None, None, Some(VG_UUID)));
        assert!(p.notes.iter().any(|n| n.contains(PV_UUID) && n.contains("in use")), "{:?}", p.notes);
        // a PV on a disk that gets no target
        let mut span = m.clone();
        span.lvm_report = Some(lvm_report("-wi-ao----").replace(r#""vg_name":"ArchinstallVg"}]}]}"#, r#""vg_name":"ArchinstallVg"},{"pv_name":"/dev/sda1","pv_uuid":"","pv_size":"1B","pe_start":"0B","vg_name":"ArchinstallVg"}]}]}"#));
        let e = build(&span, &target, &|_| false).unwrap_err();
        assert!(e.contains("no target") && e.contains("/dev/sda1"), "{e}");
        // a PV on a whole disk
        let mut whole = m.clone();
        whole.lvm_report = Some(lvm_report("-wi-ao----").replace("/dev/nvme0n1p2", "/dev/nvme0n1"));
        assert!(build(&whole, &target, &|_| false).unwrap_err().contains("whole disk"));
    }

    #[test]
    fn lvm_with_luks_below_and_inside() {
        let mut m = arch_lvm();
        // the PV in a LUKS container on p2, and home in LUKS inside its volume
        m.luks = vec![
            Luks { device: "/dev/nvme0n1p2".into(), source: FsSource::Partition { disk: 1, number: 2 }, uuid: "11111111-2222-4333-8444-555555555555".into(), version: 2, cipher: "aes-xts-plain64".into(), key_bits: 512, mapper: "cryptlvm".into(), ..Default::default() },
            Luks { device: "/dev/mapper/ArchinstallVg-home".into(), source: FsSource::Lvm { vg: "ArchinstallVg".into(), lv: "home".into() }, uuid: "66666666-2222-4333-8444-555555555555".into(), version: 2, mapper: "home".into(), ..Default::default() },
        ];
        m.lvm_report = Some(lvm_report("-wi-ao----").replace("\"/dev/nvme0n1p2", "\"/dev/mapper/cryptlvm"));
        m.filesystems[2].source = FsSource::Luks { luks: 1 };
        let p = build(&m, &[(1, nvme(500_107_862_016))], &|_| false).unwrap();
        assert_eq!(p.luks.iter().map(|l| (l.partition.as_str(), l.on_lvm)).collect::<Vec<_>>(), [("/dev/nvme1n1p2", false), ("/dev/ArchinstallVg/home", true)]);
        assert_eq!(p.lvm[0].pvs[0].device, "/dev/mapper/cryptlvm");
        assert_eq!(p.filesystems[2].device, "/dev/mapper/home");
        let text = describe(&m, &p, false).join("\n");
        let (a, b, c) = (text.find("luksFormat --type luks2 --uuid 1111").unwrap(), text.find("vgcreate").unwrap(), text.find("luksFormat --type luks2 --uuid 6666").unwrap());
        assert!(a < b && b < c, "{text}");
        assert_eq!(super::super::source_disk(&m), Some(1));
        // the LUKS header comes off what LVM gets
        assert!(p.lvm[0].available < build(&arch_lvm(), &[(1, nvme(500_107_862_016))], &|_| false).unwrap().lvm[0].available);
    }
}
