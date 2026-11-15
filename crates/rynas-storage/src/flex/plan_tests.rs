//! Golden step sequences of the flexible RAID planner for the cases of the design table, the
//! tier-state model (planning again after every step converges), refusals.

use super::*;
use crate::flex::tiers::{MIN_TIER, reach};
use crate::lsblk::BlockDev;
use crate::tests_support::{FlexState, flex_inventory};
use rynas_api_types::storage::{
    BtrfsFilesystem, FlexPendingKind, PowerState, ScrubStatus, TierRecord, VolumeRecord,
};

const TB: u64 = 1_000_000_000_000;
const NOW: u64 = 1_793_836_860; // 2026-11-05

/// An inventory of blank HDDs.
fn blank_inventory(disks: &[(&str, u64)]) -> Inventory {
    let mut inv = Inventory::default();
    for (n, s) in disks {
        add_blank(&mut inv, n, *s);
    }
    inv
}

/// Adds a blank HDD to an inventory.
fn add_blank(inv: &mut Inventory, name: &str, size: u64) {
    let d = BlockDev {
        kname: name.into(),
        name: name.into(),
        path: format!("/dev/{name}"),
        kind: "disk".into(),
        size,
        rotational: true,
        serial: Some(format!("SER-{name}")),
        model: Some("TEST HDD".into()),
        ..Default::default()
    };
    inv.graph.devs.insert(name.into(), d);
    let d = inv.graph.get(name).unwrap();
    let s = crate::inventory::summary(d, &inv.graph, None, None, PowerState::Active, NOW * 1000, false);
    inv.disks.push(s);
}

fn sizes(disks: &[(&str, u64)]) -> Vec<DiskSize> {
    disks.iter().map(|(n, s)| (n.to_string(), s * TB)).collect()
}

fn tb_disks(disks: &[(&str, u64)]) -> Vec<(&'static str, u64)> {
    disks
        .iter()
        .map(|(n, s)| (&*Box::leak(n.to_string().into_boxed_str()), s * TB))
        .collect()
}

fn create_req(disks: &[&str], level: RaidLevel, encrypt: bool) -> VolumePlanRequest {
    VolumePlanRequest {
        disks: disks.iter().map(|s| s.to_string()).collect(),
        level,
        encrypt,
        compress: true,
        label: "vol2".into(),
    }
}

fn line(s: &PlanStep) -> String {
    match &s.action {
        PlanAction::Command { argv, .. } => argv.join(" "),
        PlanAction::Config { file, .. } => format!("config {file}"),
        PlanAction::Wait { condition } => format!("wait {condition}"),
        PlanAction::WriteSysfs { path, value } => format!("echo {value} > {path}"),
    }
}

fn lines(steps: &[PlanStep]) -> Vec<String> {
    steps.iter().map(line).collect()
}

/// Every step has a kind whose English text is its description.
fn kinds_ok(steps: &[PlanStep]) {
    for s in steps {
        let k = s.kind.expect("kind");
        assert_ne!(k, PlanStepKind::Other);
        assert_eq!(crate::plan::step_english(k, &s.args), s.description);
    }
}

/// `sgdisk --new` arguments of tier slice `[start, end)`.
fn new_arg(n: u32, start: u64, end: u64) -> String {
    format!("--new={n}:{}:{} --typecode={n}:FD00 --change-name={n}:rynas-vol2-t{n}", start / 512, end / 512 - 1)
}

/// The view after creating a volume over `disks` (TB) at `tolerance`, with every initial
/// resync finished.
fn created_view(disks: &[(&str, u64)], tolerance: u32) -> View {
    let td = tb_disks(disks);
    let inv = blank_inventory(&td);
    let names: Vec<&str> = disks.iter().map(|(n, _)| *n).collect();
    let level = RaidLevel::flexible(tolerance).unwrap();
    let p = crate::plan::plan_volume(&inv, &create_req(&names, level, false));
    assert!(p.valid, "{:?}", p.errors);
    let mut v = View::blank("vol2", tolerance, &sizes(disks));
    for s in &p.steps {
        v.apply(s);
    }
    for t in &mut v.tiers {
        t.sync = None;
    }
    v
}

/// Grows `v` by blank disks (TB): the plan of the tier calculator and the growth steps.
struct Grown {
    /// The view with the new disks attached, before the first step.
    begin: View,
    steps: Vec<PlanStep>,
    end: View,
    goal: Goal,
    target: Vec<Tier>,
    growth: tiers::Growth,
}

fn grow_view(v: &View, new: &[(&str, u64)]) -> Grown {
    let mut v = v.clone();
    let ns = sizes(new);
    for (n, s) in &ns {
        v.disks.push(VDisk { name: n.clone(), size: *s, parts: Vec::new(), others: 0 });
    }
    let growth = tiers::grow(&v.shapes(), &v.layout_disks(), &ns, v.tolerance, MIN_TIER);
    let goal = Goal { old: None, new: ns.iter().map(|(n, _)| n.clone()).collect() };
    let target = growth.tiers.clone();
    let begin = v.clone();
    let steps = growth_steps(&mut v, &goal, Some(&target)).unwrap();
    Grown { begin, steps, end: v, goal, target, growth }
}

/// What a finished growth must look like: shape of every tier and LVM sizes.
/// (index, level, raid disks, members with roles, bitmap, in the group, recorded)
type Shape = (u32, Option<RaidLevel>, u32, Vec<(String, MemberRole)>, bool, bool, bool);

fn shape(v: &View) -> Vec<Shape> {
    v.tiers
        .iter()
        .map(|t| {
            let mut m: Vec<(String, MemberRole)> =
                t.members.iter().map(|m| (m.part.clone(), m.role)).collect();
            m.sort_by(|a, b| a.0.cmp(&b.0));
            (t.index, t.level, t.raid_disks, m, t.bitmap, t.in_vg, t.recorded)
        })
        .collect()
}

/// Planning again from the end state plans nothing; a crash after any step, planned again
/// with the same goal (and, once the partitions are written, with no goal: `complete`),
/// reaches the same end state.
fn converges(start: &View, goal: &Goal, target: &[Tier], steps: &[PlanStep], end: &View, complete_too: bool) {
    let mut again = end.clone();
    assert!(
        growth_steps(&mut again, &Goal::default(), None).unwrap().is_empty(),
        "steps left after the plan"
    );
    let mut again = end.clone();
    assert!(growth_steps(&mut again, goal, Some(target)).unwrap().is_empty());
    let prelude = steps.iter().position(|s| s.kind == Some(PlanStepKind::WaitUdev)).map_or(0, |i| i + 1);
    for i in 0..=steps.len() {
        let mut v = start.clone();
        for s in &steps[..i] {
            v.apply(s);
        }
        let mut w = v.clone();
        growth_steps(&mut w, goal, Some(target)).unwrap();
        assert_eq!(shape(&w), shape(end), "same goal after step {i}: {}", steps.get(i.saturating_sub(1)).map(line).unwrap_or_default());
        assert_eq!((w.lv_size, w.vg_free, w.fs.clone()), (end.lv_size, end.vg_free, end.fs.clone()), "after step {i}");
        // complete takes a new disk's slices only once the disk is in a tier: before, they
        // are slices on a disk in no tier, never adopted silently (adding the disk again
        // clears and uses it)
        let adopted = goal.new.iter().all(|d| v.in_some_tier(d));
        if complete_too && i >= prelude && adopted {
            let mut w = v.clone();
            growth_steps(&mut w, &Goal::default(), None).unwrap();
            assert_eq!(shape(&w), shape(end), "complete after step {i}: {}", line(&steps[i.saturating_sub(1)]));
            assert_eq!(w.lv_size, end.lv_size, "complete after step {i}");
        }
    }
}

// ------------------------------------------------------------------------------- create

#[test]
fn create_4_8_8_one_disk_tolerance() {
    let inv = blank_inventory(&[("sda", 4 * TB), ("sdb", 8 * TB), ("sdc", 8 * TB)]);
    let p = crate::plan::plan_volume(&inv, &create_req(&["sda", "sdb", "sdc"], RaidLevel::Flex1, false));
    assert!(p.valid, "{:?}", p.errors);
    kinds_ok(&p.steps);
    let (b1, b2) = (reach(4 * TB), reach(8 * TB));
    let t1 = new_arg(1, MIB, b1);
    let t2 = new_arg(2, b1, b2);
    assert_eq!(
        lines(&p.steps),
        [
            "wipefs --all /dev/sda".to_string(),
            "sgdisk --zap-all /dev/sda".into(),
            format!("sgdisk {t1} /dev/sda"),
            "wipefs --all /dev/sdb".into(),
            "sgdisk --zap-all /dev/sdb".into(),
            format!("sgdisk {t1} {t2} /dev/sdb"),
            "wipefs --all /dev/sdc".into(),
            "sgdisk --zap-all /dev/sdc".into(),
            format!("sgdisk {t1} {t2} /dev/sdc"),
            "udevadm settle".into(),
            "mdadm --create /dev/md/vol2_t1 --run --level=5 --raid-devices=3 --metadata=1.2 --name=vol2_t1 --bitmap=internal /dev/sda1 /dev/sdb1 /dev/sdc1".into(),
            "mdadm --create /dev/md/vol2_t2 --run --level=1 --raid-devices=2 --metadata=1.2 --name=vol2_t2 --bitmap=internal /dev/sdb2 /dev/sdc2".into(),
            "config /etc/mdadm/mdadm.conf".into(),
            "pvcreate --yes --force /dev/md/vol2_t1".into(),
            "pvcreate --yes --force /dev/md/vol2_t2".into(),
            "vgcreate rynas_vol2 /dev/md/vol2_t1 /dev/md/vol2_t2".into(),
            "lvcreate --yes -l 100%FREE -n data rynas_vol2".into(),
            "mkfs.btrfs --force --label vol2 --data single --metadata dup /dev/rynas_vol2/data".into(),
            "config /etc/fstab".into(),
            "mkdir -p /vol2".into(),
            "systemctl start vol2.mount".into(),
            "btrfs subvolume create /vol2/.snapshots".into(),
            "btrfs subvolume create /vol2/@rynas-cache".into(),
            "chattr +C /vol2/@rynas-cache".into(),
        ]
    );
    // destructive steps name what they erase
    assert_eq!(p.steps[0].destroys, ["sda"]);
    assert_eq!(p.steps[10].destroys, ["sda1", "sdb1", "sdc1"]);
    assert_eq!(p.fault_tolerance, 1);
    assert_eq!((p.usable_bytes as f64 / 1e12).round(), 12.0);
    assert_eq!(p.tiers_after.len(), 2);
    // no SMART data in the fixture: nothing else to warn about
    assert!(
        p.warning_items.iter().all(|w| w.code == PlanWarningCode::NoSmartData),
        "{:?}",
        p.warnings
    );
    let get = |k: &str| p.summary.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
    assert_eq!(get("vg").as_deref(), Some("rynas_vol2"));
    assert_eq!(get("luks_device").as_deref(), Some("/dev/rynas_vol2/data"));
    assert_eq!(get("level").as_deref(), Some("flex1"));
}

