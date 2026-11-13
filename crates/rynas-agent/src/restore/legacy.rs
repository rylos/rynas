//! Root snapshots written before the layout manifest existed (`docs/research/pc-backup.md`
//! §11.5): an older restore script's hook kept, inside the root subvolume, copies of the boot
//! folder (`/.bootbackup/<time>_post/`) and a `sfdisk --dump` of the disk. Nothing here
//! knows those files' names: the NAS sends every small text file of `/.bootbackup/` that is
//! a GPT dump, and every folder ending in `_post` ([`RestoreLegacy`]).
//!
//! A manifest is put together from what there is, always with the "legacy" warning:
//! - the GPT from the newest dump (same label-id, PARTUUIDs, types, names);
//! - without a dump, the guided layout: one disk, an ESP of a chosen size, btrfs on the rest;
//! - the filesystems' UUIDs, mount points and subvolumes from the snapshot's `/etc/fstab`
//!   (so fstab and the loader entries keep working), the subvolume names from the snapshots;
//! - the ESP's content from the newest `_post` copy, laid down after the receive;
//! - boot loader and initramfs tool unknown: the boot step finds them in the restored system.

use rynas_api_types::pcagent::*;

/// GPT type GUIDs.
pub const ESP_TYPE: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
const SWAP_TYPE: &str = "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F";
const LINUX_TYPE: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";
/// The disk name of a guided layout (no dump).
pub const GUIDED: &str = "guided";

/// One active fstab line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FstabLine {
    pub spec: String,
    pub path: String,
    pub fstype: String,
    pub options: String,
}

impl FstabLine {
    pub fn subvol(&self) -> Option<String> {
        self.options.split(',').find_map(|o| o.strip_prefix("subvol=")).map(|s| s.trim_start_matches('/').to_string())
    }
    /// Mounts the top level (`subvolid=5`, `subvol=/`).
    pub fn top_level(&self) -> bool {
        self.options.split(',').any(|o| o == "subvolid=5" || o == "subvol=/" || o == "subvol=")
    }
    fn id(&self, kind: &str) -> Option<String> {
        self.spec.strip_prefix(kind).map(|s| s.trim_matches('"').to_string())
    }
}

pub fn parse_fstab(text: &str) -> Vec<FstabLine> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 3).then(|| FstabLine {
                spec: f[0].to_string(),
                path: f[1].to_string(),
                fstype: f[2].to_string(),
                options: f.get(3).copied().unwrap_or("defaults").to_string(),
            })
        })
        .collect()
}

/// `GUID:60,62` / `RequiredPartition LegacyBIOSBootable` → the attribute bits.
fn attrs_bits(s: &str) -> u64 {
    let mut bits = 0u64;
    for w in s.split([' ', ',']).filter(|w| !w.is_empty()) {
        match w {
            "RequiredPartition" => bits |= 1,
            "NoBlockIOProtocol" => bits |= 1 << 1,
            "LegacyBIOSBootable" => bits |= 1 << 2,
            _ => {
                let n = w.strip_prefix("GUID:").unwrap_or(w);
                for x in n.split(',') {
                    if let Ok(b) = x.parse::<u32>()
                        && b < 64
                    {
                        bits |= 1 << b;
                    }
                }
            }
        }
    }
    bits
}

