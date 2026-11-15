//! Golden step sequences of the flexible RAID planner for the cases of the design table, the
//! tier-state model (planning again after every step converges), refusals.

use super::*;
use crate::flex::tiers::{MIN_TIER, reach};
use crate::lsblk::BlockDev;
use crate::tests_support::{FlexState, flex_inventory};
use rynas_api_types::storage::{
    BtrfsFilesystem, PowerState, ScrubStatus, TierRecord, VolumeRecord,
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
        v.disks.push(VDisk { name: n.clone(), size: *s, parts: Vec::new() });
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
        if complete_too && i >= prelude {
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
    v.disks.push(VDisk { name: "sdd".into(), size: 12 * TB, parts: Vec::new() });
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
    assert!(l.contains(&"mdadm --grow /dev/md/vol2_t1 --raid-devices=5".to_string()));
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
    start.disks.push(VDisk { name: "sdd".into(), size: 8 * TB, parts: Vec::new() });
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
