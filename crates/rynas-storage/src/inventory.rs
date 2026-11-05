//! Pure assembly of the API views from parsed pieces: disk summaries, btrfs filesystems and
//! the per-volume tree.

use std::collections::{HashMap, HashSet};

use rynas_api_types::storage::{
    ArrayHealth, BtrfsDevice, BtrfsDeviceErrors, BtrfsFilesystem, DiskKind, DiskSummary,
    HealthVerdict, NodeKind, Partition, PowerState, QuotaMode, ScrubStatus, StorageTree, Subvolume,
    TreeNode, VolumeTree,
};

use crate::Inventory;
use crate::btrfs::{ShowFs, Usage, compression, total_errors};
use crate::lsblk::{BlockDev, BlockGraph};
use crate::mounts::Mount;
use crate::smart::{SmartData, known_smr, verdict};

pub fn disk_kind(d: &BlockDev) -> DiskKind {
    if d.kname.starts_with("nvme") || d.transport.as_deref() == Some("nvme") {
        DiskKind::Nvme
    } else if d.rotational {
        DiskKind::Hdd
    } else {
        DiskKind::Ssd
    }
}

/// What a disk is used for.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiskUsage {
    pub used_by: Vec<String>,
    /// Something on it is live: mounted, an assembled array, an open LUKS mapping, swap.
    pub in_use: bool,
    /// The root filesystem lives on it.
    pub system: bool,
}

pub fn disk_usage(g: &BlockGraph, disk: &str) -> DiskUsage {
    let mut u = DiskUsage::default();
    let Some(d) = g.get(disk) else { return u };
    let mut push = |s: String| {
        if !u.used_by.contains(&s) {
            u.used_by.push(s);
        }
    };
    let mut devs: Vec<&BlockDev> = vec![d];
    devs.extend(g.above(disk));
    let mut live = false;
    let mut system = false;
    for x in &devs {
        if x.is_md() && !x.is_disk() {
            push(format!("{} ({})", x.kname, x.kind));
            live = true;
        } else if x.kind == "crypt" {
            push(format!("LUKS {} (unlocked)", x.name));
            live = true;
        } else if x.fstype.as_deref() == Some("crypto_LUKS")
            && !x
                .children
                .iter()
                .any(|c| g.get(c).is_some_and(|c| c.kind == "crypt"))
        {
            push("LUKS (locked)".to_string());
        } else if x.kind == "lvm" {
            push(format!("LVM {}", x.name));
            live = true;
        }
        for mp in &x.mountpoints {
            if mp == "[SWAP]" {
                push("swap".into());
            } else {
                push(format!("mounted {mp}"));
            }
            live = true;
            if mp == "/" || mp == "/usr" || mp == "/boot" || mp == "/boot/efi" {
                system = true;
            }
        }
        if let Some(fs) = &x.fstype
            && x.mountpoints.is_empty()
            && !matches!(fs.as_str(), "crypto_LUKS" | "linux_raid_member")
        {
            push(match &x.label {
                Some(l) => format!("{fs} \"{l}\""),
                None => fs.clone(),
            });
        }
        if x.fstype.as_deref() == Some("linux_raid_member") && x.children.is_empty() {
            push(format!(
                "md member of {} (not assembled)",
                x.label.as_deref().unwrap_or("an array")
            ));
        }
    }
    if u.used_by.is_empty() && !d.children.is_empty() {
        u.used_by.push(format!("{} partition(s)", d.children.len()));
    }
    u.in_use = live;
    u.system = system;
    u
}

pub fn partitions(g: &BlockGraph, disk: &str) -> Vec<Partition> {
    let Some(d) = g.get(disk) else {
        return Vec::new();
    };
    d.children
        .iter()
        .filter_map(|c| g.get(c))
        .filter(|c| c.kind == "part")
        .map(|c| Partition {
            name: c.kname.clone(),
            size: c.size,
            fstype: c.fstype.clone(),
            label: c.label.clone().or_else(|| c.partlabel.clone()),
            uuid: c.uuid.clone(),
            mountpoints: c.mountpoints.clone(),
            holders: c
                .children
                .iter()
                .filter_map(|h| g.get(h))
                .map(|h| h.name.clone())
                .collect(),
        })
        .collect()
}

/// SMART as cached by the service, with its time.
#[derive(Debug, Clone, PartialEq)]
pub struct SmartEntry {
    pub data: SmartData,
    pub serial: String,
}