/// The disk of a `sfdisk --dump` of a GPT disk.
pub fn parse_sfdisk(text: &str) -> Option<Disk> {
    let mut label = String::new();
    let mut id = String::new();
    let mut device = String::new();
    let mut sector = 512u32;
    let (mut first, mut last) = (0u64, 0u64);
    let mut parts = Vec::new();
    for l in text.lines().map(str::trim) {
        if let Some((dev, rest)) = l.split_once(" : ").or_else(|| l.split_once(": ").filter(|(d, _)| d.starts_with('/'))) {
            if !dev.trim().starts_with('/') {
                continue;
            }
            let dev = dev.trim();
            let digits: String = dev.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
            let number: u32 = digits.parse().ok()?;
            let mut p = Partition { number, dev: dev.rsplit('/').next().unwrap_or(dev).to_string(), ..Default::default() };
            // key=value pairs; a quoted name may hold commas
            let mut rest = rest.trim();
            while !rest.is_empty() {
                let (kv, tail) = if let Some(eq) = rest.find('=') {
                    let after = &rest[eq + 1..];
                    if let Some(q) = after.strip_prefix('"') {
                        let end = q.find('"').unwrap_or(q.len());
                        let used = eq + 1 + 1 + end + 1;
                        (&rest[..used.min(rest.len())], rest[used.min(rest.len())..].trim_start_matches([',', ' ']))
                    } else {
                        match rest.find(',') {
                            Some(c) => (&rest[..c], rest[c + 1..].trim_start()),
                            None => (rest, ""),
                        }
                    }
                } else {
                    (rest, "")
                };
                if let Some((k, v)) = kv.split_once('=') {
                    let v = v.trim().trim_matches('"');
                    match k.trim() {
                        "start" => p.start_lba = v.parse().ok()?,
                        "size" => p.size_lba = v.parse().ok()?,
                        "type" => p.type_guid = v.to_uppercase(),
                        "uuid" => p.part_guid = v.to_uppercase(),
                        "name" => p.name = v.to_string(),
                        "attrs" => p.attrs = attrs_bits(v),
                        _ => {}
                    }
                }
                rest = tail;
            }
            if p.size_lba > 0 && !p.type_guid.is_empty() {
                parts.push(p);
            }
            continue;
        }
        if let Some((k, v)) = l.split_once(':') {
            let v = v.trim();
            match k.trim() {
                "label" => label = v.to_string(),
                "label-id" => id = v.to_uppercase(),
                "device" => device = v.to_string(),
                "sector-size" => sector = v.parse().unwrap_or(512),
                "first-lba" => first = v.parse().unwrap_or(0),
                "last-lba" => last = v.parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    if label != "gpt" || parts.is_empty() {
        return None;
    }
    let end = parts.iter().map(|p| p.start_lba + p.size_lba).max().unwrap_or(0);
    // the backup GPT takes 33 sectors after the last usable one
    let size_bytes = (last.max(end) + 34) * u64::from(sector);
    Some(Disk {
        name: device.rsplit('/').next().unwrap_or("disk").to_string(),
        size_bytes,
        logical_sector: sector,
        table: Some(PartTable { kind: "gpt".into(), disk_guid: id, first_lba: first, last_lba: last, partitions: parts }),
        ..Default::default()
    })
}

/// The guided layout's disk: one ESP of `esp_mib` and btrfs on the rest (it grows to the
/// target's end), with the PARTUUIDs fstab names when it names them.
pub fn guided_disk(esp_mib: u64, sector: u32, esp_guid: &str, root_guid: &str) -> Disk {
    let s = u64::from(sector.max(512));
    let start = (1 << 20) / s;
    let esp = (esp_mib.max(64) << 20) / s;
    Disk {
        name: GUIDED.into(),
        size_bytes: 0,
        logical_sector: sector,
        table: Some(PartTable {
            kind: "gpt".into(),
            disk_guid: String::new(),
            first_lba: 34,
            last_lba: 0,
            partitions: vec![
                Partition { number: 1, start_lba: start, size_lba: esp, type_guid: ESP_TYPE.into(), part_guid: esp_guid.to_uppercase(), name: "EFI system partition".into(), ..Default::default() },
                Partition { number: 2, start_lba: start + esp, size_lba: 1, type_guid: LINUX_TYPE.into(), part_guid: root_guid.to_uppercase(), name: "linux".into(), ..Default::default() },
            ],
        }),
        ..Default::default()
    }
}

/// What the legacy snapshot gave, for the plan and the boot step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Legacy {
    pub manifest: Manifest,
    pub notes: Vec<String>,
    /// The ESP's copy inside the restored root (`/.bootbackup/<newest>_post`), and where it goes.
    pub boot: Option<(String, String)>,
    /// No dump: the layout is the guided one ([`refit_guided`] before planning).
    pub guided: bool,
    pub esp_mib: u64,
}

/// The layout of a PC without manifest. `groups`: the subvolume names the NAS has snapshots
/// of; `referenced`: the bytes of the chosen snapshots (0: unknown).
pub fn synthesize(device: &str, l: &RestoreLegacy, groups: &[String], referenced: u64, esp_mib: u64) -> Result<Legacy, String> {
    let fstab_text = l.fstab.clone().ok_or_else(|| format!("{}: no /etc/fstab, no manifest: not a root snapshot this restore can read", l.snapshot))?;
    let fstab = parse_fstab(&fstab_text);
    let root = fstab.iter().find(|f| f.path == "/" && f.fstype == "btrfs").ok_or("the snapshot's /etc/fstab mounts no btrfs filesystem at /")?;
    let esp_line = fstab.iter().find(|f| f.fstype == "vfat" && f.path.starts_with('/'));
    let mut notes = vec![format!("legacy snapshot {} without a layout manifest: the layout comes from its own files (check the plan)", l.snapshot)];
    // the disk
    let dump = l.partition_dumps.iter().rev().find_map(|f| parse_sfdisk(&f.text).map(|d| (f.name.clone(), d)));
    let guided = dump.is_none();
    let mut disk = match dump {
        Some((name, d)) => {
            notes.push(format!("partition table from the legacy dump .bootbackup/{name}"));
            d
        }
        None => {
            notes.push(format!("no partition table dump: guided layout, one disk with a {esp_mib} MiB ESP and btrfs on the rest"));
            guided_disk(esp_mib, 512, &esp_line.and_then(|e| e.id("PARTUUID=")).unwrap_or_default(), &root.id("PARTUUID=").unwrap_or_default())
        }
    };
    let table = disk.table.as_mut().ok_or("no partition table")?;
    let by_guid = |t: &PartTable, g: &str| t.partitions.iter().find(|p| !g.is_empty() && p.part_guid.eq_ignore_ascii_case(g)).map(|p| p.number);
    let esp_part = esp_line
        .and_then(|e| e.id("PARTUUID=").and_then(|g| by_guid(table, &g)))
        .or_else(|| table.partitions.iter().find(|p| p.type_guid.eq_ignore_ascii_case(ESP_TYPE)).map(|p| p.number));
    let root_part = root.id("PARTUUID=").and_then(|g| by_guid(table, &g)).or_else(|| {
        table
            .partitions
            .iter()
            .filter(|p| !p.type_guid.eq_ignore_ascii_case(ESP_TYPE) && !p.type_guid.eq_ignore_ascii_case(SWAP_TYPE) && Some(p.number) != esp_part)
            .max_by_key(|p| p.size_lba)
            .map(|p| p.number)
    });
    let root_part = root_part.ok_or("no partition for the btrfs filesystem")?;
    let swap_part = table.partitions.iter().find(|p| p.type_guid.eq_ignore_ascii_case(SWAP_TYPE)).map(|p| p.number);
    // the btrfs filesystem: every fstab line of the same filesystem
    let same_fs = |f: &FstabLine| f.fstype == "btrfs" && f.spec == root.spec;
    let mut mounts = Vec::new();
    let mut subvolumes: Vec<Subvolume> = Vec::new();
    for f in fstab.iter().filter(|f| same_fs(f)) {
        let sub = f.subvol().filter(|s| !s.is_empty());
        if let Some(s) = &sub
            && !subvolumes.iter().any(|x| &x.path == s)
        {
            let base = s.rsplit('/').next().unwrap_or(s);
            subvolumes.push(Subvolume { path: s.clone(), captured: groups.iter().any(|g| g == base || g == s), ..Default::default() });
        }
        mounts.push(MountEntry { path: f.path.clone(), subvol: sub, options: f.options.clone() });
    }
    // the btrbk names: each subvolume the NAS has, by fstab's subvolume of that name
    let mut layout = Vec::new();
    for g in groups {
        let line = fstab.iter().filter(|f| same_fs(f)).find(|f| f.subvol().is_some_and(|s| s == *g || s.rsplit('/').next() == Some(g.as_str())));
        let path = line.and_then(|f| f.subvol()).unwrap_or_else(|| g.clone());
        if !subvolumes.iter().any(|x| x.path == path) {
            subvolumes.push(Subvolume { path: path.clone(), captured: true, ..Default::default() });
        }
        layout.push(BtrbkSubvolume { name: g.clone(), path, mount: line.map(|f| f.path.clone()).unwrap_or_default() });
    }
    let top = fstab.iter().find(|f| same_fs(f) && f.top_level()).map(|f| format!("{}/btrbk_snapshots", f.path.trim_end_matches('/')));
    let mut filesystems = vec![Filesystem {
        device: format!("/dev/{}", super::plan::part_name(&disk.name, root_part)),
        source: FsSource::Partition { disk: 0, number: root_part },
        fstype: "btrfs".into(),
        uuid: root.id("UUID=").unwrap_or_default(),
        label: root.id("LABEL=").unwrap_or_default(),
        mounts,
        used_bytes: referenced,
        capture: Some(CaptureMethod::BtrfsSnapshot),
        subvolumes,
        ..Default::default()
    }];
    let esp_mount = esp_line.map(|e| e.path.clone());
    if let Some(n) = esp_part {
        filesystems.push(Filesystem {
            device: format!("/dev/{}", super::plan::part_name(&disk.name, n)),
            source: FsSource::Partition { disk: 0, number: n },
            fstype: "vfat".into(),
            uuid: esp_line.and_then(|e| e.id("UUID=")).unwrap_or_default(),
            label: esp_line.and_then(|e| e.id("LABEL=")).unwrap_or_default(),
            mounts: vec![MountEntry { path: esp_mount.clone().unwrap_or_else(|| "/boot".into()), ..Default::default() }],
            used_bytes: 64 << 20,
            capture: Some(CaptureMethod::Files),
            ..Default::default()
        });
    }
    let mut swap = Vec::new();
    if let (Some(n), Some(line)) = (swap_part, fstab.iter().find(|f| f.fstype == "swap" && !f.spec.starts_with('/'))) {
        swap.push(Swap { path: line.spec.clone(), uuid: line.id("UUID=").unwrap_or_default(), source: FsSource::Partition { disk: 0, number: n }, ..Default::default() });
    }
    let os_release: std::collections::BTreeMap<String, String> = l
        .os_release
        .as_deref()
        .unwrap_or("")
        .lines()
        .filter_map(|x| x.split_once('='))
        .filter(|(k, _)| matches!(*k, "ID" | "VERSION_ID" | "PRETTY_NAME" | "ID_LIKE"))
        .map(|(k, v)| (k.to_string(), v.trim_matches('"').to_string()))
        .collect();
    let boot = l.boot_backups.iter().max().map(|b| (format!("/{LEGACY_BOOT_DIR}/{b}"), esp_mount.clone().unwrap_or_else(|| "/boot".into())));
    match &boot {
        Some((dir, at)) => notes.push(format!("boot files from the legacy copy {dir} onto {at}")),
        None => notes.push("no boot folder copy in the snapshot: the boot step regenerates the boot files".into()),
    }
    let manifest = Manifest {
        format: MANIFEST_FORMAT,
        hostname: device.to_string(),
        os_release,
        boot: BootInfo { uefi: esp_part.is_some(), esp: esp_mount, ..Default::default() },
        disks: vec![disk],
        filesystems,
        swap,
        fstab: fstab_text,
        btrbk: Some(BtrbkLayout { snapshot_dir: top.unwrap_or_default(), subvolumes: layout, boot_copies: Vec::new(), agent: false }),
        ..Default::default()
    };
    Ok(Legacy { manifest, notes, boot, guided, esp_mib })
}

/// The guided disk again for a target's sector size (the LBAs depend on it).
pub fn refit_guided(m: &mut Manifest, sector: u32, esp_mib: u64) {
    let Some(d) = m.disks.first_mut().filter(|d| d.name == GUIDED) else { return };
    let guid = |n: u32| d.table.as_ref().and_then(|t| t.partitions.iter().find(|p| p.number == n)).map(|p| p.part_guid.clone()).unwrap_or_default();
    let (esp, root) = (guid(1), guid(2));
    *d = guided_disk(esp_mib, sector, &esp, &root);
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = "label: gpt\nlabel-id: 1E2B8F6D-3643-4D8D-AAC5-AB7EDEBECACC\ndevice: /dev/nvme0n1\nunit: sectors\nfirst-lba: 34\nlast-lba: 1953525134\nsector-size: 512\n\n/dev/nvme0n1p1 : start=        2048, size=     2097152, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, uuid=0397CC02-0000-4000-8000-000000000002, name=\"EFI system, boot\"\n/dev/nvme0n1p2 : start=     2099200, size=  1951425935, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, uuid=3612A9BF-0000-4000-8000-000000000003, name=\"root\", attrs=\"GUID:59\"\n";
    const FSTAB: &str = "# static\nUUID=91BC-A784 /boot vfat rw,relatime 0 2\nUUID=2d5cddec-8777-4aef-9b8d-1d7093e60982 / btrfs rw,noatime,subvol=/@ 0 0\nUUID=2d5cddec-8777-4aef-9b8d-1d7093e60982 /home btrfs rw,noatime,subvol=/@home 0 0\nUUID=2d5cddec-8777-4aef-9b8d-1d7093e60982 /home/me/games btrfs rw,noatime,subvol=@games 0 0\nUUID=2d5cddec-8777-4aef-9b8d-1d7093e60982 /var/log btrfs rw,noatime,subvol=/@log 0 0\nUUID=2d5cddec-8777-4aef-9b8d-1d7093e60982 /mnt/pool btrfs rw,noatime,subvolid=5 0 0\n";

    fn legacy(dump: bool) -> RestoreLegacy {
        RestoreLegacy {
            snapshot: "@.20261101T0300".into(),
            fstab: Some(FSTAB.into()),
            os_release: Some("NAME=\"Arch Linux\"\nID=arch\n".into()),
            partition_dumps: if dump { vec![LegacyFile { name: "any_name_at_all".into(), text: DUMP.into() }] } else { Vec::new() },
            boot_backups: vec!["20261030_post".into(), "20261031_post".into()],
        }
    }

    #[test]
    fn sfdisk_dumps_parse() {
        let d = parse_sfdisk(DUMP).unwrap();
        let t = d.table.unwrap();
        assert_eq!(t.disk_guid, "1E2B8F6D-3643-4D8D-AAC5-AB7EDEBECACC");
        assert_eq!(t.partitions.len(), 2);
        assert_eq!(t.partitions[0].name, "EFI system, boot");
        assert_eq!((t.partitions[1].number, t.partitions[1].start_lba, t.partitions[1].size_lba), (2, 2099200, 1951425935));
        assert_eq!(t.partitions[1].attrs, 1 << 59);
        assert!(parse_sfdisk("label: dos\n/dev/sda1 : start=1, size=2, type=83\n").is_none());
    }

    #[test]
    fn legacy_layout_from_the_dump_and_fstab() {
        let groups = vec!["@".to_string(), "@home".into(), "@games".into()];
        let l = synthesize("pc-casa", &legacy(true), &groups, 40 << 30, 1024).unwrap();
        assert!(!l.guided);
        let m = &l.manifest;
        let root = m.root().unwrap();
        assert_eq!(root.uuid, "2d5cddec-8777-4aef-9b8d-1d7093e60982");
        assert_eq!(root.source, FsSource::Partition { disk: 0, number: 2 });
        assert_eq!(root.used_bytes, 40 << 30);
        assert!(root.subvolumes.iter().any(|s| s.path == "@log" && !s.captured), "@log is recreated empty");
        let b = m.btrbk.as_ref().unwrap();
        assert_eq!(b.snapshot_dir, "/mnt/pool/btrbk_snapshots");
        let games = b.subvolumes.iter().find(|s| s.name == "@games").unwrap();
        assert_eq!((games.path.as_str(), games.mount.as_str()), ("@games", "/home/me/games"));
        assert_eq!(m.boot.esp.as_deref(), Some("/boot"));
        assert_eq!(l.boot, Some(("/.bootbackup/20261031_post".into(), "/boot".into())));
        let esp = m.filesystems.iter().find(|f| f.fstype == "vfat").unwrap();
        assert_eq!((esp.uuid.as_str(), &esp.source), ("91BC-A784", &FsSource::Partition { disk: 0, number: 1 }));
        assert!(l.notes[0].contains("legacy"));
        assert_eq!(m.os_release.get("ID").map(String::as_str), Some("arch"));
    }

    #[test]
    fn guided_layout_without_a_dump() {
        let mut l = synthesize("pc", &legacy(false), &["@".to_string()], 0, 512).unwrap();
        assert!(l.guided);
        assert!(l.notes.iter().any(|n| n.contains("guided")));
        refit_guided(&mut l.manifest, 4096, 512);
        let t = l.manifest.disks[0].table.as_ref().unwrap();
        assert_eq!(l.manifest.disks[0].logical_sector, 4096);
        assert_eq!(t.partitions[0].start_lba, 256);
        assert_eq!(t.partitions[0].size_lba, (512 << 20) / 4096);
        assert_eq!(t.partitions[1].start_lba, 256 + (512 << 20) / 4096);
        // no fstab: nothing to restore from
        let mut bad = legacy(false);
        bad.fstab = None;
        assert!(synthesize("pc", &bad, &[], 0, 512).is_err());
    }
}