#[test]
fn create_encrypted_puts_luks_on_the_logical_volume() {
    let inv = blank_inventory(&[("sda", 4 * TB), ("sdb", 8 * TB)]);
    let p = crate::plan::plan_volume(&inv, &create_req(&["sda", "sdb"], RaidLevel::Flex1, true));
    assert!(p.valid, "{:?}", p.errors);
    let l = lines(&p.steps);
    assert!(l.contains(&"cryptsetup luksFormat --batch-mode --type luks2 --sector-size 4096 --pbkdf argon2id --pbkdf-memory 262144 --label rynas-vol2 --key-file - /dev/rynas_vol2/data".to_string()), "{l:?}");
    assert!(l.contains(&"mkfs.btrfs --force --label vol2 --data single --metadata dup /dev/mapper/rynas-vol2".to_string()));
    let lv = l.iter().position(|x| x.starts_with("lvcreate")).unwrap();
    let luks = l.iter().position(|x| x.starts_with("cryptsetup luksFormat")).unwrap();
    assert!(lv < luks);
    // 4 + 8: one 4 TB mirror, 4 TB of sdb unused
    assert!(p.warning_items.iter().any(|w| w.code == PlanWarningCode::FlexUnusedSpace && w.args.items == ["sdb"]));
    assert!(p.warning_items.iter().any(|w| w.code == PlanWarningCode::Encrypted));
}

/// The design table: tiers, levels and usable space of each new layout.
#[test]
fn create_cases_of_the_design_table() {
    struct Case {
        disks: &'static [(&'static str, u64)],
        level: RaidLevel,
        /// (level, members) of each array created
        arrays: &'static [(&'static str, usize)],
        usable_tb: f64,
    }
    let cases = [
        Case { disks: &[("sda", 4), ("sdb", 4), ("sdc", 8), ("sdd", 12)], level: RaidLevel::Flex1, arrays: &[("5", 4), ("1", 2)], usable_tb: 16.0 },
        Case { disks: &[("sda", 4), ("sdb", 4), ("sdc", 8), ("sdd", 12)], level: RaidLevel::Flex2, arrays: &[("6", 4)], usable_tb: 8.0 },
        Case { disks: &[("sda", 4), ("sdb", 4), ("sdc", 8), ("sdd", 8)], level: RaidLevel::Flex2, arrays: &[("6", 4)], usable_tb: 8.0 },
        Case { disks: &[("sda", 4), ("sdb", 8), ("sdc", 8), ("sdd", 8)], level: RaidLevel::Flex2, arrays: &[("6", 4), ("1", 3)], usable_tb: 12.0 },
    ];
    for c in cases {
        let inv = blank_inventory(&tb_disks(c.disks));
        let names: Vec<&str> = c.disks.iter().map(|(n, _)| *n).collect();
        let p = crate::plan::plan_volume(&inv, &create_req(&names, c.level, false));
        assert!(p.valid, "{:?}: {:?}", c.disks, p.errors);
        kinds_ok(&p.steps);
        let creates: Vec<(String, usize)> = p
            .steps
            .iter()
            .filter(|s| s.kind == Some(PlanStepKind::CreateArray))
            .map(|s| {
                let PlanAction::Command { argv, .. } = &s.action else { unreachable!() };
                let level = argv.iter().find_map(|a| a.strip_prefix("--level=")).unwrap().to_string();
                (level, argv.iter().filter(|a| a.starts_with("/dev/sd")).count())
            })
            .collect();
        let want: Vec<(String, usize)> = c.arrays.iter().map(|(l, n)| (l.to_string(), *n)).collect();
        assert_eq!(creates, want, "{:?} {:?}", c.disks, c.level);
        assert_eq!((p.usable_bytes as f64 / 1e12).round(), c.usable_tb, "{:?} {:?}", c.disks, c.level);
        // partitions only for the tiers that exist
        let parts = lines(&p.steps).iter().filter(|l| l.starts_with("sgdisk --new")).map(|l| l.matches("--new=").count()).sum::<usize>();
        assert_eq!(parts, c.arrays.iter().map(|(_, n)| n).sum::<usize>());
        // planning nothing more once created: the model of the created volume is complete
        let mut v = created_view(c.disks, c.level.tolerance());
        assert!(growth_steps(&mut v, &Goal::default(), None).unwrap().is_empty());
        assert_eq!(v.lv_size, p.usable_bytes);
    }
    // 2-disk tolerance on two disks is refused
    let inv = blank_inventory(&[("sda", 4 * TB), ("sdb", 8 * TB)]);
    let p = crate::plan::plan_volume(&inv, &create_req(&["sda", "sdb"], RaidLevel::Flex2, false));
    assert!(!p.valid);
    assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::DiskCount));
}

/// The capacity calculator of the web interface ([`rynas_api_types::capacity::volume`]) gives
/// the numbers of the plan of every level.
#[test]
fn the_calculator_matches_the_plans() {
    use rynas_api_types::capacity;
    for sizes in [&[4, 8, 8][..], &[4, 4, 12, 16, 20], &[8, 8, 8, 8], &[2, 3]] {
        let disks: Vec<(String, u64)> = sizes.iter().enumerate().map(|(i, s)| (format!("sd{}", (b'a' + i as u8) as char), s * TB)).collect();
        let named: Vec<(&str, u64)> = disks.iter().map(|(n, s)| (n.as_str(), *s)).collect();
        let inv = blank_inventory(&named);
        let names: Vec<&str> = named.iter().map(|(n, _)| *n).collect();
        let bytes: Vec<u64> = disks.iter().map(|(_, s)| *s).collect();
        for level in [
            RaidLevel::Single, RaidLevel::Raid0, RaidLevel::Raid1, RaidLevel::Raid5, RaidLevel::Raid6,
            RaidLevel::Raid10, RaidLevel::Flex1, RaidLevel::Flex2,
        ] {
            let p = crate::plan::plan_volume(&inv, &create_req(&names, level, false));
            let c = capacity::volume(level, &bytes, false, MIN_TIER);
            assert_eq!(p.valid, c.is_some(), "{level:?} {sizes:?}: {:?}", p.errors);
            if let Some(c) = c {
                assert_eq!(
                    (p.usable_bytes, p.raw_bytes, p.wasted_bytes, p.fault_tolerance),
                    (c.usable, c.raw, c.wasted, c.tolerance),
                    "{level:?} {sizes:?}"
                );
            }
        }
    }
}

// ------------------------------------------------------------------------------- growth

#[test]
fn add_a_12_to_4_8_8() {
    let start = created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1);
    let Grown { begin, steps, end, goal, target, growth: g } = grow_view(&start, &[("sdd", 12)]);
    kinds_ok(&steps);
    let (b1, b2) = (reach(4 * TB), reach(8 * TB));
    assert_eq!(
        lines(&steps),
        [
            "wipefs --all /dev/sdd".to_string(),
            "sgdisk --zap-all /dev/sdd".into(),
            format!("sgdisk {} {} /dev/sdd", new_arg(1, MIB, b1), new_arg(2, b1, b2)),
            "udevadm settle".into(),
            // tier 1: RAID5 3 → 4
            "mdadm --grow /dev/md/vol2_t1 --bitmap=none".into(),
            "mdadm /dev/md/vol2_t1 --add /dev/sdd1".into(),
            "mdadm --grow /dev/md/vol2_t1 --raid-devices=4".into(),
            "wait /sys/block/md101/md/sync_action == idle".into(),
            "mdadm --grow /dev/md/vol2_t1 --bitmap=internal".into(),
            "pvresize /dev/md/vol2_t1".into(),
            "lvextend -l +100%FREE rynas_vol2/data".into(),
            "btrfs filesystem resize max /vol2".into(),
            // tier 2: RAID1 → RAID5 over 3
            "mdadm --grow /dev/md/vol2_t2 --bitmap=none".into(),
            "mdadm --grow /dev/md/vol2_t2 --level=5".into(),
            "mdadm /dev/md/vol2_t2 --add /dev/sdd2".into(),
            "mdadm --grow /dev/md/vol2_t2 --raid-devices=3".into(),
            "wait /sys/block/md102/md/sync_action == idle".into(),
            "mdadm --grow /dev/md/vol2_t2 --bitmap=internal".into(),
            "pvresize /dev/md/vol2_t2".into(),
            "lvextend -l +100%FREE rynas_vol2/data".into(),
            "btrfs filesystem resize max /vol2".into(),
        ]
    );
    assert_eq!((lvm_usable(&g.tiers) as f64 / 1e12).round(), 20.0);
    assert_eq!(end.lv_size, lvm_usable(&g.tiers));
    converges(&begin, &goal, &target, &steps, &end, true);
}

#[test]
fn replace_the_4_with_a_12_in_4_8_8() {
    let start = created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1);
    let mut v = start.clone();
    v.disks.push(VDisk { name: "sdd".into(), size: 12 * TB, parts: Vec::new(), others: 0 });
    let g = tiers::replace(&v.shapes(), &v.layout_disks(), "sda", &("sdd".into(), 12 * TB), 1, MIN_TIER).unwrap();
    let goal = Goal { old: Some("sda".into()), new: vec!["sdd".into()] };
    let begin = v.clone();
    let steps = growth_steps(&mut v, &goal, Some(&g.tiers)).unwrap();
    kinds_ok(&steps);
    let l = lines(&steps);
    assert_eq!(
        l[4..],
        [
            "mdadm /dev/md/vol2_t1 --add-spare /dev/sdd1".to_string(),
            "mdadm /dev/md/vol2_t1 --replace /dev/sda1 --with /dev/sdd1".into(),
            "wait /sys/block/md101/md/sync_action == idle".into(),
            "mdadm /dev/md/vol2_t1 --remove /dev/sda1".into(),
            "mdadm --grow /dev/md/vol2_t2 --bitmap=none".into(),
            "mdadm --grow /dev/md/vol2_t2 --level=5".into(),
            "mdadm /dev/md/vol2_t2 --add /dev/sdd2".into(),
            "mdadm --grow /dev/md/vol2_t2 --raid-devices=3".into(),
            "wait /sys/block/md102/md/sync_action == idle".into(),
            "mdadm --grow /dev/md/vol2_t2 --bitmap=internal".into(),
            "pvresize /dev/md/vol2_t2".into(),
            "lvextend -l +100%FREE rynas_vol2/data".into(),
            "btrfs filesystem resize max /vol2".into(),
            "mdadm --zero-superblock /dev/sda1".into(),
            "sgdisk --zap-all /dev/sda".into(),
        ]
    );
    assert_eq!(steps[6].kind, Some(PlanStepKind::WaitCopy));
    assert!(v.tiers.iter().all(|t| t.members.iter().all(|m| m.disk != "sda")));
    assert_eq!((v.lv_size as f64 / 1e12).round(), 16.0);
    converges(&begin, &goal, &g.tiers, &steps, &v, false);
}