pub fn summary(
    d: &BlockDev,
    g: &BlockGraph,
    smart: Option<&SmartEntry>,
    power: PowerState,
    now_ms: u64,
    zoned: bool,
) -> DiskSummary {
    let kind = disk_kind(d);
    let model = d.model.clone().unwrap_or_default();
    let usage = disk_usage(g, &d.kname);
    let mut s = DiskSummary {
        name: d.kname.clone(),
        path: d.path.clone(),
        serial: d.serial.clone().unwrap_or_default(),
        wwn: d.wwn.clone(),
        firmware: d.firmware.clone(),
        size: d.size,
        kind,
        rotational: d.rotational,
        transport: d.transport.clone().unwrap_or_else(|| "unknown".into()),
        bay: None,
        power_state: power,
        smr: zoned || known_smr(&model),
        removable: d.removable || d.hotplug,
        used_by: usage.used_by,
        in_use: usage.in_use,
        system: usage.system,
        model,
        ..Default::default()
    };
    if let Some(e) = smart {
        let i = &e.data.info;
        let (v, reasons) = verdict(i, kind);
        s.health = v;
        s.health_reasons = reasons;
        s.temperature_c = i.temperature_c;
        s.power_on_hours = i.power_on_hours;
        s.rotation_rpm = e.data.rotation_rpm;
        s.smart_age_s = Some(now_ms.saturating_sub(i.read_ms) / 1000);
        if s.firmware.is_none() {
            s.firmware = e.data.firmware.clone();
        }
    }
    s
}

/// Pieces gathered for one btrfs filesystem.
#[derive(Debug, Clone, Default)]
pub struct FsParts<'a> {
    pub show: Option<&'a ShowFs>,
    pub usage: Option<&'a Usage>,
    pub stats: Option<&'a HashMap<String, BtrfsDeviceErrors>>,
    pub subvolumes: Vec<Subvolume>,
    pub readonly: HashSet<u64>,
    pub scrub: Option<ScrubStatus>,
    pub quota: QuotaMode,
}

/// Mounts of one filesystem, the main one first: the top level (subvolid 5), else the
/// shortest path.
pub fn fs_mounts<'a>(mounts: &'a [Mount], sources: &[String]) -> Vec<&'a Mount> {
    let mut v: Vec<&Mount> = mounts
        .iter()
        .filter(|m| m.fstype == "btrfs" && sources.iter().any(|s| s == &m.source))
        .collect();
    v.sort_by_key(|m| (m.subvolid() != Some(5), m.mountpoint.len()));
    v
}

pub fn build_filesystem(
    uuid: &str,
    label: Option<String>,
    sources: &[String],
    mounts: &[Mount],
    parts: FsParts<'_>,
    now_ms: u64,
) -> BtrfsFilesystem {
    let ms = fs_mounts(mounts, sources);
    let main = ms.first().copied();
    let options = main.map(|m| m.options.clone()).unwrap_or_default();
    let mut f = BtrfsFilesystem {
        uuid: uuid.to_string(),
        label: label.or_else(|| parts.show.and_then(|s| s.label.clone())),
        mountpoint: main.map(|m| m.mountpoint.clone()),
        mountpoints: ms.iter().map(|m| m.mountpoint.clone()).collect(),
        compression: compression(&options),
        mount_options: options,
        quota: parts.quota,
        scrub: parts.scrub,
        read_ms: now_ms,
        ..Default::default()
    };
    if let Some(sh) = parts.show {
        for d in &sh.devices {
            f.devices.push(BtrfsDevice {
                devid: d.devid,
                path: d.path.clone(),
                size: d.size,
                used: d.used,
                missing: d.missing,
                errors: parts
                    .stats
                    .and_then(|s| s.get(&d.path))
                    .cloned()
                    .unwrap_or_default(),
            });
        }
        f.used = sh.used;
        f.device_size = sh.devices.iter().map(|d| d.size).sum();
    }
    if let Some(u) = parts.usage {
        f.device_size = u.device_size;
        f.used = u.used;
        f.free_estimated = u.free_estimated;
        f.free_min = u.free_min;
        f.data_ratio = u.data_ratio;
        f.metadata_ratio = u.metadata_ratio;
        f.profiles = u.profiles.clone();
    }
    // subvolume → mountpoint (subvolid=N mounts)
    let by_id: HashMap<u64, &str> = ms
        .iter()
        .filter_map(|m| Some((m.subvolid()?, m.mountpoint.as_str())))
        .collect();
    f.subvolumes = parts
        .subvolumes
        .into_iter()
        .map(|mut s| {
            s.readonly = parts.readonly.contains(&s.id);
            s.mountpoint = by_id.get(&s.id).map(|m| m.to_string());
            s
        })
        .collect();
    f
}

fn node_kind(d: &BlockDev) -> NodeKind {
    match d.kind.as_str() {
        "disk" => NodeKind::Disk,
        "part" => NodeKind::Partition,
        "crypt" => NodeKind::Crypt,
        "lvm" => NodeKind::Lvm,
        _ if d.is_md() => NodeKind::Md,
        _ => NodeKind::Other,
    }
}

