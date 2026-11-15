//! Recognizes a flexible RAID in the inventory: a logical volume whose physical volumes are
//! all md arrays of level 1, 5 or 6 built from partitions, where
//!
//! - the member partitions of each array (tier) have the same offset and size on every disk;
//! - the member disks of the tiers are nested (each tier's disks are among the lower one's);
//! - the partitions are in tier order on each disk.
//!
//! A layout that has the shape but breaks a rule is still reported, with `blockers`: rynas
//! shows it and refuses to change it. It is `managed` when the names follow the rynas pattern
//! (`rynas_<label>`/`data`, arrays `<label>_tN`, partitions `rynas-<label>-tN`) and a btrfs
//! labeled `<label>` sits on the logical volume. The structure comes from lsblk alone; the LVM
//! report adds sizes and free space for the pending checks (skipped while it is stale).

use std::collections::BTreeSet;

use rynas_api_types::storage::{
    ArrayHealth, FlexLayout, FlexPending, FlexPendingKind, FlexTier, MemberRole, RaidLevel,
    VolumeRecord,
};

use crate::Inventory;
use crate::flex::tiers;
use crate::lsblk::BlockDev;
use crate::lvm::split_dm_name;
use crate::md;
use crate::plan::{MIB, valid_label};

/// LUKS2 header and slack: a mapping smaller than its logical volume by more than this was
/// not resized after a growth.
const LUKS_SLACK: u64 = 32 * MIB;

/// Natural disk order (sda, sdb, ..., sdaa, nvme0n1), as [`crate::lsblk::BlockGraph::disks`].
fn disk_order(v: &mut [String]) {
    v.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
}

/// mdadm's form of an md UUID from lsblk's (`4f1c2a9e-7b3d-...` → `4f1c2a9e:7b3d5e60:...`).
pub fn mdadm_uuid(lsblk: &str) -> Option<String> {
    let hex: String = lsblk.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    (hex.len() == 32).then(|| {
        hex.as_bytes()
            .chunks(8)
            .map(|c| String::from_utf8_lossy(c).to_lowercase())
            .collect::<Vec<_>>()
            .join(":")
    })
}

/// The name an md array has without its host prefix ("nas:vol2_t1" → "vol2_t1").
pub(crate) fn short_name(n: &str) -> &str {
    n.rsplit(':').next().unwrap_or(n)
}

/// One tier array as found, before ordering.
struct Found<'a> {
    md: &'a BlockDev,
    level: RaidLevel,
    raid_disks: u32,
    /// Member partitions attached to the array (faulty ones included while md holds them).
    parts: Vec<&'a BlockDev>,
    start: u64,
    size: u64,
}

/// The md level of a tier array, from mdadm's view or lsblk's type.
fn tier_level(inv: &Inventory, md: &BlockDev) -> Option<(RaidLevel, u32)> {
    let a = inv.arrays.iter().find(|a| a.name == md.kname);
    let level = a.map(|a| a.level.as_str()).unwrap_or(md.kind.as_str());
    let raid_disks = a
        .map(|a| a.raid_disks)
        .unwrap_or(md.parents.len() as u32);
    let l = match level {
        "raid1" => RaidLevel::Raid1,
        "raid5" => RaidLevel::Raid5,
        "raid6" => RaidLevel::Raid6,
        _ => return None,
    };
    Some((l, raid_disks))
}

/// The kernel name of a physical volume path (`/dev/md126`, or `/dev/md/vol2_t1`).
pub(crate) fn resolve_pv<'a>(inv: &'a Inventory, path: &str) -> Option<&'a BlockDev> {
    if let Some(d) = inv.graph.find(path) {
        return Some(d);
    }
    let name = path.strip_prefix("/dev/md/")?;
    let a = inv
        .arrays
        .iter()
        .find(|a| a.md_name.as_deref().map(short_name) == Some(name))?;
    inv.graph.get(&a.name)
}