/// 2-disk tolerance, 4+4+8+8 (tier 2 unused) plus a 12: three disks reach past tier 1, so a
/// three-way mirror tier is made on the disks in use and the new one. It is recorded before
/// it becomes a physical volume.
#[test]
fn a_new_top_tier_on_disks_in_use() {
    let start = created_view(&[("sda", 4), ("sdb", 4), ("sdc", 8), ("sdd", 8)], 2);
    assert_eq!(start.tiers.len(), 1);
    let Grown { begin, steps, end, goal, target, growth: g } = grow_view(&start, &[("sde", 12)]);
    kinds_ok(&steps);
    assert_eq!(g.created, [2]);
    let (b1, b2) = (reach(4 * TB), reach(8 * TB));
    let l = lines(&steps);
    assert_eq!(
        l[..9],
        [
            format!("sgdisk {} /dev/sdc", new_arg(2, b1, b2)),
            "partx --update --nr 2 /dev/sdc".into(),
            format!("sgdisk {} /dev/sdd", new_arg(2, b1, b2)),
            "partx --update --nr 2 /dev/sdd".into(),
            "wipefs --all /dev/sde".into(),
            "sgdisk --zap-all /dev/sde".into(),
            format!("sgdisk {} {} /dev/sde", new_arg(1, MIB, b1), new_arg(2, b1, b2)),
            "udevadm settle".into(),
            "mdadm --create /dev/md/vol2_t2 --run --level=1 --raid-devices=3 --metadata=1.2 --name=vol2_t2 --bitmap=internal /dev/sdc2 /dev/sdd2 /dev/sde2".into(),
        ]
    );
    assert_eq!(steps[0].kind, Some(PlanStepKind::CreatePartition));
    assert_eq!(steps[1].kind, Some(PlanStepKind::AddPartitionLive));
    assert_eq!(steps[0].args.other.as_deref(), Some("sdc2"));
    assert_eq!(
        l[9..12],
        [
            "config /etc/mdadm/mdadm.conf".to_string(),
            "pvcreate --yes --force /dev/md/vol2_t2".into(),
            "vgextend rynas_vol2 /dev/md/vol2_t2".into(),
        ]
    );
    let PlanAction::Config { entry, .. } = &steps[9].action else { panic!() };
    assert_eq!(entry, "ARRAY /dev/md/vol2_t2 metadata=1.2 UUID=<from mdadm --detail --export /dev/md/vol2_t2>");
    // then tier 1 takes the new disk: RAID6 4 → 5
    assert!(l.contains(&"mdadm /dev/md/vol2_t1 --add /dev/sde1".to_string()));
    assert!(l.contains(&"mdadm --grow /dev/md/vol2_t1 --raid-devices=5 --layout=normalise".to_string()), "{l:?}");
    assert_eq!((end.lv_size as f64 / 1e12).round(), 16.0);
    converges(&begin, &goal, &target, &steps, &end, true);
}

/// 2-disk tolerance, 4+8+8+8: tier 2 is a three-way mirror; a fourth 8 makes it RAID6
/// through RAID5, and the plan warns that it tolerates one disk meanwhile.
#[test]
fn three_way_mirror_to_raid6() {
    let start = created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8), ("sdd", 8)], 2);
    let Grown { begin, steps, end, goal, target, growth: g } = grow_view(&start, &[("sde", 8)]);
    kinds_ok(&steps);
    assert!(g.level_changes[0].reduces_tolerance());
    let l = lines(&steps);
    let t2: Vec<&String> = l
        .iter()
        .filter(|x| x.contains("vol2_t2") || x.contains("md102") || x.contains("sdd2"))
        .collect();
    assert_eq!(
        t2,
        [
            "mdadm --grow /dev/md/vol2_t2 --bitmap=none",
            "mdadm /dev/md/vol2_t2 --fail /dev/sdd2",
            "mdadm /dev/md/vol2_t2 --remove /dev/sdd2",
            "mdadm --zero-superblock /dev/sdd2",
            "mdadm --grow /dev/md/vol2_t2 --raid-devices=2",
            "mdadm --grow /dev/md/vol2_t2 --level=5",
            "mdadm /dev/md/vol2_t2 --add /dev/sdd2 /dev/sde2",
            "mdadm --grow /dev/md/vol2_t2 --raid-devices=3",
            "wait /sys/block/md102/md/sync_action == idle",
            "pvresize /dev/md/vol2_t2",
            "mdadm --grow /dev/md/vol2_t2 --level=6 --raid-devices=4 --layout=preserve",
            "wait /sys/block/md102/md/sync_action == idle",
            "mdadm --grow /dev/md/vol2_t2 --bitmap=internal",
        ]
    );
    let t = end.tiers.iter().find(|t| t.index == 2).unwrap();
    assert_eq!((t.level, t.raid_disks, t.active()), (Some(RaidLevel::Raid6), 4, 4));
    assert_eq!((end.lv_size as f64 / 1e12).round(), 20.0);
    converges(&begin, &goal, &target, &steps, &end, true);
}

/// sdb of 4+8+8 failed in both tiers; an 8 takes its place: both rebuilds are queued before
/// the first wait, the failed partitions are removed.
#[test]
fn replace_a_failed_disk_in_the_model() {
    let mut start = created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1);
    for t in &mut start.tiers {
        for m in t.members.iter_mut().filter(|m| m.disk == "sdb") {
            m.role = MemberRole::Faulty;
        }
    }
    start.disks.push(VDisk { name: "sdd".into(), size: 8 * TB, parts: Vec::new(), others: 0 });
    let g = tiers::replace(&start.shapes(), &start.layout_disks(), "sdb", &("sdd".into(), 8 * TB), 1, MIN_TIER).unwrap();
    let goal = Goal { old: Some("sdb".into()), new: vec!["sdd".into()] };
    let mut end = start.clone();
    let steps = growth_steps(&mut end, &goal, Some(&g.tiers)).unwrap();
    kinds_ok(&steps);
    assert_eq!(
        lines(&steps)[4..],
        [
            "mdadm /dev/md/vol2_t1 --remove /dev/sdb1",
            "mdadm /dev/md/vol2_t1 --add /dev/sdd1",
            "mdadm /dev/md/vol2_t2 --remove /dev/sdb2",
            "mdadm /dev/md/vol2_t2 --add /dev/sdd2",
            "wait /sys/block/md101/md/sync_action == idle",
            "wait /sys/block/md102/md/sync_action == idle",
            // the failed disk is still attached: its old partitions are cleared last
            "mdadm --zero-superblock /dev/sdb1",
            "mdadm --zero-superblock /dev/sdb2",
            "sgdisk --zap-all /dev/sdb",
        ]
    );
    assert!(end.tiers.iter().all(|t| t.degraded() == 0 && t.members.iter().all(|m| m.disk != "sdb")));
    converges(&start, &goal, &g.tiers, &steps, &end, true);
}

// ------------------------------------------------------------------------------- on the fixture

fn flex(state: FlexState) -> Inventory {
    let mut inv = flex_inventory(state, true);
    add_blank(&mut inv, "sdd", 8_001_563_222_016);
    inv
}

fn replace_req(old: &str, new: &str) -> ReplacePlanRequest {
    ReplacePlanRequest {
        array: String::new(),
        old: old.into(),
        new_disk: new.into(),
        volume: Some("vol2".into()),
    }
}

#[test]
fn the_fixture_view() {
    let inv = flex_inventory(FlexState::Healthy, true);
    let v = View::from_inventory(&inv, &inv.flex[0], "vol2");
    assert_eq!(v.tolerance, 1);
    assert_eq!(v.tiers.len(), 2);
    let t = &v.tiers[1];
    assert_eq!((t.dev.as_str(), t.kname.as_str(), t.level), ("/dev/md127", "md127", Some(RaidLevel::Raid1)));
    assert_eq!(t.member_disks(), ["sdb", "sdc"]);
    assert!(t.bitmap && t.in_vg && t.pv.is_some());
    assert_eq!(v.fs.as_ref().map(|f| f.0.as_str()), Some("/vol2"));
    assert_eq!(v.disks.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["sda", "sdb", "sdc"]);
    assert!(v.disks[1].parts.iter().all(|p| !p.fresh), "members carry the tier's superblock");
    // nothing to complete on a healthy volume
    let p = plan_complete(&inv, &CompletePlanRequest { volume: "vol2".into() });
    assert!(p.valid && p.steps.is_empty(), "{:?} {:?}", p.errors, lines(&p.steps));
}

#[test]
fn complete_an_interrupted_growth() {
    let mut inv = flex_inventory(FlexState::Healthy, true);
    // the group grew but the volume did not; md127 grew but its physical volume did not
    inv.lvm.vgs[0].free = 8 << 30;
    inv.lvm.pvs.iter_mut().find(|p| p.name == "/dev/md127").unwrap().dev_size += 1 << 40;
    inv.flex = crate::flex::detect::detect(&inv, None, MIN_TIER);
    let p = plan_complete(&inv, &CompletePlanRequest { volume: "vol2".into() });
    assert!(p.valid, "{:?}", p.errors);
    kinds_ok(&p.steps);
    assert_eq!(
        lines(&p.steps),
        [
            "pvresize /dev/md127",
            "lvextend -l +100%FREE rynas_vol2/data",
            "btrfs filesystem resize max /vol2",
        ]
    );
    assert_eq!(p.operation, "complete_growth");
    // an interrupted reshape is continued and waited for, then the bitmap goes back on
    let mut inv = flex_inventory(FlexState::Healthy, true);
    let a = inv.arrays.iter_mut().find(|a| a.name == "md126").unwrap();
    a.reshape_position = Some(1234);
    a.bitmap = Some("none".into());
    inv.flex = crate::flex::detect::detect(&inv, None, MIN_TIER);
    let p = plan_complete(&inv, &CompletePlanRequest { volume: "vol2".into() });
    assert_eq!(
        lines(&p.steps)[..3],
        [
            "mdadm --grow --continue /dev/md126",
            "wait /sys/block/md126/md/sync_action == idle",
            "mdadm --grow /dev/md126 --bitmap=internal",
        ]
    );
}