fn array_verdict(h: ArrayHealth) -> HealthVerdict {
    match h {
        ArrayHealth::Healthy => HealthVerdict::Healthy,
        ArrayHealth::Degraded => HealthVerdict::Warning,
        ArrayHealth::Failed | ArrayHealth::Inactive => HealthVerdict::Failing,
        ArrayHealth::Unknown => HealthVerdict::Unknown,
    }
}

fn worst(a: HealthVerdict, b: HealthVerdict) -> HealthVerdict {
    a.max(b)
}

/// The node for a block device and everything below it.
fn layer(inv: &Inventory, kname: &str, depth: usize) -> Option<TreeNode> {
    let d = inv.graph.get(kname)?;
    let mut n = TreeNode {
        kind: node_kind(d),
        name: d.name.clone(),
        size: d.size,
        ..Default::default()
    };
    match n.kind {
        NodeKind::Disk => {
            if let Some(s) = inv.disks.iter().find(|s| s.name == d.kname) {
                n.health = s.health;
                n.state = Some(format!("{:?}", s.power_state).to_lowercase());
                n.detail = Some(s.model.clone());
            }
        }
        NodeKind::Md => {
            if let Some(a) = inv.arrays.iter().find(|a| a.name == d.kname) {
                n.health = array_verdict(a.health);
                n.state = Some(a.state.clone());
                let active = a.members.len();
                n.detail = Some(format!(
                    "{}, {} disk{}{}",
                    a.level,
                    active,
                    if active == 1 { "" } else { "s" },
                    a.sync
                        .as_ref()
                        .and_then(|s| s.pct.map(|p| format!(", {} {p:.1}%", s.action)))
                        .unwrap_or_default()
                ));
            }
        }
        NodeKind::Crypt => {
            n.state = Some("unlocked".into());
            n.detail = Some("LUKS".into());
            n.health = HealthVerdict::Healthy;
        }
        NodeKind::Partition => {
            n.detail = d.partlabel.clone().or_else(|| d.fstype.clone());
            n.health = HealthVerdict::Healthy;
        }
        _ => {}
    }
    if depth < 8 {
        for p in &d.parents {
            if let Some(c) = layer(inv, p, depth + 1) {
                n.health = worst(n.health, c.health);
                n.children.push(c);
            }
        }
    }
    Some(n)
}