/// The md name of an array: mdadm's, else the label of its member partitions.
fn md_name<'a>(inv: &'a Inventory, f: &Found<'a>) -> Option<&'a str> {
    inv.arrays
        .iter()
        .find(|a| a.name == f.md.kname)
        .and_then(|a| a.md_name.as_deref())
        .or_else(|| f.parts.iter().find_map(|p| p.label.as_deref()))
        .map(short_name)
}

/// The md UUID of an array (mdadm form): mdadm's, else from its member partitions.
fn md_uuid(inv: &Inventory, f: &Found) -> Option<String> {
    inv.arrays
        .iter()
        .find(|a| a.name == f.md.kname)
        .and_then(|a| a.uuid.clone())
        .or_else(|| f.parts.iter().find_map(|p| p.uuid.as_deref().and_then(mdadm_uuid)))
}

/// Every flexible RAID of the inventory. `records` are the volumes rynas recorded (`None`
/// when not known here: the "not recorded" check is skipped); `min_tier` is the thinnest
/// tier ([`tiers::MIN_TIER`]).
pub fn detect(
    inv: &Inventory,
    records: Option<&[VolumeRecord]>,
    min_tier: u64,
) -> Vec<FlexLayout> {
    inv.graph
        .devs
        .values()
        .filter(|d| d.kind == "lvm")
        .filter_map(|lv| layout(inv, lv, records, min_tier))
        .collect()
}