#[test]
fn replace_a_failed_disk() {
    let inv = flex(FlexState::DiskFaulty);
    let p = plan_replace_at(&inv, &replace_req("sdb", "sdd"), NOW);
    assert!(p.valid, "{:?}", p.errors);
    kinds_ok(&p.steps);
    let t1 = new_arg(1, MIB, 4_000_651_935_744);
    let l = lines(&p.steps);
    assert_eq!(l[2], format!("sgdisk {t1} {} /dev/sdd", new_arg(2, 4_000_651_935_744, 4_000_651_935_744 + 4_000_776_716_288)));
    assert_eq!(
        l[3..],
        [
            "udevadm settle".to_string(),
            "mdadm /dev/md126 --remove /dev/sdb1".into(),
            "mdadm /dev/md126 --add /dev/sdd1".into(),
            "mdadm /dev/md127 --remove /dev/sdb2".into(),
            "mdadm /dev/md127 --add /dev/sdd2".into(),
            "wait /sys/block/md126/md/sync_action == idle".into(),
            "wait /sys/block/md127/md/sync_action == idle".into(),
            "mdadm --zero-superblock /dev/sdb1".into(),
            "mdadm --zero-superblock /dev/sdb2".into(),
            "sgdisk --zap-all /dev/sdb".into(),
        ]
    );
    assert_eq!(p.steps[5].kind, Some(PlanStepKind::AddForRebuild));
    assert_eq!(p.steps[9].kind, Some(PlanStepKind::WaitRebuild));
    assert_eq!(p.operation, "replace_disk");
    // a smaller disk cannot take tier 2
    let mut inv = flex(FlexState::DiskFaulty);
    add_blank(&mut inv, "sde", 6 * TB);
    let p = plan_replace_at(&inv, &replace_req("sdb", "sde"), NOW);
    assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::DiskTooSmall), "{:?}", p.issues);
}

#[test]
fn replace_a_missing_disk() {
    let inv = flex(FlexState::DiskGone);
    let p = plan_replace_at(&inv, &replace_req(MISSING, "sdd"), NOW);
    assert!(p.valid, "{:?}", p.errors);
    assert_eq!(
        lines(&p.steps)[3..],
        [
            "udevadm settle".to_string(),
            "mdadm /dev/md126 --add /dev/sdd1".into(),
            "mdadm /dev/md127 --add /dev/sdd2".into(),
            "wait /sys/block/md126/md/sync_action == idle".into(),
            "wait /sys/block/md127/md/sync_action == idle".into(),
        ]
    );
    // on a healthy volume nothing is missing
    let p = plan_replace_at(&flex(FlexState::Healthy), &replace_req(MISSING, "sdd"), NOW);
    assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::NotAMember));
}

#[test]
fn grow_on_the_fixture_and_its_refusals() {
    let mut inv = flex(FlexState::Healthy);
    add_blank(&mut inv, "sde", 12 * TB);
    let req = |disks: &[&str], level| GrowPlanRequest {
        array: String::new(),
        disks: disks.iter().map(|s| s.to_string()).collect(),
        level,
        volume: Some("vol2".into()),
    };
    let p = plan_grow_at(&inv, &req(&["sde"], None), NOW);
    assert!(p.valid, "{:?}", p.errors);
    kinds_ok(&p.steps);
    assert_eq!(p.operation, "grow_array");
    assert_eq!(p.tiers_before.len(), 2);
    assert_eq!(p.tiers_after.iter().map(|t| t.members.len()).collect::<Vec<_>>(), [4, 3]);
    assert_eq!(p.tiers_after[0].array.as_deref(), Some("md126"));
    let codes: Vec<PlanWarningCode> = p.warning_items.iter().map(|w| w.code).collect();
    assert!(codes.contains(&PlanWarningCode::FlexUnusedSpace), "{codes:?}");
    assert!(codes.contains(&PlanWarningCode::FlexLongReshape), "{codes:?}");
    assert!(codes.contains(&PlanWarningCode::FlexNoRecentScrub), "no scrub ever ran: {codes:?}");
    assert_eq!(p.warning_items.len(), p.warnings.len());
    assert!(lines(&p.steps).contains(&"mdadm --grow /dev/md127 --level=5".to_string()));
    // a recent clean scrub: no scrub warning; uncorrectable errors: refused
    let mut inv2 = inv.clone();
    set_scrub(&mut inv2, "Sun Nov  1 03:00:00 2026", 0);
    let p = plan_grow_at(&inv2, &req(&["sde"], None), NOW);
    assert!(!p.warning_items.iter().any(|w| w.code == PlanWarningCode::FlexNoRecentScrub));
    set_scrub(&mut inv2, "Sun Nov  1 03:00:00 2026", 2);
    let p = plan_grow_at(&inv2, &req(&["sde"], None), NOW);
    assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::ScrubErrors), "{:?}", p.issues);
    // tolerance change, disk in use, missing disks, unknown volume
    let p = plan_grow_at(&inv, &req(&["sde"], Some(RaidLevel::Flex2)), NOW);
    assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::ToleranceChangeUnsupported));
    let p = plan_grow_at(&inv, &req(&["sdb"], None), NOW);
    assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::DiskInUse), "{:?}", p.issues);
    let p = plan_grow_at(&inv, &req(&[], None), NOW);
    assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::NoDisks));
    let mut r = req(&["sde"], None);
    r.volume = Some("vol9".into());
    assert!(plan_grow_at(&inv, &r, NOW).issues.iter().any(|i| i.code == PlanIssueCode::NoSuchArray));
    // a degraded tier, an interrupted growth
    let p = plan_grow_at(&flex(FlexState::DiskFaulty), &req(&["sdd"], None), NOW);
    assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::ArrayNotHealthy));
    let mut inv3 = inv.clone();
    inv3.lvm.vgs[0].free = 8 << 30;
    inv3.flex = crate::flex::detect::detect(&inv3, None, MIN_TIER);
    let p = plan_grow_at(&inv3, &req(&["sde"], None), NOW);
    assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::FlexGrowthPending));
    // a layout made elsewhere is read-only
    let mut inv4 = inv.clone();
    inv4.flex[0].managed = false;
    assert!(plan_grow_at(&inv4, &req(&["sde"], None), NOW).issues.iter().any(|i| i.code == PlanIssueCode::FlexLayoutUnsupported));
    // the records known but this volume not recorded: read-only
    let mut inv5 = inv.clone();
    inv5.records = Some(Vec::new());
    assert!(plan_grow_at(&inv5, &req(&["sde"], None), NOW).issues.iter().any(|i| i.code == PlanIssueCode::FlexLayoutUnsupported));
}

fn set_scrub(inv: &mut Inventory, started: &str, uncorrectable: u64) {
    let fs: &mut BtrfsFilesystem = inv.filesystems.iter_mut().find(|f| f.mountpoint.as_deref() == Some("/vol2")).unwrap();
    fs.scrub = Some(ScrubStatus {
        status: ScrubState::Finished,
        started: Some(started.into()),
        errors: uncorrectable,
        uncorrectable: Some(uncorrectable),
        corrected: Some(0),
        error_summary: if uncorrectable > 0 { "csum=2".into() } else { "no errors found".into() },
        ..Default::default()
    });
}

/// With the records known, a tier missing from them is recorded first (before anything else
/// about it), and the planner records new tiers too.
#[test]
fn records_known() {
    let mut inv = flex(FlexState::Healthy);
    add_blank(&mut inv, "sde", 12 * TB);
    add_blank(&mut inv, "sdf", 12 * TB);
    let u1 = "4f1c2a9e:7b3d5e60:81a2c3d4:e5f60718";
    inv.arrays.iter_mut().find(|a| a.name == "md126").unwrap().uuid = Some(u1.into());
    inv.records = Some(vec![VolumeRecord {
        label: "vol2".into(),
        level: RaidLevel::Flex1,
        tiers: vec![TierRecord { index: 1, md_uuid: u1.into() }],
        vg: Some("rynas_vol2".into()),
        fs_uuid: "x".into(),
        mountpoint: "/vol2".into(),
        options: "noatime".into(),
        ..Default::default()
    }]);
    inv.flex = crate::flex::detect::detect(&inv, inv.records.as_deref(), MIN_TIER);
    let p = plan_complete(&inv, &CompletePlanRequest { volume: "vol2".into() });
    assert_eq!(lines(&p.steps), ["config /etc/mdadm/mdadm.conf"]);
    let PlanAction::Config { entry, .. } = &p.steps[0].action else { panic!() };
    assert_eq!(entry, "ARRAY /dev/md/vol2_t2 metadata=1.2 UUID=<from mdadm --detail --export /dev/md127>");
}

#[test]
fn scrub_dates() {
    assert_eq!(scrub_epoch("Thu Jan  1 00:00:10 1970"), Some(10));
    assert_eq!(scrub_epoch("Thu Oct 22 15:35:01 2026"), Some(1_792_683_301));
    assert_eq!(scrub_epoch("garbage"), None);
}

// ------------------------------------------------------------------------------- recoveries

/// The steps clearing a replaced disk name it as erased (the UI warns, the executor checks it
/// is still that disk) and are best effort; the other steps are neither.
#[test]
fn the_replaced_disk_is_erased_best_effort() {
    let start = created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1);
    let mut v = start.clone();
    v.disks.push(VDisk { name: "sdd".into(), size: 12 * TB, parts: Vec::new(), others: 0 });
    let g = tiers::replace(&v.shapes(), &v.layout_disks(), "sda", &("sdd".into(), 12 * TB), 1, MIN_TIER).unwrap();
    let goal = Goal { old: Some("sda".into()), new: vec!["sdd".into()] };
    let steps = growth_steps(&mut v, &goal, Some(&g.tiers)).unwrap();
    let n = steps.len();
    assert_eq!(lines(&steps[n - 2..]), ["mdadm --zero-superblock /dev/sda1", "sgdisk --zap-all /dev/sda"]);
    for s in &steps[n - 2..] {
        assert_eq!(s.destroys, ["sda"]);
        assert!(s.best_effort);
    }
    assert!(steps[..n - 2].iter().all(|s| !s.best_effort && !s.destroys.contains(&"sda".to_string())));
}

/// Replaces "missing" the way plan_replace does: a ghost disk in every degraded tier.
fn replace_missing(v: &View, new: (&str, u64)) -> (Goal, Vec<Tier>) {
    let mut shapes = v.shapes();
    let mut disks = v.layout_disks();
    let degraded: Vec<u32> = v.tiers.iter().filter(|t| t.created() && t.degraded() > 0).map(|t| t.index).collect();
    let mut top = crate::plan::PART_START;
    for t in shapes.iter_mut().filter(|t| degraded.contains(&t.index)) {
        t.members.push("<missing>".into());
        top = top.max(t.end);
    }
    disks.push(("<missing>".into(), top + END_RESERVE));
    let g = tiers::replace(&shapes, &disks, "<missing>", &(new.0.into(), new.1), v.tolerance, MIN_TIER).unwrap();
    (Goal { old: Some(MISSING.into()), new: vec![new.0.into()] }, g.tiers)
}