pub fn tree(inv: &Inventory) -> StorageTree {
    let mut t = StorageTree::default();
    let mut covered: HashSet<String> = HashSet::new();
    for f in &inv.filesystems {
        let mut v = VolumeTree {
            id: f.uuid.clone(),
            label: f.label.clone(),
            fstype: "btrfs".into(),
            mountpoint: f.mountpoint.clone(),
            size: f.device_size,
            used: f.used,
            health: HealthVerdict::Healthy,
            ..Default::default()
        };
        for dev in &f.devices {
            if total_errors(&dev.errors) > 0 || dev.missing {
                v.health = worst(v.health, HealthVerdict::Warning);
            }
            if let Some(b) = inv.graph.find(&dev.path) {
                covered.insert(b.kname.clone());
                if let Some(n) = layer(inv, &b.kname, 0) {
                    v.health = worst(v.health, n.health);
                    v.layers.push(n);
                }
            }
        }
        v.encrypted = v.layers.iter().any(|l| l.kind == NodeKind::Crypt);
        v.subvolumes = f
            .subvolumes
            .iter()
            .map(|s| TreeNode {
                kind: NodeKind::Subvolume,
                name: s.path.clone(),
                state: s.readonly.then(|| "read-only".to_string()),
                detail: s.mountpoint.clone(),
                health: HealthVerdict::Healthy,
                ..Default::default()
            })
            .collect();
        t.volumes.push(v);
    }
    // other filesystems (ext4/xfs volumes, unmounted btrfs)
    for d in inv.graph.devs.values() {
        let Some(fs) = d.fstype.as_deref() else {
            continue;
        };
        if covered.contains(&d.kname) || !matches!(fs, "btrfs" | "ext4" | "xfs") {
            continue;
        }
        if inv
            .filesystems
            .iter()
            .any(|f| d.uuid.as_deref() == Some(f.uuid.as_str()))
        {
            continue;
        }
        let mut v = VolumeTree {
            id: d.uuid.clone().unwrap_or_else(|| d.kname.clone()),
            label: d.label.clone(),
            fstype: fs.to_string(),
            mountpoint: d.mountpoints.first().cloned(),
            size: d.size,
            health: HealthVerdict::Healthy,
            ..Default::default()
        };
        if let Some(n) = layer(inv, &d.kname, 0) {
            v.health = worst(v.health, n.health);
            v.layers.push(n);
        }
        v.encrypted = v.layers.iter().any(|l| l.kind == NodeKind::Crypt);
        t.volumes.push(v);
    }
    for c in crate::luks::containers(&inv.graph) {
        if c.mapping.is_none()
            && let Some(mut n) = layer(inv, &c.device, 0)
        {
            n.state = Some("locked".into());
            n.detail = Some(format!("LUKS{}", c.version.unwrap_or_default()));
            t.locked.push(TreeNode {
                kind: NodeKind::Crypt,
                name: c.uuid.unwrap_or(c.device),
                size: n.size,
                state: Some("locked".into()),
                detail: n.detail.clone(),
                health: n.health,
                children: vec![n],
            });
        }
    }
    t.unused_disks = inv
        .disks
        .iter()
        .filter(|d| d.used_by.is_empty())
        .map(|d| d.name.clone())
        .collect();
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::nas_inventory;

    #[test]
    fn usage_of_disks() {
        let inv = nas_inventory();
        let g = &inv.graph;
        let sda = disk_usage(g, "sda");
        assert!(sda.in_use && !sda.system);
        assert!(
            sda.used_by.contains(&"md2 (raid5)".to_string()),
            "{:?}",
            sda.used_by
        );
        assert!(
            sda.used_by
                .contains(&"LUKS rynas-vol1 (unlocked)".to_string())
        );
        assert!(sda.used_by.contains(&"mounted /vol1".to_string()));
        let nvme = disk_usage(g, "nvme0n1");
        assert!(nvme.system);
        let sdd = disk_usage(g, "sdd");
        assert_eq!(sdd, DiskUsage::default());
        let sde = disk_usage(g, "sde");
        assert!(!sde.in_use);
        assert_eq!(sde.used_by, ["LUKS (locked)"]);
    }

    #[test]
    fn summaries() {
        let inv = nas_inventory();
        let sda = inv.disks.iter().find(|d| d.name == "sda").unwrap();
        assert_eq!(sda.kind, DiskKind::Hdd);
        assert_eq!(sda.health, HealthVerdict::Healthy);
        assert_eq!(sda.temperature_c, Some(36.0));
        assert!(!sda.smr);
        let sdd = inv.disks.iter().find(|d| d.name == "sdd").unwrap();
        assert!(sdd.smr);
        assert_eq!(sdd.health, HealthVerdict::Unknown);
        assert_eq!(sdd.power_state, PowerState::Standby);
        let nvme = inv.disks.iter().find(|d| d.name == "nvme0n1").unwrap();
        assert_eq!(nvme.kind, DiskKind::Nvme);
        assert!(inv.disks.iter().all(|d| d.name != "zram0"));
        let parts = partitions(&inv.graph, "nvme0n1");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].holders, ["md0"]);
    }

    #[test]
    fn filesystem_view() {
        let inv = nas_inventory();
        let f = inv
            .filesystems
            .iter()
            .find(|f| f.label.as_deref() == Some("vol1"))
            .unwrap();
        assert_eq!(f.mountpoint.as_deref(), Some("/vol1"));
        assert_eq!(f.mountpoints, ["/vol1", "/srv/photos"]);
        assert_eq!(f.compression.as_deref(), Some("zstd:1"));
        assert_eq!(f.devices[0].errors.corruption, 2);
        let photos = f.subvolumes.iter().find(|s| s.path == "photos").unwrap();
        assert_eq!(photos.mountpoint.as_deref(), Some("/srv/photos"));
        assert!(!photos.readonly);
        assert!(f.subvolumes.iter().find(|s| s.id == 260).unwrap().readonly);
        assert_eq!(f.profiles.len(), 3);
    }

    #[test]
    fn volume_tree() {
        let inv = nas_inventory();
        let t = tree(&inv);
        let vol1 = t
            .volumes
            .iter()
            .find(|v| v.label.as_deref() == Some("vol1"))
            .unwrap();
        assert!(vol1.encrypted);
        // crypt → md2 → 3 partitions → 3 disks
        let crypt = &vol1.layers[0];
        assert_eq!(crypt.kind, NodeKind::Crypt);
        assert_eq!(crypt.name, "rynas-vol1");
        let md = &crypt.children[0];
        assert_eq!(md.kind, NodeKind::Md);
        assert_eq!(md.children.len(), 3);
        assert_eq!(md.children[0].kind, NodeKind::Partition);
        assert_eq!(md.children[0].children[0].kind, NodeKind::Disk);
        // md2 is recovering and btrfs has corruption errors
        assert_eq!(vol1.health, HealthVerdict::Warning);
        assert!(vol1.subvolumes.iter().any(|s| s.name == ".snapshots"));
        let root = t
            .volumes
            .iter()
            .find(|v| v.label.as_deref() == Some("root"))
            .unwrap();
        assert!(!root.encrypted);
        assert_eq!(t.locked.len(), 1);
        assert_eq!(t.locked[0].children[0].name, "sde1");
        assert_eq!(t.unused_disks, ["sdd"]);
    }
}