fn layout(
    inv: &Inventory,
    lvdev: &BlockDev,
    records: Option<&[VolumeRecord]>,
    min_tier: u64,
) -> Option<FlexLayout> {
    let g = &inv.graph;
    let report = inv.lvm.lvs.iter().find(|l| {
        l.dm_path == lvdev.path || l.dm_path == format!("/dev/mapper/{}", lvdev.name)
    });
    let (vg, lv) = match report {
        Some(l) => (l.vg.clone(), l.name.clone()),
        None => split_dm_name(&lvdev.name)?,
    };
    let vg_report = inv.lvm.vg(&vg);
    let pvs: Vec<_> = inv.lvm.pvs.iter().filter(|p| p.vg == vg).collect();

    // the physical volumes: under the logical volume, and in its group without being used
    let mut pv_devs: Vec<&BlockDev> = lvdev.parents.iter().filter_map(|p| g.get(p)).collect();
    for p in &pvs {
        if let Some(d) = resolve_pv(inv, &p.name)
            && !pv_devs.iter().any(|x| x.kname == d.kname)
        {
            pv_devs.push(d);
        }
    }
    if pv_devs.is_empty() {
        return None;
    }
    // the shape: md arrays of level 1, 5 or 6 made of partitions
    let mut found: Vec<Found> = Vec::new();
    for md in &pv_devs {
        if !md.is_md() {
            return None;
        }
        let (level, raid_disks) = tier_level(inv, md)?;
        let parts: Vec<&BlockDev> = md.parents.iter().filter_map(|p| g.get(p)).collect();
        if parts.is_empty() || parts.iter().any(|p| p.kind != "part") {
            return None;
        }
        found.push(Found {
            md,
            level,
            raid_disks,
            start: parts[0].start.unwrap_or(0),
            size: parts[0].size,
            parts,
        });
    }
    found.sort_by_key(|f| (f.start, f.md.kname.clone()));

    let mut blockers: Vec<String> = Vec::new();
    let mut pending: Vec<FlexPending> = Vec::new();
    let pend = |pending: &mut Vec<FlexPending>, kind, subject: &str| {
        pending.push(FlexPending {
            kind,
            subject: Some(subject.to_string()),
        })
    };

    // identical slices on every member
    for f in &found {
        if f.parts.iter().any(|p| p.start.is_none() || p.start != Some(f.start) || p.size != f.size) {
            blockers.push(format!(
                "{}: its partitions do not have the same offset and size on every disk",
                f.md.kname
            ));
        }
    }
    // the disks of the layout, and which of them hold each tier's slice
    let mut disks: Vec<String> = found
        .iter()
        .flat_map(|f| f.parts.iter().flat_map(|p| g.disks_under(&p.kname)))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    disk_order(&mut disks);
    let slice_on = |disk: &str, f: &Found| -> Option<&BlockDev> {
        g.get(disk)?
            .children
            .iter()
            .filter_map(|c| g.get(c))
            .find(|p| p.kind == "part" && p.start == Some(f.start) && p.size == f.size)
    };
    let mut members: Vec<Vec<String>> = Vec::new();
    for f in &found {
        let mut m = Vec::new();
        for d in &disks {
            if let Some(p) = slice_on(d, f) {
                m.push(d.clone());
                if !f.parts.iter().any(|x| x.kname == p.kname) {
                    pend(&mut pending, FlexPendingKind::PartitionNotMember, &p.kname);
                }
            }
        }
        members.push(m);
    }
    for (i, w) in found.windows(2).enumerate() {
        if w[0].start + w[0].size > w[1].start {
            blockers.push(format!("{} and {} overlap", w[0].md.kname, w[1].md.kname));
        }
        if !members[i + 1].iter().all(|d| members[i].contains(d)) {
            blockers.push(format!(
                "the disks of {} are not all in {}",
                w[1].md.kname, w[0].md.kname
            ));
        }
    }
    for d in &disks {
        let partns: Vec<u32> = found
            .iter()
            .filter_map(|f| slice_on(d, f).and_then(|p| p.partn))
            .collect();
        if partns.windows(2).any(|w| w[0] >= w[1]) {
            blockers.push(format!("the partitions on {d} are not in tier order"));
        }
    }

    // tiers
    let mut tiers_out: Vec<FlexTier> = Vec::new();
    let mut tolerance = u32::MAX;
    for (i, f) in found.iter().enumerate() {
        let arr = inv.arrays.iter().find(|a| a.name == f.md.kname);
        let level_str = match f.level {
            RaidLevel::Raid1 => "raid1",
            RaidLevel::Raid5 => "raid5",
            _ => "raid6",
        };
        tolerance = tolerance.min(md::tolerance(level_str, f.raid_disks));
        let active: Vec<String> = arr
            .map(|a| {
                a.members
                    .iter()
                    .filter(|m| m.role == MemberRole::Active)
                    .filter_map(|m| m.disk.clone())
                    .collect()
            })
            .unwrap_or_else(|| f.parts.iter().flat_map(|p| g.disks_under(&p.kname)).collect());
        tiers_out.push(FlexTier {
            index: i as u32 + 1,
            start: f.start,
            end: f.start + f.size,
            level: Some(f.level),
            members: members[i].clone(),
            array: Some(f.md.kname.clone()),
            usable: arr.map_or(f.md.size, |a| a.size),
            health: arr.map_or(ArrayHealth::Unknown, |a| a.health),
            sync: arr.and_then(|a| a.sync.clone()),
            degraded: arr.map_or(0, |a| a.degraded),
            missing: members[i]
                .iter()
                .filter(|d| !active.contains(d))
                .cloned()
                .collect(),
        });
        if let Some(a) = arr
            && a.reshape_position.is_some()
            && a.sync.as_ref().is_none_or(|s| s.action != "reshape")
        {
            pend(&mut pending, FlexPendingKind::ReshapeInterrupted, &a.name);
        }
    }
    if tolerance == 0 {
        blockers.push("a tier has no redundancy".into());
    }

    // naming: rynas_<label>/data, <label>_tN arrays, rynas-<label>-tN partitions, btrfs <label>
    let label = vg.strip_prefix("rynas_").filter(|l| valid_label(l));
    let fs_dev = std::iter::once(lvdev)
        .chain(lvdev.children.iter().filter_map(|c| g.get(c)).filter(|c| c.kind == "crypt"))
        .find(|d| d.fstype.as_deref() == Some("btrfs"));
    let managed = label.is_some_and(|l| {
        lv == "data"
            && fs_dev.is_some_and(|d| d.label.as_deref() == Some(l))
            && found.iter().enumerate().all(|(i, f)| {
                let n = i + 1;
                md_name(inv, f) == Some(format!("{l}_t{n}").as_str())
                    && f.parts
                        .iter()
                        .all(|p| p.partlabel.as_deref() == Some(format!("rynas-{l}-t{n}").as_str()))
            })
    });
    if let Some(l) = label {
        // an array of the volume left out of the group (created, never added)
        for a in &inv.arrays {
            let tier_name = a
                .md_name
                .as_deref()
                .map(short_name)
                .and_then(|n| n.strip_prefix(&format!("{l}_t")))
                .is_some_and(|n| n.parse::<u32>().is_ok());
            if tier_name && !found.iter().any(|f| f.md.kname == a.name) {
                pend(&mut pending, FlexPendingKind::TierNotInVg, &a.name);
            }
        }
        if let Some(rec) = records.and_then(|r| r.iter().find(|r| r.label == l)) {
            for f in &found {
                let recorded = md_uuid(inv, f)
                    .is_some_and(|u| rec.tiers.iter().any(|t| t.md_uuid.eq_ignore_ascii_case(&u)));
                if !recorded {
                    pend(&mut pending, FlexPendingKind::TierNotRecorded, &f.md.kname);
                }
            }
        }
    }

    // the checks that need the LVM metadata
    let stale = vg_report.is_none()
        || found.iter().any(|f| inv.lvm_stale.contains(&f.md.kname));
    if let Some(v) = vg_report {
        if v.partial || pvs.iter().any(|p| p.missing) {
            blockers.push(format!("the volume group {vg} is missing a physical volume"));
        }
        let extent = v.extent_size.max(4 * MIB);
        for p in &pvs {
            if p.dev_size.saturating_sub(p.size) > 2 * extent {
                let subject = resolve_pv(inv, &p.name).map_or(p.name.clone(), |d| d.kname.clone());
                pend(&mut pending, FlexPendingKind::ResizePv, &subject);
            }
        }
        if v.free >= extent {
            pend(&mut pending, FlexPendingKind::ExtendLv, &lv);
        }
    }
    let lv_size = report.map_or(lvdev.size, |l| l.size.max(lvdev.size));
    if let Some(c) = lvdev
        .children
        .iter()
        .filter_map(|c| g.get(c))
        .find(|c| c.kind == "crypt")
        && lv_size.saturating_sub(c.size) > LUKS_SLACK
    {
        pend(&mut pending, FlexPendingKind::ResizeLuks, &c.name);
    }
    if let Some(d) = fs_dev
        && let Some(fs) = inv
            .filesystems
            .iter()
            .find(|f| d.uuid.as_deref() == Some(f.uuid.as_str()))
        && fs
            .devices
            .iter()
            .any(|x| g.find(&x.path).is_some_and(|b| b.kname == d.kname) && d.size.saturating_sub(x.size) > MIB)
    {
        pend(&mut pending, FlexPendingKind::ResizeFilesystem, &fs.mountpoint.clone().unwrap_or_else(|| d.name.clone()));
    }

    // space no tier uses
    let unused = disks
        .iter()
        .filter_map(|d| {
            let size = g.get(d)?.size;
            let top = tiers_out
                .iter()
                .filter(|t| t.members.contains(d))
                .map(|t| t.end)
                .max()?;
            let rest = tiers::reach(size).saturating_sub(top);
            (rest >= min_tier.max(MIB)).then(|| (d.clone(), rest))
        })
        .collect();

    Some(FlexLayout {
        tolerance: if tolerance == u32::MAX { 0 } else { tolerance },
        vg,
        lv,
        lv_device: lvdev.kname.clone(),
        size: lv_size,
        tiers: tiers_out,
        unused,
        managed,
        pending,
        blockers,
        stale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{FlexState, flex_inventory, nas_inventory};
    use rynas_api_types::storage::TierRecord;

    const MIB: u64 = 1 << 20;

    #[test]
    fn healthy_layout() {
        let inv = flex_inventory(FlexState::Healthy, true);
        assert_eq!(inv.flex.len(), 1);
        let l = &inv.flex[0];
        assert_eq!((l.vg.as_str(), l.lv.as_str(), l.lv_device.as_str()), ("rynas_vol2", "data", "dm-0"));
        assert_eq!(l.tolerance, 1);
        assert_eq!(l.level(), Some(RaidLevel::Flex1));
        assert!(l.managed, "{l:?}");
        assert!(l.blockers.is_empty(), "{:?}", l.blockers);
        assert!(l.pending.is_empty(), "{:?}", l.pending);
        assert!(!l.stale);
        assert!(l.unused.is_empty(), "{:?}", l.unused);
        assert_eq!(l.size, 12_001_669_545_984);
        assert_eq!(l.health(), ArrayHealth::Healthy);
        let t: Vec<_> = l
            .tiers
            .iter()
            .map(|t| (t.index, t.level, t.members.clone(), t.array.clone().unwrap()))
            .collect();
        assert_eq!(
            t,
            [
                (1, Some(RaidLevel::Raid5), vec!["sda".to_string(), "sdb".into(), "sdc".into()], "md126".to_string()),
                (2, Some(RaidLevel::Raid1), vec!["sdb".to_string(), "sdc".into()], "md127".to_string()),
            ]
        );
        assert_eq!(l.tiers[0].start, MIB);
        assert_eq!(l.tiers[0].end, l.tiers[1].start);
        // the slices are where the tier calculator puts them for 4+8+8 TB disks
        let plan = tiers::plan(
            &[("sda".into(), 4_000_787_030_016), ("sdb".into(), 8_001_563_222_016), ("sdc".into(), 8_001_563_222_016)],
            1,
            tiers::MIN_TIER,
        )
        .unwrap();
        assert_eq!(
            plan.tiers.iter().map(|t| (t.start, t.end)).collect::<Vec<_>>(),
            l.tiers.iter().map(|t| (t.start, t.end)).collect::<Vec<_>>()
        );
        assert!(l.tiers.iter().all(|t| t.missing.is_empty() && t.degraded == 0));
    }

    #[test]
    fn a_lost_disk_degrades_every_tier_it_was_in() {
        let inv = flex_inventory(FlexState::DiskGone, true);
        let l = &inv.flex[0];
        assert!(l.blockers.is_empty(), "{:?}", l.blockers);
        assert_eq!(l.health(), ArrayHealth::Degraded);
        assert!(l.tiers.iter().all(|t| t.health == ArrayHealth::Degraded && t.degraded == 1));
        // the disk is gone: nothing names it
        assert!(l.tiers.iter().all(|t| t.missing.is_empty()));
        assert_eq!(l.tiers[0].members, ["sda", "sdc"]);
        assert_eq!(l.tiers[1].members, ["sdc"]);

        let inv = flex_inventory(FlexState::DiskFaulty, true);
        let l = &inv.flex[0];
        assert_eq!(l.health(), ArrayHealth::Degraded);
        assert!(l.tiers.iter().all(|t| t.missing == ["sdb"]), "{:?}", l.tiers);
        assert!(l.blockers.is_empty() && l.pending.is_empty(), "{l:?}");
    }

    /// While a disk sleeps the LVM report is not refreshed: names come from the
    /// device-mapper name, the LVM checks are skipped.
    #[test]
    fn without_the_lvm_report() {
        let inv = flex_inventory(FlexState::Healthy, false);
        let l = &inv.flex[0];
        assert!(l.stale);
        assert_eq!((l.vg.as_str(), l.lv.as_str()), ("rynas_vol2", "data"));
        assert!(l.managed);
        assert_eq!(l.tiers.len(), 2);
        assert!(l.pending.is_empty());
    }

    #[test]
    fn pending_work_of_an_interrupted_growth() {
        let mut inv = flex_inventory(FlexState::Healthy, true);
        // the VG grew but the LV did not, md127 grew but its PV did not
        inv.lvm.vgs[0].free = 8 << 30;
        let pv = inv.lvm.pvs.iter_mut().find(|p| p.name == "/dev/md127").unwrap();
        pv.dev_size += 1 << 40;
        // md126 stopped half way through a reshape
        inv.arrays.iter_mut().find(|a| a.name == "md126").unwrap().reshape_position = Some(1234);
        // sdb2 holds the tier-2 slice but is not in md127
        inv.graph.devs.get_mut("md127").unwrap().parents.retain(|p| p != "sdb2");
        inv.graph.devs.get_mut("sdb2").unwrap().children.clear();
        // a third tier array exists outside the group
        let mut t3 = inv.arrays[0].clone();
        t3.name = "md125".into();
        t3.md_name = Some("nas:vol2_t3".into());
        inv.arrays.push(t3);
        // the record lists tier 1 only
        let uuid1 = mdadm_uuid("4f1c2a9e-7b3d-5e60-81a2-c3d4e5f60718").unwrap();
        let rec = VolumeRecord {
            label: "vol2".into(),
            level: RaidLevel::Flex1,
            tiers: vec![TierRecord { index: 1, md_uuid: uuid1 }],
            vg: Some("rynas_vol2".into()),
            ..Default::default()
        };
        let l = &detect(&inv, Some(&[rec]), tiers::MIN_TIER)[0];
        let mut kinds: Vec<(FlexPendingKind, String)> = l
            .pending
            .iter()
            .map(|p| (p.kind, p.subject.clone().unwrap_or_default()))
            .collect();
        kinds.sort_by_key(|(k, s)| (format!("{k:?}"), s.clone()));
        assert_eq!(
            kinds,
            [
                (FlexPendingKind::ExtendLv, "data".to_string()),
                (FlexPendingKind::PartitionNotMember, "sdb2".to_string()),
                (FlexPendingKind::ReshapeInterrupted, "md126".to_string()),
                (FlexPendingKind::ResizePv, "md127".to_string()),
                (FlexPendingKind::TierNotInVg, "md125".to_string()),
                (FlexPendingKind::TierNotRecorded, "md127".to_string()),
            ]
        );
        // the slice is still counted as sdb's
        assert_eq!(l.tiers[1].members, ["sdb", "sdc"]);
        assert!(l.blockers.is_empty(), "{:?}", l.blockers);
    }

    #[test]
    fn rule_breaks_are_blockers_and_foreign_names_are_read_only() {
        let mut inv = flex_inventory(FlexState::Healthy, true);
        inv.graph.devs.get_mut("sdc2").unwrap().start = Some(MIB * 8_000_000);
        let l = &detect(&inv, None, tiers::MIN_TIER)[0];
        assert!(
            l.blockers.iter().any(|b| b.contains("md127") && b.contains("offset")),
            "{:?}",
            l.blockers
        );

        let mut inv = flex_inventory(FlexState::Healthy, true);
        inv.graph.devs.get_mut("sdb1").unwrap().partlabel = Some("data".into());
        assert!(!detect(&inv, None, tiers::MIN_TIER)[0].managed);
        let mut inv = flex_inventory(FlexState::Healthy, false);
        inv.graph.devs.get_mut("dm-0").unwrap().name = "storage-media".into();
        let l = &detect(&inv, None, tiers::MIN_TIER)[0];
        assert_eq!((l.vg.as_str(), l.lv.as_str(), l.managed), ("storage", "media", false));
        assert!(l.blockers.is_empty());

        let mut inv = flex_inventory(FlexState::Healthy, true);
        inv.lvm.vgs[0].partial = true;
        let l = &detect(&inv, None, tiers::MIN_TIER)[0];
        assert!(l.blockers.iter().any(|b| b.contains("missing a physical volume")));
    }

    #[test]
    fn other_layouts_are_not_flexible() {
        assert!(nas_inventory().flex.is_empty());
        assert!(detect(&nas_inventory(), None, tiers::MIN_TIER).is_empty());
        // an LV straight on a partition
        let mut inv = flex_inventory(FlexState::Healthy, false);
        inv.graph.devs.get_mut("dm-0").unwrap().parents = vec!["nvme0n1p2".into()];
        assert!(detect(&inv, None, tiers::MIN_TIER).is_empty());
        assert_eq!(mdadm_uuid("4f1c2a9e-7b3d-5e60-81a2-c3d4e5f60718").unwrap(), "4f1c2a9e:7b3d5e60:81a2c3d4:e5f60718");
        assert_eq!(mdadm_uuid("xyz"), None);
    }
}