/// A 2-disk tolerance tier missing two disks gets one back: the rebuild ends with one slot
/// still empty, and the wait says so (it would never end waiting for none).
#[test]
fn a_rebuild_that_leaves_the_tier_degraded_waits_for_that() {
    let mut start = created_view(&[("sda", 8), ("sdb", 8), ("sdc", 8), ("sdd", 8)], 2);
    for t in &mut start.tiers {
        t.members.retain(|m| m.disk != "sdc" && m.disk != "sdd");
    }
    start.disks.retain(|d| d.name != "sdc" && d.name != "sdd");
    start.disks.push(VDisk { name: "sde".into(), size: 8 * TB, parts: Vec::new(), others: 0 });
    let (goal, target) = replace_missing(&start, ("sde", 8 * TB));
    let mut end = start.clone();
    let steps = growth_steps(&mut end, &goal, Some(&target)).unwrap();
    kinds_ok(&steps);
    assert_eq!(
        lines(&steps)[4..],
        [
            "mdadm /dev/md/vol2_t1 --add /dev/sde1",
            "wait /sys/block/md101/md/sync_action == idle, degraded <= 1",
        ]
    );
    assert_eq!(end.tiers[0].degraded(), 1);
    converges(&start, &goal, &target, &steps, &end, true);
    // complete on a degraded tier: an interrupted reshape is continued and waited for with
    // the slot that stays empty
    let mut v = created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1);
    v.tiers[0].members.retain(|m| m.disk != "sdb");
    v.disks.retain(|d| d.name != "sdb");
    v.tiers[0].interrupted = true;
    let steps = growth_steps(&mut v, &Goal::default(), None).unwrap();
    assert_eq!(
        lines(&steps),
        [
            "mdadm --grow --continue /dev/md/vol2_t1",
            "wait /sys/block/md101/md/sync_action == idle, degraded <= 1",
        ]
    );
}

/// A disk that was a member once comes back with its old slices (in no array): added, it is
/// cleared (its old superblocks first) and used; the volume's own disks never are.
#[test]
fn a_disk_with_leftover_slices_is_cleared_and_used() {
    let start = created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1);
    let (b1, b2) = (reach(4 * TB), reach(8 * TB));
    let mut v = start.clone();
    v.disks.push(VDisk {
        name: "sdd".into(),
        size: 8 * TB,
        parts: vec![
            VPart { tier: 1, kname: "sdd1".into(), start: MIB, end: b1 - 64 * MIB, fresh: false, partn: 1 },
            VPart { tier: 2, kname: "sdd2".into(), start: b1 - 64 * MIB, end: b2, fresh: false, partn: 2 },
        ],
        others: 0,
    });
    let ns = vec![("sdd".to_string(), 8 * TB)];
    let growth = tiers::grow(&v.shapes(), &v.layout_disks(), &ns, 1, MIN_TIER);
    let goal = Goal { old: None, new: vec!["sdd".into()] };
    let begin = v.clone();
    let steps = growth_steps(&mut v, &goal, Some(&growth.tiers)).unwrap();
    kinds_ok(&steps);
    let l = lines(&steps);
    assert_eq!(
        l[..5],
        [
            "wipefs --all /dev/sdd1".to_string(),
            "wipefs --all /dev/sdd2".into(),
            "wipefs --all /dev/sdd".into(),
            "sgdisk --zap-all /dev/sdd".into(),
            format!("sgdisk {} {} /dev/sdd", new_arg(1, MIB, b1), new_arg(2, b1, b2)),
        ]
    );
    assert!(steps[..4].iter().all(|s| s.destroys == ["sdd"]));
    assert!(l.contains(&"mdadm /dev/md/vol2_t1 --add /dev/sdd1".to_string()), "{l:?}");
    assert!(l.contains(&"mdadm /dev/md/vol2_t2 --add /dev/sdd2".to_string()), "{l:?}");
    converges(&begin, &goal, &growth.tiers, &steps, &v, true);
    // a member disk named as new is never cleared
    let mut w = start.clone();
    let target: Vec<Tier> = w.shapes();
    let steps = partition_steps(&mut w, &target, &["sdb".into()]).unwrap();
    assert!(steps.is_empty(), "{:?}", lines(&steps));
}

/// "missing": the disk gone and back (its slices in no array) is the new disk: cleared and
/// rebuilt on; another disk takes its place: the old one is never erased (the request does
/// not name it; the plan warns about it, see `replace_missing_warns_about_a_leftover_disk`).
#[test]
fn replace_missing_with_the_old_disk_still_attached() {
    let mut start = created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1);
    for t in &mut start.tiers {
        t.members.retain(|m| m.disk != "sdb");
    }
    for p in &mut start.disks.iter_mut().find(|d| d.name == "sdb").unwrap().parts {
        p.fresh = false;
    }
    // the same disk back
    let (goal, target) = replace_missing(&start, ("sdb", 8 * TB));
    let mut end = start.clone();
    let steps = growth_steps(&mut end, &goal, Some(&target)).unwrap();
    let l = lines(&steps);
    assert_eq!(l[..4], ["wipefs --all /dev/sdb1", "wipefs --all /dev/sdb2", "wipefs --all /dev/sdb", "sgdisk --zap-all /dev/sdb"]);
    assert!(l.contains(&"mdadm /dev/md/vol2_t1 --add /dev/sdb1".to_string()), "{l:?}");
    assert!(end.tiers.iter().all(|t| t.degraded() == 0));
    // another disk
    let mut v = start.clone();
    v.disks.push(VDisk { name: "sdd".into(), size: 8 * TB, parts: Vec::new(), others: 0 });
    let (goal, target) = replace_missing(&v, ("sdd", 8 * TB));
    let begin = v.clone();
    let steps = growth_steps(&mut v, &goal, Some(&target)).unwrap();
    assert!(
        !steps.iter().any(|s| line(s).contains("/dev/sdb") || s.destroys.contains(&"sdb".to_string())),
        "{:?}",
        lines(&steps)
    );
    assert!(v.tiers.iter().all(|t| t.degraded() == 0));
    converges(&begin, &goal, &target, &steps, &v, false);
}

/// A crash between taking the third mirror out and clearing it: complete clears it and goes
/// on to RAID6 (the rule does not need a tier below: at 2-disk tolerance tier 1 has every
/// disk, four at least, so only an upper tier is a three-way mirror).
#[test]
fn complete_a_three_way_mirror_stopped_after_its_mirror_went_out() {
    let start = created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8), ("sdd", 8)], 2);
    let Grown { begin, steps, end, .. } = grow_view(&start, &[("sde", 8)]);
    let cut = lines(&steps).iter().position(|l| l == "mdadm /dev/md/vol2_t2 --remove /dev/sdd2").unwrap() + 1;
    let mut crashed = begin.clone();
    for s in &steps[..cut] {
        crashed.apply(s);
    }
    // the inventory after the crash: the partition still has the superblock
    for p in crashed.disks.iter_mut().flat_map(|d| d.parts.iter_mut()).filter(|p| p.kname == "sdd2") {
        p.fresh = false;
    }
    let mut w = crashed.clone();
    let got = growth_steps(&mut w, &Goal::default(), None).unwrap();
    let l = lines(&got);
    assert_eq!(
        l[..3],
        [
            "mdadm --zero-superblock /dev/sdd2",
            "mdadm --grow /dev/md/vol2_t2 --raid-devices=2",
            "mdadm --grow /dev/md/vol2_t2 --level=5",
        ]
    );
    let t = w.tiers.iter().find(|t| t.index == 2).unwrap();
    assert_eq!((t.level, t.raid_disks, t.active()), (Some(RaidLevel::Raid6), 4, 4), "{l:?}");
    assert_eq!(shape(&w), shape(&end));
    // the same with the superblock already cleared: no rebuild back to three mirrors
    for p in crashed.disks.iter_mut().flat_map(|d| d.parts.iter_mut()).filter(|p| p.kname == "sdd2") {
        p.fresh = true;
    }
    let mut w = crashed.clone();
    let l = lines(&growth_steps(&mut w, &Goal::default(), None).unwrap());
    assert_eq!(l[0], "mdadm --grow /dev/md/vol2_t2 --raid-devices=2", "{l:?}");
    assert_eq!(shape(&w), shape(&end));
    // a three-way mirror that lost a mirror with no new partition around is left alone
    let mut v = start.clone();
    let t2 = v.tiers.iter_mut().find(|t| t.index == 2).unwrap();
    t2.members.retain(|m| m.part != "sdd2");
    for p in v.disks.iter_mut().flat_map(|d| d.parts.iter_mut()).filter(|p| p.kname == "sdd2") {
        p.fresh = false;
    }
    let l = lines(&growth_steps(&mut v, &Goal::default(), None).unwrap());
    assert!(!l.iter().any(|x| x.contains("--raid-devices=2")), "{l:?}");
}

/// A new tier is never created over a partition carrying an md superblock.
#[test]
fn a_new_tier_refuses_a_partition_with_a_superblock() {
    let start = created_view(&[("sda", 4), ("sdb", 4), ("sdc", 8), ("sdd", 8)], 2);
    let (b1, b2) = (reach(4 * TB), reach(8 * TB));
    let mut v = start.clone();
    v.disks.iter_mut().find(|d| d.name == "sdc").unwrap().parts.push(VPart {
        tier: 2,
        kname: "sdc2".into(),
        start: b1,
        end: b2,
        fresh: false,
        partn: 2,
    });
    v.disks.push(VDisk { name: "sde".into(), size: 12 * TB, parts: Vec::new(), others: 0 });
    let ns = vec![("sde".to_string(), 12 * TB)];
    let growth = tiers::grow(&v.shapes(), &v.layout_disks(), &ns, 2, MIN_TIER);
    let goal = Goal { old: None, new: vec!["sde".into()] };
    let e = growth_steps(&mut v, &goal, Some(&growth.tiers)).unwrap_err();
    assert!(e.contains("sdc2: carries an md superblock"), "{e}");
}

/// A crash while the disks in use got their partitions for a new top tier: completing with
/// the growth recorded (its new disk named) makes the missing ones before the array, over
/// every disk the tier was planned on. Without that record the new disk, in no tier yet, is
/// never adopted: no tier is made over its slices.
#[test]
fn complete_the_partitions_of_a_new_tier() {
    let start = created_view(&[("sda", 4), ("sdc", 4), ("sdd", 8), ("sde", 8)], 2);
    let Grown { begin, steps, end, .. } = grow_view(&start, &[("sdb", 12)]);
    let (b1, b2) = (reach(4 * TB), reach(8 * TB));
    let l = lines(&steps);
    let cut = l.iter().position(|x| x == "partx --update --nr 2 /dev/sdd").unwrap() + 1;
    assert!(l[cut].starts_with("sgdisk") && l[cut].ends_with("/dev/sde"), "{l:?}");
    let mut w = begin.clone();
    for s in &steps[..cut] {
        w.apply(s);
    }
    let mut silent = w.clone();
    let got = growth_steps(&mut silent, &Goal::default(), None).unwrap();
    assert!(!lines(&got).iter().any(|l| l.contains("--create") || l.contains("/dev/sdb")), "{:?}", lines(&got));
    let named = Goal { old: None, new: vec!["sdb".into()] };
    let got = growth_steps(&mut w, &named, None).unwrap();
    let g = lines(&got);
    assert_eq!(
        g[..3],
        [
            format!("sgdisk {} /dev/sde", new_arg(2, b1, b2)),
            "partx --update --nr 2 /dev/sde".into(),
            "udevadm settle".into(),
        ]
    );
    let create = g.iter().find(|x| x.starts_with("mdadm --create /dev/md/vol2_t2")).expect("tier 2 created");
    assert!(create.contains("--level=1 --raid-devices=3") && create.contains("/dev/sde2"), "{create}");
    assert_eq!(shape(&w), shape(&end));
}

/// A replace whose last step (the old disk's partition table) failed leaves that disk with
/// clean slices of the volume in no array. A later growth by another disk, and complete,
/// never touch it (they do not add its slices to a tier); a growth naming it clears it
/// first (the plan says it is erased); a replace of a missing disk leaves it alone.
#[test]
fn leftover_slices_without_superblocks_are_never_adopted() {
    let start = created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1);
    let mut v = start.clone();
    v.disks.push(VDisk { name: "sdd".into(), size: 12 * TB, parts: Vec::new(), others: 0 });
    let g = tiers::replace(&v.shapes(), &v.layout_disks(), "sda", &("sdd".into(), 12 * TB), 1, MIN_TIER).unwrap();
    let goal = Goal { old: Some("sda".into()), new: vec!["sdd".into()] };
    let steps = growth_steps(&mut v, &goal, Some(&g.tiers)).unwrap();
    assert_eq!(line(steps.last().unwrap()), "sgdisk --zap-all /dev/sda");
    // the superblock went, the partition table stayed: the inventory finds sda by its
    // partition label again, with a clean slice of tier 1
    let old_part = start.disks.iter().find(|d| d.name == "sda").unwrap().parts[0].clone();
    assert_eq!(old_part.tier, 1);
    v.disks.retain(|d| d.name != "sda");
    v.disks.push(VDisk { name: "sda".into(), size: 4 * TB, parts: vec![VPart { fresh: true, ..old_part }], others: 0 });
    let after_replace = v.clone();
    let touches_sda = |steps: &[PlanStep]| {
        steps.iter().any(|s| line(s).contains("/dev/sda") || s.destroys.contains(&"sda".to_string()))
    };

    // complete: nothing to do, sda is not added
    let mut w = after_replace.clone();
    let got = growth_steps(&mut w, &Goal::default(), None).unwrap();
    assert!(got.is_empty(), "{:?}", lines(&got));

    // growing by another disk: sda stays out of every tier
    let Grown { steps, end, .. } = grow_view(&after_replace, &[("sde", 8)]);
    assert!(!touches_sda(&steps), "{:?}", lines(&steps));
    assert!(lines(&steps).contains(&"mdadm /dev/md/vol2_t1 --add /dev/sde1".to_string()));
    assert!(!end.in_some_tier("sda"));

    // growing by sda itself: cleared like a blank disk, then used
    let mut w = after_replace.clone();
    let ns = vec![("sda".to_string(), 4 * TB)];
    let growth = tiers::grow(&w.shapes(), &w.layout_disks(), &ns, 1, MIN_TIER);
    let goal = Goal { old: None, new: vec!["sda".into()] };
    let begin = w.clone();
    let steps = growth_steps(&mut w, &goal, Some(&growth.tiers)).unwrap();
    kinds_ok(&steps);
    let l = lines(&steps);
    assert_eq!(l[..2], ["wipefs --all /dev/sda", "sgdisk --zap-all /dev/sda"], "{l:?}");
    assert!(steps[..2].iter().all(|s| s.destroys == ["sda"]));
    assert!(l.contains(&"mdadm /dev/md/vol2_t1 --add /dev/sda1".to_string()), "{l:?}");
    converges(&begin, &goal, &growth.tiers, &steps, &w, true);

    // a replace of a missing disk: sda is not named by the request, never erased
    let mut v = after_replace.clone();
    for t in &mut v.tiers {
        t.members.retain(|m| m.disk != "sdb");
    }
    v.disks.retain(|d| d.name != "sdb");
    v.disks.push(VDisk { name: "sdf".into(), size: 8 * TB, parts: Vec::new(), others: 0 });
    let (goal, target) = replace_missing(&v, ("sdf", 8 * TB));
    let steps = growth_steps(&mut v, &goal, Some(&target)).unwrap();
    assert!(!touches_sda(&steps), "{:?}", lines(&steps));
    assert!(v.tiers.iter().all(|t| t.degraded() == 0));
}

// ------------------------------------------------------------------------------- through the request checks

/// The inventory of the volume in model state `v`, as lsblk, mdadm, LVM and btrfs show it,
/// with the volume's record (and `change` recorded as under way): the planners' input, so a
/// state of the model goes through the request checks (plan_replace, plan_grow, complete).
fn inventory_of(v: &View, change: Option<FlexChange>) -> Inventory {
    use crate::lvm::{Lv, Pv, Vg};
    use rynas_api_types::storage::{BtrfsDevice, MdArray, MdMember, SyncProgress};
    let label = v.label.clone();
    let vg = vg_name(&label);
    let dm_path = format!("/dev/mapper/{vg}-{LV_NAME}");
    let fs_uuid = "11111111-2222-3333-4444-555555555555".to_string();
    let mut inv = Inventory::default();
    let member_of = |part: &str| {
        v.tiers
            .iter()
            .find(|t| t.created() && t.members.iter().any(|m| m.part == part))
    };
    for d in &v.disks {
        let mut disk = BlockDev {
            kname: d.name.clone(),
            name: d.name.clone(),
            path: dev(&d.name),
            kind: "disk".into(),
            size: d.size,
            rotational: true,
            serial: Some(format!("SER-{}", d.name)),
            model: Some("TEST HDD".into()),
            ..Default::default()
        };
        for p in &d.parts {
            let t = member_of(&p.kname);
            inv.graph.devs.insert(
                p.kname.clone(),
                BlockDev {
                    kname: p.kname.clone(),
                    name: p.kname.clone(),
                    path: dev(&p.kname),
                    kind: "part".into(),
                    size: p.end - p.start,
                    start: Some(p.start),
                    partn: Some(if p.partn == 0 { p.tier } else { p.partn }),
                    partlabel: Some(tier_partlabel(&label, p.tier)),
                    fstype: (!p.fresh || t.is_some()).then(|| "linux_raid_member".to_string()),
                    parents: vec![d.name.clone()],
                    children: t.map(|t| vec![t.kname.clone()]).unwrap_or_default(),
                    ..Default::default()
                },
            );
            disk.children.push(p.kname.clone());
        }
        for n in 0..d.others {
            let k = format!("{}{}", d.name, 9 + n);
            inv.graph.devs.insert(
                k.clone(),
                BlockDev {
                    kname: k.clone(),
                    name: k.clone(),
                    path: dev(&k),
                    kind: "part".into(),
                    size: MIB,
                    start: Some(d.size - (n as u64 + 2) * MIB),
                    partn: Some(9 + n),
                    partlabel: Some("other".into()),
                    parents: vec![d.name.clone()],
                    ..Default::default()
                },
            );
            disk.children.push(k);
        }
        inv.graph.devs.insert(d.name.clone(), disk);
    }
    let mut in_vg: Vec<String> = Vec::new();
    for t in v.tiers.iter().filter(|t| t.created()) {
        let kind = match t.level {
            Some(RaidLevel::Raid5) => "raid5",
            Some(RaidLevel::Raid6) => "raid6",
            _ => "raid1",
        };
        if t.in_vg {
            in_vg.push(t.kname.clone());
        }
        inv.graph.devs.insert(
            t.kname.clone(),
            BlockDev {
                kname: t.kname.clone(),
                name: t.kname.clone(),
                path: format!("/dev/{}", t.kname),
                kind: kind.into(),
                size: t.size,
                parents: t.members.iter().map(|m| m.part.clone()).collect(),
                children: if t.in_vg { vec!["dm-0".into()] } else { Vec::new() },
                ..Default::default()
            },
        );
        inv.arrays.push(MdArray {
            name: t.kname.clone(),
            path: t.dev.clone(),
            md_name: Some(format!("nas:{label}_t{}", t.index)),
            uuid: Some(format!("{:08x}:00000000:00000000:00000000", t.index)),
            level: kind.into(),
            raid_disks: t.raid_disks,
            size: t.size,
            degraded: t.degraded(),
            sync: t.sync.as_ref().map(|a| SyncProgress { action: a.clone(), ..Default::default() }),
            bitmap: Some(if t.bitmap { "internal" } else { "none" }.into()),
            reshape_position: t.interrupted.then_some(1),
            members: t
                .members
                .iter()
                .map(|m| MdMember {
                    device: m.part.clone(),
                    disk: Some(m.disk.clone()),
                    role: m.role,
                    // sysfs state of a copy under way
                    state: if t.replacing.as_deref() == Some(m.part.as_str()) {
                        "in_sync,want_replacement".into()
                    } else if t.replacing.is_some() && m.role == MemberRole::Rebuilding {
                        "spare,replacement".into()
                    } else {
                        String::new()
                    },
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
        if let Some(pv) = t.pv {
            inv.lvm.pvs.push(Pv {
                name: t.dev.clone(),
                vg: if t.in_vg { vg.clone() } else { String::new() },
                size: pv,
                dev_size: t.size,
                ..Default::default()
            });
        }
    }
    if v.vg_exists {
        inv.lvm.vgs.push(Vg {
            name: vg.clone(),
            size: inv.lvm.pvs.iter().filter(|p| p.vg == vg).map(|p| p.size).sum(),
            free: v.vg_free,
            extent_size: v.extent,
            pv_count: in_vg.len() as u32,
            lv_count: 1,
            ..Default::default()
        });
    }
    if v.lv_exists {
        inv.lvm.lvs.push(Lv {
            name: LV_NAME.into(),
            vg: vg.clone(),
            size: v.lv_size,
            dm_path: dm_path.clone(),
            ..Default::default()
        });
        inv.graph.devs.insert(
            "dm-0".into(),
            BlockDev {
                kname: "dm-0".into(),
                name: format!("{vg}-{LV_NAME}"),
                path: dm_path.clone(),
                kind: "lvm".into(),
                size: v.lv_size,
                fstype: Some("btrfs".into()),
                label: Some(label.clone()),
                uuid: Some(fs_uuid.clone()),
                parents: in_vg,
                ..Default::default()
            },
        );
        if let Some((mnt, used)) = &v.fs {
            inv.filesystems.push(BtrfsFilesystem {
                uuid: fs_uuid.clone(),
                label: Some(label.clone()),
                mountpoint: Some(mnt.clone()),
                mountpoints: vec![mnt.clone()],
                devices: vec![BtrfsDevice { devid: 1, path: dm_path.clone(), size: *used, ..Default::default() }],
                ..Default::default()
            });
        }
    }
    let names: Vec<String> = inv.graph.disks().iter().map(|d| d.kname.clone()).collect();
    for n in names {
        let d = inv.graph.get(&n).unwrap();
        let s = crate::inventory::summary(d, &inv.graph, None, None, PowerState::Active, NOW * 1000, false);
        inv.disks.push(s);
    }
    inv.records = Some(vec![VolumeRecord {
        label: label.clone(),
        level: RaidLevel::flexible(v.tolerance).unwrap(),
        tiers: v
            .tiers
            .iter()
            .filter(|t| t.created() && t.recorded)
            .map(|t| TierRecord { index: t.index, md_uuid: format!("{:08x}:00000000:00000000:00000000", t.index) })
            .collect(),
        vg: Some(vg),
        fs_uuid,
        mountpoint: format!("/{label}"),
        options: "noatime".into(),
        change,
        ..Default::default()
    }]);
    inv.flex = crate::flex::detect::detect(&inv, inv.records.as_deref(), MIN_TIER);
    inv
}

/// The view of a created volume over `disks` (TB) plus blank disks `extra`, and the change
/// record of a request on it.
fn with_blank(v: &View, extra: &[(&str, u64)]) -> View {
    let mut v = v.clone();
    for (n, s) in extra {
        v.disks.push(VDisk { name: n.to_string(), size: s * TB, ..Default::default() });
    }
    v
}

fn prev(steps: &[PlanStep], i: usize) -> String {
    i.checked_sub(1).and_then(|j| steps.get(j)).map(line).unwrap_or_default()
}

/// The steps of `plan` (valid) run from `v` reach the end state `end`.
fn reaches(v: &View, plan: &StoragePlan, end: &View, what: &str) {
    assert!(plan.valid, "{what}: {:?}", plan.errors);
    let mut w = v.clone();
    for s in &plan.steps {
        w.apply(s);
    }
    assert_eq!(shape(&w), shape(end), "{what}: {:?}", lines(&plan.steps));
    assert_eq!(w.lv_size, end.lv_size, "{what}");
}

/// A change interrupted after any of its steps, the volume read again: the same request and
/// complete both pass the request checks and end where the uninterrupted plan ends (the
/// change recorded before its first step). Returns the end state.
fn resumes(start: &View, change: FlexChange, request: &dyn Fn(&Inventory) -> StoragePlan) -> View {
    let first = request(&inventory_of(start, None));
    assert!(first.valid, "{:?}", first.errors);
    kinds_ok(&first.steps);
    let steps = first.steps.clone();
    let mut end = start.clone();
    for s in &steps {
        end.apply(s);
    }
    assert!(growth_steps(&mut end.clone(), &Goal::default(), None).unwrap().is_empty(), "left after the plan");
    for i in 0..=steps.len() {
        let mut v = start.clone();
        for s in &steps[..i] {
            v.apply(s);
        }
        let inv = inventory_of(&v, Some(change.clone()));
        let what = format!("after step {i} ({})", prev(&steps, i));
        if i < steps.len() {
            reaches(&v, &request(&inv), &end, &format!("requested again {what}"));
        }
        let c = plan_complete_at(&inv, &CompletePlanRequest { volume: "vol2".into() }, NOW);
        if recorded_change(&inv, "vol2").is_some() {
            reaches(&v, &c, &end, &format!("complete {what}"));
            assert!(c.steps.iter().all(|s| s.kind != Some(PlanStepKind::Other)));
        } else {
            assert!(c.valid, "complete {what}: {:?}", c.errors);
        }
    }
    end
}

fn grow_req(disks: &[&str]) -> GrowPlanRequest {
    GrowPlanRequest {
        array: String::new(),
        disks: disks.iter().map(|s| s.to_string()).collect(),
        level: None,
        volume: Some("vol2".into()),
    }
}

/// A hot replace (the old disk works) stopped at any step, e.g. after the old disk's first
/// partition left tier 1 (the reported case: re-requesting was refused, complete grew the
/// volume instead and the old disk never left): requested again or completed, it goes on to
/// its end, the old disk cleared included.
#[test]
fn a_hot_replace_resumes_from_any_step() {
    let start = with_blank(&created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1), &[("sdd", 12)]);
    for old in ["sdb", "sda"] {
        let inv = inventory_of(&start, None);
        let change = change_record(&inv, Some(old), &["sdd".into()]);
        let end = resumes(&start, change, &|inv| plan_replace_at(inv, &replace_req(old, "sdd"), NOW));
        assert!(end.tiers.iter().all(|t| t.members.iter().all(|m| m.disk != old)), "{old}: {:?}", shape(&end));
        assert!(end.disks.iter().find(|d| d.name == old).is_none_or(|d| d.parts.is_empty()), "{old} not cleared");
    }
    // 2-disk tolerance: a member of the three-way mirror
    let start = with_blank(&created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8), ("sdd", 8)], 2), &[("sde", 8)]);
    let change = change_record(&inventory_of(&start, None), Some("sdc"), &["sde".into()]);
    resumes(&start, change, &|inv| plan_replace_at(inv, &replace_req("sdc", "sde"), NOW));
}

/// A replace of a failed disk, and of a missing one, stopped at any step: the same.
#[test]
fn a_rebuild_resumes_from_any_step() {
    let mut start = with_blank(&created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1), &[("sdd", 8)]);
    for t in &mut start.tiers {
        for m in t.members.iter_mut().filter(|m| m.disk == "sdb") {
            m.role = MemberRole::Faulty;
        }
    }
    let change = change_record(&inventory_of(&start, None), Some("sdb"), &["sdd".into()]);
    resumes(&start, change, &|inv| plan_replace_at(inv, &replace_req("sdb", "sdd"), NOW));
    // gone: its members out of the arrays, the disk detached
    let mut start = start.clone();
    for t in &mut start.tiers {
        t.members.retain(|m| m.disk != "sdb");
    }
    start.disks.retain(|d| d.name != "sdb");
    let change = change_record(&inventory_of(&start, None), Some(MISSING), &["sdd".into()]);
    resumes(&start, change, &|inv| plan_replace_at(inv, &replace_req(MISSING, "sdd"), NOW));
}

/// A growth stopped at any step (a new top tier on disks in use, a three-way mirror on its
/// way to RAID6 stopped between `--fail` and `--remove` included): the same request again, or
/// complete, goes on to the end.
#[test]
fn a_growth_resumes_from_any_step() {
    for (disks, tolerance, new) in [
        (&[("sda", 4), ("sdb", 8), ("sdc", 8)][..], 1, ("sdd", 12)),
        (&[("sda", 4), ("sdb", 8), ("sdc", 8), ("sdd", 8)][..], 2, ("sde", 8)),
        (&[("sda", 4), ("sdb", 4), ("sdc", 8), ("sdd", 8)][..], 2, ("sde", 12)),
    ] {
        let start = with_blank(&created_view(disks, tolerance), &[new]);
        let change = change_record(&inventory_of(&start, None), None, &[new.0.into()]);
        resumes(&start, change, &|inv| plan_grow_at(inv, &grow_req(&[new.0]), NOW));
    }
}

/// A change recorded as under way blocks other changes (the same request or complete goes
/// on with it); one that never started (no new disk holds a slice) is ignored.
#[test]
fn a_recorded_change_blocks_other_changes() {
    let start = with_blank(&created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1), &[("sdd", 12), ("sde", 12)]);
    let inv0 = inventory_of(&start, None);
    let change = change_record(&inv0, Some("sdb"), &["sdd".into()]);
    let p = plan_replace_at(&inv0, &replace_req("sdb", "sdd"), NOW);
    let mut v = start.clone();
    for s in &p.steps[..6] {
        v.apply(s);
    }
    let inv = inventory_of(&v, Some(change.clone()));
    assert!(inv.flex[0].pending.iter().any(|p| p.kind == FlexPendingKind::ChangeInterrupted));
    for p in [
        plan_replace_at(&inv, &replace_req("sdb", "sde"), NOW),
        plan_replace_at(&inv, &replace_req("sdc", "sdd"), NOW),
        plan_grow_at(&inv, &grow_req(&["sde"]), NOW),
    ] {
        assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::FlexGrowthPending), "{:?}", p.issues);
        assert!(p.errors.iter().any(|e| e.contains("the replace of sdb by sdd stopped half way")), "{:?}", p.errors);
    }
    // never started: ignored by the plans (complete then clears the record)
    let inv = inventory_of(&start, Some(change));
    assert!(recorded_change(&inv, "vol2").is_none());
    let p = plan_complete_at(&inv, &CompletePlanRequest { volume: "vol2".into() }, NOW);
    assert!(p.valid && p.steps.is_empty(), "{:?}", lines(&p.steps));
}

/// "missing" never erases a disk the request does not name: a disk with slices of the volume
/// in no tier is left alone, and the plan says so.
#[test]
fn replace_missing_warns_about_a_leftover_disk() {
    let mut v = with_blank(&created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1), &[("sdd", 8)]);
    for t in &mut v.tiers {
        t.members.retain(|m| m.disk != "sdb");
    }
    for p in &mut v.disks.iter_mut().find(|d| d.name == "sdb").unwrap().parts {
        p.fresh = false;
    }
    let inv = inventory_of(&v, None);
    let p = plan_replace_at(&inv, &replace_req(MISSING, "sdd"), NOW);
    assert!(p.valid, "{:?}", p.errors);
    assert!(
        p.warning_items.iter().any(|w| w.code == PlanWarningCode::FlexLeftoverDisk && w.args.subject.as_deref() == Some("sdb")),
        "{:?}",
        p.warnings
    );
    assert!(!p.steps.iter().any(|s| line(s).contains("/dev/sdb") || !s.destroys.iter().all(|d| d == "sdd" || d.starts_with("sdd"))), "{:?}", lines(&p.steps));
}

/// A replaced disk with partitions of something else keeps them: only the volume's are
/// deleted, the partition table stays.
#[test]
fn a_replaced_disk_with_other_partitions_keeps_them() {
    let mut start = with_blank(&created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1), &[("sdd", 12)]);
    start.disks.iter_mut().find(|d| d.name == "sda").unwrap().others = 1;
    let inv = inventory_of(&start, None);
    let p = plan_replace_at(&inv, &replace_req("sda", "sdd"), NOW);
    assert!(p.valid, "{:?}", p.errors);
    kinds_ok(&p.steps);
    let last = p.steps.last().unwrap();
    assert_eq!(line(last), "sgdisk --delete=1 /dev/sda");
    assert_eq!(last.kind, Some(PlanStepKind::DeletePartitions));
    assert_eq!(last.args.items, ["sda1"]);
    assert!(last.best_effort && last.destroys == ["sda"]);
    assert!(!lines(&p.steps).iter().any(|l| l.contains("--zap-all /dev/sda")));
    // the model: the slices go, the disk stays
    let mut w = start.clone();
    for s in &p.steps {
        w.apply(s);
    }
    assert!(w.disks.iter().find(|d| d.name == "sda").unwrap().parts.is_empty());
}

/// An array of the volume's names that is not the volume's (another system's disk, the same
/// label) is never taken for a tier, nor are slices on a disk in no tier; a new label may not
/// take the names of a flexible volume's arrays or volume group.
#[test]
fn foreign_arrays_and_slices_are_not_the_volumes() {
    let start = created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1);
    let mut inv = inventory_of(&start, None);
    // another system's disk with a "vol2_t3" array on slices labeled for tier 3
    let (b1, b2) = (reach(4 * TB), reach(8 * TB));
    let mut x = with_blank(&View::blank("vol2", 1, &[]), &[("sdx", 12), ("sdy", 12)]);
    for d in x.disks.iter_mut() {
        d.parts.push(VPart { tier: 3, kname: format!("{}3", d.name), start: b2, end: b2 + (b2 - b1), fresh: false, partn: 3 });
    }
    let xi = inventory_of(&x, None);
    for n in ["sdx", "sdy", "sdx3", "sdy3"] {
        inv.graph.devs.insert(n.into(), xi.graph.get(n).unwrap().clone());
    }
    inv.disks.extend(xi.disks.iter().cloned());
    inv.arrays.push(rynas_api_types::storage::MdArray {
        name: "md90".into(),
        path: "/dev/md90".into(),
        md_name: Some("othernas:vol2_t3".into()),
        level: "raid1".into(),
        raid_disks: 2,
        members: ["sdx", "sdy"]
            .iter()
            .map(|d| rynas_api_types::storage::MdMember { device: format!("{d}3"), disk: Some(d.to_string()), role: MemberRole::Active, ..Default::default() })
            .collect(),
        ..Default::default()
    });
    for d in ["sdx3", "sdy3"] {
        inv.graph.devs.get_mut(d).unwrap().children = vec!["md90".into()];
    }
    inv.graph.devs.insert("md90".into(), BlockDev { kname: "md90".into(), name: "md90".into(), path: "/dev/md90".into(), kind: "raid1".into(), parents: vec!["sdx3".into(), "sdy3".into()], ..Default::default() });
    inv.flex = crate::flex::detect::detect(&inv, inv.records.as_deref(), MIN_TIER);
    assert!(inv.flex[0].pending.is_empty(), "{:?}", inv.flex[0].pending);
    let v = View::from_inventory(&inv, &inv.flex[0], "vol2");
    assert_eq!(v.tiers.len(), 2);
    let p = plan_complete_at(&inv, &CompletePlanRequest { volume: "vol2".into() }, NOW);
    assert!(p.valid && p.steps.is_empty(), "{:?}", lines(&p.steps));
    // its slices without an array are not planned into a tier either
    inv.arrays.retain(|a| a.name != "md90");
    inv.graph.devs.remove("md90");
    for d in ["sdx3", "sdy3"] {
        inv.graph.devs.get_mut(d).unwrap().children.clear();
        inv.graph.devs.get_mut(d).unwrap().fstype = None;
    }
    inv.flex = crate::flex::detect::detect(&inv, inv.records.as_deref(), MIN_TIER);
    let p = plan_complete_at(&inv, &CompletePlanRequest { volume: "vol2".into() }, NOW);
    assert!(p.valid && p.steps.is_empty(), "{:?}", lines(&p.steps));
    // names of a flexible volume are taken
    let mut inv = blank_inventory(&[("sde", 4 * TB), ("sdf", 4 * TB)]);
    let mut req = create_req(&["sde", "sdf"], RaidLevel::Flex1, false);
    req.label = "store".into();
    let p = crate::plan::plan_volume(&inv, &req);
    assert!(p.valid, "{:?}", p.errors);
    inv.arrays.push(rynas_api_types::storage::MdArray { name: "md80".into(), md_name: Some("nas:store_t2".into()), ..Default::default() });
    assert!(crate::plan::plan_volume(&inv, &req).issues.iter().any(|i| i.code == PlanIssueCode::LabelTaken));
    inv.arrays.clear();
    inv.lvm.vgs.push(crate::lvm::Vg { name: "rynas_store".into(), ..Default::default() });
    assert!(crate::plan::plan_volume(&inv, &req).issues.iter().any(|i| i.code == PlanIssueCode::LabelTaken));
    // "store_t2x" or "storex_t1" are other names
    inv.lvm.vgs.clear();
    inv.arrays.push(rynas_api_types::storage::MdArray { name: "md80".into(), md_name: Some("nas:store_t2x".into()), ..Default::default() });
    inv.arrays.push(rynas_api_types::storage::MdArray { name: "md81".into(), md_name: Some("nas:storex_t1".into()), ..Default::default() });
    assert!(crate::plan::plan_volume(&inv, &req).valid);
}

/// Without the records the tolerance comes from the tiers, never lowered by a three-way
/// mirror half way to RAID6 (a RAID5 of two disks for a while): complete finishes it as RAID6.
#[test]
fn the_tolerance_without_records_is_never_lowered() {
    let start = with_blank(&created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8), ("sdd", 8)], 2), &[("sde", 8)]);
    let p = plan_grow_at(&inventory_of(&start, None), &grow_req(&["sde"]), NOW);
    let cut = lines(&p.steps).iter().position(|l| l == "mdadm --grow /dev/md/vol2_t2 --level=5").unwrap() + 1;
    let mut v = start.clone();
    for s in &p.steps[..cut] {
        v.apply(s);
    }
    let mut inv = inventory_of(&v, None);
    inv.records = None;
    inv.flex = crate::flex::detect::detect(&inv, None, MIN_TIER);
    assert_eq!(inv.flex[0].tolerance, 1, "the layout alone reads 1");
    let w = View::from_inventory(&inv, &inv.flex[0], "vol2");
    assert_eq!(w.tolerance, 2);
    let mut end = v.clone();
    for s in &p.steps[cut..] {
        end.apply(s);
    }
    let mut w2 = v.clone();
    w2.tolerance = w.tolerance;
    let named = Goal { old: None, new: vec!["sde".into()] };
    growth_steps(&mut w2, &named, None).unwrap();
    let t = w2.tiers.iter().find(|t| t.index == 2).unwrap();
    assert_eq!((t.level, t.raid_disks), (Some(RaidLevel::Raid6), 4));
    assert_eq!(shape(&w2), shape(&end));
}

/// A three-way mirror on its way to RAID6 stopped between `--fail` and `--remove`: complete
/// removes it and goes on, instead of rebuilding the mirror to take it out again.
#[test]
fn complete_a_three_way_mirror_stopped_after_fail() {
    let start = created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8), ("sdd", 8)], 2);
    let Grown { begin, steps, end, goal, .. } = grow_view(&start, &[("sde", 8)]);
    let cut = lines(&steps).iter().position(|l| l == "mdadm /dev/md/vol2_t2 --fail /dev/sdd2").unwrap() + 1;
    let mut v = begin.clone();
    for s in &steps[..cut] {
        v.apply(s);
    }
    let got = growth_steps(&mut v.clone(), &goal, None).unwrap();
    let l = lines(&got);
    assert_eq!(l[..3], ["mdadm /dev/md/vol2_t2 --remove /dev/sdd2", "mdadm --zero-superblock /dev/sdd2", "mdadm --grow /dev/md/vol2_t2 --raid-devices=2"], "{l:?}");
    assert!(!l.iter().any(|x| x.contains("--add /dev/sde2") && x.contains("vol2_t2") && !x.contains("sdd2")), "{l:?}");
    for s in &got {
        v.apply(s);
    }
    assert_eq!(shape(&v), shape(&end));
}

/// A replace that makes a new top tier rebuilds the degraded tiers first: the new tier comes
/// after the rebuilds end.
#[test]
fn rebuilds_come_before_a_new_tier() {
    let mut start = with_blank(&created_view(&[("sda", 4), ("sdb", 8), ("sdc", 12)], 1), &[("sdd", 12)]);
    for t in &mut start.tiers {
        for m in t.members.iter_mut().filter(|m| m.disk == "sdb") {
            m.role = MemberRole::Faulty;
        }
    }
    let p = plan_replace_at(&inventory_of(&start, None), &replace_req("sdb", "sdd"), NOW);
    assert!(p.valid, "{:?}", p.errors);
    let l = lines(&p.steps);
    let create = l.iter().position(|x| x.starts_with("mdadm --create /dev/md/vol2_t3")).expect("a new top tier");
    let last_wait = l.iter().rposition(|x| x.starts_with("wait /sys/block/md10") && !x.contains("md103")).unwrap();
    let last_add = l.iter().rposition(|x| x.contains("--add /dev/sdd2")).unwrap();
    assert!(last_add < create && last_wait < create, "{l:?}");
}

/// "missing" with the old slice of a disk that still works below: the three-way mirror tier
/// becomes RAID6 with it and the new disk; the plan shows that and warns about the window.
#[test]
fn replace_missing_shows_the_raid6_it_makes() {
    let mut start = with_blank(&created_view(&[("sda", 4), ("sdb", 8), ("sdc", 8), ("sdd", 8)], 2), &[("sde", 8)]);
    let t2 = start.tiers.iter_mut().find(|t| t.index == 2).unwrap();
    t2.members.retain(|m| m.part != "sdd2");
    for p in start.disks.iter_mut().flat_map(|d| d.parts.iter_mut()).filter(|p| p.kname == "sdd2") {
        p.fresh = false;
    }
    let p = plan_replace_at(&inventory_of(&start, None), &replace_req(MISSING, "sde"), NOW);
    assert!(p.valid, "{:?}", p.errors);
    let t = p.tiers_after.iter().find(|t| t.index == 2).unwrap();
    assert_eq!((t.level, t.members.len()), (Some(RaidLevel::Raid6), 4), "{:?}", p.tiers_after);
    assert!(p.warning_items.iter().any(|w| w.code == PlanWarningCode::FlexReducedToleranceWindow && w.args.subject.as_deref() == Some("2")), "{:?}", p.warnings);
    let mut end = start.clone();
    for s in &p.steps {
        end.apply(s);
    }
    assert_eq!(p.usable_bytes, lvm_usable(&end.shapes()));
}

/// A working disk of a tier that misses another disk is not replaced: md would put the new
/// disk in the empty slot, not in its place (the message says what to do instead).
#[test]
fn a_working_disk_of_a_degraded_tier_is_not_replaced() {
    let mut start = with_blank(&created_view(&[("sda", 8), ("sdb", 8), ("sdc", 8), ("sdd", 8)], 2), &[("sde", 8)]);
    for t in &mut start.tiers {
        t.members.retain(|m| m.disk != "sdd");
    }
    start.disks.retain(|d| d.name != "sdd");
    let p = plan_replace_at(&inventory_of(&start, None), &replace_req("sda", "sde"), NOW);
    assert!(p.issues.iter().any(|i| i.code == PlanIssueCode::ArrayNotHealthy), "{:?}", p.issues);
    assert!(p.errors.iter().any(|e| e.contains("replace the missing disk first")), "{:?}", p.errors);
    assert!(plan_replace_at(&inventory_of(&start, None), &replace_req(MISSING, "sde"), NOW).valid);
}
