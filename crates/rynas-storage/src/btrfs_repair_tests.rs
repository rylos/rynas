use super::*;
use crate::exec::FakeMachine;
use crate::lsblk::BlockDev;
use rynas_api_types::storage::BtrfsDevice;

const UUID: &str = "de89be44-41d2-4fad-ae56-9a2c5b027c0e";

fn usage(name: &str) -> Vec<DevUsage> {
    let text = match name {
        "healthy" => include_str!("../tests/fixtures/btrfs-device-usage-healthy.txt"),
        "degraded" => include_str!("../tests/fixtures/btrfs-device-usage-degraded.txt"),
        _ => include_str!("../tests/fixtures/btrfs-device-usage-after-replace.txt"),
    };
    parse_device_usage(text)
}

fn dev(devid: u64, path: &str, chunks: &[(&str, &str, u64)], unallocated: u64) -> DevUsage {
    DevUsage {
        devid,
        path: path.into(),
        missing: path.is_empty(),
        size: if path.is_empty() { 0 } else { 1000 },
        chunks: chunks.iter().map(|(k, p, b)| (k.to_string(), p.to_string(), *b)).collect(),
        unallocated,
    }
}

/// A three-device raid1 data / raid1c3 metadata filesystem mounted degraded on two of them
/// (the captured outputs).
fn degraded_fs() -> BtrfsFilesystem {
    let show = crate::btrfs::parse_show(include_str!("../tests/fixtures/btrfs-show-degraded.txt"));
    let mut f = BtrfsFilesystem {
        uuid: UUID.into(),
        label: Some("fxvol".into()),
        mountpoint: Some("/fxvol".into()),
        mountpoints: vec!["/fxvol".into()],
        mount_options: ["rw", "noatime", "degraded", "ssd"].map(String::from).into(),
        devices: show[0]
            .devices
            .iter()
            .map(|d| BtrfsDevice { devid: d.devid, path: d.path.clone(), size: d.size, missing: d.missing, ..Default::default() })
            .collect(),
        ..Default::default()
    };
    f.repair = repair_of(&f, &usage("degraded"));
    f
}

#[test]
fn device_usage_of_a_degraded_filesystem() {
    let u = usage("degraded");
    assert_eq!(u.len(), 3);
    assert_eq!((u[0].devid, u[0].path.as_str(), u[0].missing), (1, "/dev/loop0", false));
    assert_eq!(u[0].chunks[0], ("data".into(), "raid1".into(), 671_088_640));
    assert_eq!(u[0].unallocated, 125_829_120);
    let m = &u[2];
    assert!(m.missing && m.path.is_empty());
    assert_eq!(m.devid, 3);
    assert_eq!(m.size, 0);
    assert_eq!(m.capacity(), 1_073_741_824, "a missing device's size is what it held");
    assert_eq!(parse_device_usage("/dev/sda1, ID: 1\n   Data,RAID0/3:  1073741824\n")[0].chunks[0].1, "raid0");
}

#[test]
fn show_names_the_missing_device_of_a_mounted_filesystem() {
    // mounted degraded, btrfs-progs prints the device with an empty path and MISSING
    let v = crate::btrfs::parse_show(include_str!("../tests/fixtures/btrfs-show-degraded.txt"));
    let d = &v[0].devices[2];
    assert_eq!(d.devid, 3);
    assert!(d.missing && d.path.is_empty(), "{d:?}");
    assert!(v[0].missing);
    assert!(!v[0].devices[0].missing);
    let u = crate::btrfs::parse_show(include_str!("../tests/fixtures/btrfs-show-unmounted-missing.txt"));
    assert_eq!(u[0].total_devices, 3);
    assert_eq!(u[0].devices.len(), 2);
    assert!(u[0].missing);
}

/// `filesystem usage` counts the missing device's size; `device stats` names it by devid.
#[test]
fn usage_and_stats_of_a_degraded_filesystem() {
    let u = crate::btrfs::parse_usage(include_str!("../tests/fixtures/btrfs-usage-degraded.txt"));
    assert_eq!(u.missing, 1_073_741_824);
    assert_eq!(u.data_ratio, 2.0);
    assert_eq!(u.metadata_ratio, 3.0);
    let s = crate::btrfs::parse_device_stats(include_str!("../tests/fixtures/btrfs-device-stats-degraded.txt"));
    assert_eq!(s.len(), 3);
    assert!(s.contains_key("devid:3") && s.contains_key("/dev/loop0"));
}

#[test]
fn profiles_lost_only_beyond_their_tolerance() {
    // one device of three gone: raid1 and raid1c3 keep a copy of everything
    assert!(lost(&usage("degraded")).is_empty());
    assert!(lost(&usage("healthy")).is_empty());
    // single data on the missing device: lost; single on a present device: fine
    let u = vec![
        dev(1, "/dev/sda", &[("data", "single", 10), ("metadata", "raid1", 5)], 100),
        dev(2, "", &[("data", "single", 10), ("metadata", "raid1", 5)], 100),
    ];
    assert_eq!(lost(&u), ["data single"]);
    let u = vec![dev(1, "/dev/sda", &[("data", "single", 10), ("metadata", "dup", 5)], 100), dev(2, "", &[("metadata", "raid1", 5)], 100)];
    assert!(lost(&u).is_empty());
    // two of four gone from a raid1: lost; from a raid6 or raid1c3: not
    let gone2 = |p: &str| {
        vec![
            dev(1, "/dev/sda", &[("data", p, 10)], 0),
            dev(2, "/dev/sdb", &[("data", p, 10)], 0),
            dev(3, "", &[("data", p, 10)], 0),
            dev(4, "", &[("data", p, 10)], 0),
        ]
    };
    assert_eq!(lost(&gone2("raid1")), ["data raid1"]);
    assert_eq!(lost(&gone2("raid10")), ["data raid10"]);
    assert!(lost(&gone2("raid6")).is_empty());
    assert!(lost(&gone2("raid1c3")).is_empty());
    assert_eq!(lost(&gone2("raid0")), ["data raid0"]);
    assert_eq!((tolerance("dup"), tolerance("raid1c4"), min_devices("raid10")), (0, 3, 2));
}

#[test]
fn weaker_chunks_left_by_the_degraded_mount_are_converted_back() {
    // after the replace: metadata and system chunks written single while two devices were
    // present (raid1c3 needs three)
    let c = converts(&usage("after"));
    assert_eq!(
        c,
        vec![
            BtrfsConvert { kind: "metadata".into(), from: vec!["single".into()], to: "raid1c3".into() },
            BtrfsConvert { kind: "system".into(), from: vec!["single".into()], to: "raid1c3".into() },
        ]
    );
    assert_eq!(convert_argv("/fxvol", &c), ["btrfs", "balance", "start", "-mconvert=raid1c3,soft", "/fxvol"]);
    assert!(converts(&usage("healthy")).is_empty());
    // a chosen mix (single data, raid1 metadata) is no weakness
    let u = vec![dev(1, "/dev/sda", &[("data", "single", 10), ("metadata", "raid1", 5)], 0)];
    assert!(converts(&u).is_empty());
    // data and metadata, different system profile
    let c = vec![
        BtrfsConvert { kind: "data".into(), from: vec!["single".into()], to: "raid1".into() },
        BtrfsConvert { kind: "metadata".into(), from: vec!["single".into()], to: "raid1c3".into() },
        BtrfsConvert { kind: "system".into(), from: vec!["single".into()], to: "raid1".into() },
    ];
    assert_eq!(
        convert_argv("/v", &c),
        ["btrfs", "balance", "start", "-dconvert=raid1,soft", "-mconvert=raid1c3,soft", "-sconvert=raid1,soft", "-f", "/v"]
    );
}

#[test]
fn removing_the_missing_device_needs_enough_devices_and_space() {
    // raid1c3 metadata on the two devices left: too few
    assert_eq!(remove_blocker(&usage("degraded")), Some(BtrfsBlocker::TooFewDevices));
    let raid1 = |free: u64| {
        vec![
            dev(1, "/dev/sda", &[("data", "raid1", 10), ("metadata", "raid1", 2)], free),
            dev(2, "/dev/sdb", &[("data", "raid1", 10), ("metadata", "raid1", 2)], free),
            dev(3, "", &[("data", "raid1", 10), ("metadata", "raid1", 2)], 50),
        ]
    };
    assert_eq!(remove_blocker(&raid1(100)), None);
    assert_eq!(remove_blocker(&raid1(5)), Some(BtrfsBlocker::NoSpace));
    let u = vec![dev(1, "/dev/sda", &[("data", "single", 10)], 100), dev(2, "", &[("data", "single", 10)], 100)];
    assert_eq!(remove_blocker(&u), Some(BtrfsBlocker::Incomplete));
}

#[test]
fn repair_facts_of_a_mounted_filesystem() {
    let f = degraded_fs();
    let r = f.repair.as_ref().unwrap();
    assert!(r.mounted && r.degraded_mount && !r.system);
    assert_eq!(r.missing, [3]);
    assert_eq!(r.missing_count, 1);
    assert_eq!(r.total_devices, 3);
    assert_eq!(r.missing_bytes, 1_073_741_824);
    assert!(r.lost.is_empty());
    assert_eq!(r.remove, Some(BtrfsBlocker::TooFewDevices));
    assert!(r.degraded());
    // nothing missing, nothing weak: no repair
    let mut healthy = f.clone();
    healthy.devices[2].missing = false;
    assert_eq!(repair_of(&healthy, &usage("healthy")), None);
    // after the replace: only the conversion is left
    let r = repair_of(&healthy, &usage("after")).unwrap();
    assert!(!r.degraded() && r.convert.len() == 2);
    assert_eq!(r.remove, None);
}

#[test]
fn superblock() {
    let s = parse_dump_super(include_str!("../tests/fixtures/btrfs-dump-super.txt")).unwrap();
    assert_eq!(s.fsid, UUID);
    assert_eq!(s.label.as_deref(), Some("fxvol"));
    assert_eq!((s.num_devices, s.devid, s.generation), (3, 2, 9));
    assert_eq!(s.dev_bytes, 1_073_741_824);
    assert_eq!(parse_dump_super("ERROR: bad magic on superblock on /dev/sdz\n"), None);
}

fn graph() -> BlockGraph {
    let mut g = BlockGraph::default();
    for (k, uuid, label) in [("sdb1", UUID, "fxvol"), ("sdc1", UUID, "fxvol"), ("sdd1", "other", "data2")] {
        g.devs.insert(
            k.into(),
            BlockDev {
                kname: k.into(),
                name: k.into(),
                path: format!("/dev/{k}"),
                kind: "part".into(),
                fstype: Some("btrfs".into()),
                uuid: Some(uuid.into()),
                label: Some(label.into()),
                ..Default::default()
            },
        );
    }
    g
}

fn sb(devid: u64, generation: u64) -> Super {
    Super { fsid: UUID.into(), label: Some("fxvol".into()), num_devices: 3, devid, generation, dev_bytes: 1 << 30 }
}

#[test]
fn configured_volumes_that_did_not_mount() {
    let fstab = parse_fstab(
        "# comment\nUUID=24a82255-ac7d-4198-b5d4-8dba11c8f5ed / btrfs defaults 0 0\n\
         UUID=DE89BE44-41d2-4fad-ae56-9a2c5b027c0e /fxvol btrfs noatime,compress=zstd:1,nofail,x-systemd.device-timeout=10s 0 0\n\
         LABEL=data2 /data2 btrfs nofail 0 0\n/dev/sde1 /media/usb btrfs noauto 0 0\nbroken\n",
    );
    assert_eq!(fstab.len(), 4);
    assert_eq!(fstab[1].options, ["noatime", "compress=zstd:1", "nofail", "x-systemd.device-timeout=10s"]);
    let g = graph();
    // data2 is mounted
    let mounts = vec![Mount { mountpoint: "/data2".into(), fstype: "btrfs".into(), ..Default::default() }];
    let u = unmounted(&fstab, &g, &mounts, &[]);
    assert_eq!(u.len(), 1, "{u:?}");
    assert_eq!(u[0].uuid, UUID, "UUID= matched without case");
    assert_eq!(u[0].mountpoint, "/fxvol");
    assert_eq!(u[0].label.as_deref(), Some("fxvol"));
    assert_eq!(u[0].present, [("sdb1".to_string(), "/dev/sdb1".to_string()), ("sdc1".to_string(), "/dev/sdc1".to_string())]);
    // the same volume mounted (found by the inventory) is no candidate
    let mounted = vec![BtrfsFilesystem { uuid: UUID.into(), ..Default::default() }];
    assert!(unmounted(&fstab, &g, &[], &mounted).iter().all(|x| x.uuid != UUID));

    // 2 of 3 present: degraded, mountable
    let supers = vec![("/dev/sdb1".to_string(), sb(1, 9)), ("/dev/sdc1".to_string(), sb(2, 9))];
    let f = unmounted_filesystem(&u[0], Some(&supers)).unwrap();
    let r = f.repair.as_ref().unwrap();
    assert!(!r.mounted);
    assert_eq!((r.total_devices, r.missing_count), (3, 1));
    assert_eq!(r.mount, None);
    assert_eq!(r.present.iter().map(|m| m.devid).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(f.devices.len(), 2);
    assert_eq!(f.mountpoint, None);
    // all present: merely not mounted, not ours to report
    let all = vec![supers[0].clone(), supers[1].clone(), ("/dev/sdx".to_string(), sb(3, 9))];
    assert_eq!(unmounted_filesystem(&u[0], Some(&all)), None);
    // superblocks unread (a disk sleeps), or no device at all
    let r = unmounted_filesystem(&u[0], None).unwrap().repair.unwrap();
    assert_eq!(r.mount, Some(BtrfsBlocker::Asleep));
    let none = Unmounted { present: Vec::new(), ..u[0].clone() };
    assert_eq!(unmounted_filesystem(&none, None).unwrap().repair.unwrap().mount, Some(BtrfsBlocker::NoDevices));
}

fn member(devid: u64) -> BtrfsMember {
    BtrfsMember { devid, path: format!("/dev/sd{}1", (b'a' + devid as u8) as char), generation: 9 }
}

#[test]
fn split_brain_guard() {
    let dir = tempfile::tempdir().unwrap();
    let path = Guard::path(dir.path());
    let mut g = Guard::load(&path);
    assert!(!g.split_brain(UUID, &[member(1)]), "nothing recorded");
    // degraded on device 1 (device 2 missing)
    g.record(UUID, &[member(1)], 1000);
    g.save(&path).unwrap();
    let g = Guard::load(&path);
    assert_eq!(g.mounts[UUID].present, [1]);
    // next boot: device 2 alone (it was the missing one): refused
    assert!(g.split_brain(UUID, &[member(2)]));
    // device 1 there again (with or without 2): fine
    assert!(!g.split_brain(UUID, &[member(1)]));
    assert!(!g.split_brain(UUID, &[member(1), member(2)]));
    // three devices: degraded on 1+2, later 2+3 keeps a device of the last mount
    let mut g = Guard::default();
    g.record(UUID, &[member(1), member(2)], 1);
    assert!(!g.split_brain(UUID, &[member(2), member(3)]));
    assert!(g.split_brain(UUID, &[member(3)]));
    // whole again: forgotten
    assert!(g.forget_healthy(&[UUID.to_uppercase()]));
    assert!(!g.split_brain(UUID, &[member(3)]));
    assert!(!g.forget_healthy(&[UUID.into()]));
}

fn candidate() -> MountCandidate {
    MountCandidate {
        uuid: UUID.into(),
        label: "fxvol".into(),
        mountpoint: "/fxvol".into(),
        options: ["noatime", "compress=zstd:1", "nofail", "x-systemd.device-timeout=10s", "defaults"].map(String::from).into(),
        present: vec![member(1), member(2)],
        total: 3,
    }
}

#[test]
fn degraded_mount_command() {
    let c = candidate();
    assert_eq!(mount_options(&c.options, &c.present), "noatime,compress=zstd:1,degraded,device=/dev/sdb1,device=/dev/sdc1");
    assert_eq!(
        mount_argv(&c, false, "x"),
        ["mount", "-t", "btrfs", "-o", "noatime,compress=zstd:1,degraded,device=/dev/sdb1,device=/dev/sdc1", "/dev/sdb1", "/fxvol"]
    );
    let a = mount_argv(&c, true, "de89be44-1");
    assert_eq!(a[0], "systemd-run");
    assert!(a.contains(&"--unit=rynas-btrfs-degraded-de89be44-1".to_string()), "{a:?}");
    assert!(a.contains(&"--wait".to_string()) && a.contains(&"--pipe".to_string()));
    let rest: Vec<&String> = a.iter().skip_while(|x| *x != "--").skip(1).collect();
    assert_eq!(rest[0], "mount");
    assert_eq!(rest.last().unwrap().as_str(), "/fxvol");
    let p = plan_mount(&c, false, "x");
    assert!(p.valid);
    assert_eq!(p.steps[0].kind, Some(PlanStepKind::BtrfsMountDegraded));
    assert!(!plan_mount(&MountCandidate { present: Vec::new(), ..c }, false, "x").valid);
}

#[test]
fn mount_after_boot() {
    let c = candidate();
    let line = mount_argv(&c, false, "x").join(" ");
    let mut h = crate::host::FakeHost::default();
    h.cmd(&line, 0, "");
    assert_eq!(mount_now(&h, &c, false, "x"), Ok(()));
    assert_eq!(h.ran(), [line.clone()]);
    let mut h = crate::host::FakeHost::default();
    h.commands.get_mut().unwrap().insert(
        line,
        crate::host::CmdOutput { status: Some(32), stdout: String::new(), stderr: "mount: /fxvol: wrong fs type, bad option, bad superblock on /dev/sdb1\n".into() },
    );
    let e = mount_now(&h, &c, false, "x").unwrap_err();
    assert!(e.contains("bad superblock") && e.contains("32"), "{e}");
}

#[test]
fn candidates_and_their_blockers() {
    let u = Unmounted {
        uuid: UUID.into(),
        label: Some("fxvol".into()),
        mountpoint: "/fxvol".into(),
        options: vec!["noatime".into()],
        present: vec![("sdb1".into(), "/dev/sdb1".into())],
    };
    let supers = vec![("/dev/sdb1".to_string(), sb(1, 9))];
    let fs = vec![unmounted_filesystem(&u, Some(&supers)).unwrap()];
    let mps: HashMap<String, (String, Vec<String>)> = [(UUID.to_string(), ("/fxvol".to_string(), vec!["noatime".to_string()]))].into();
    let (ok, blocked) = candidates(&fs, &[], &Guard::default(), &mps);
    assert_eq!(ok.len(), 1);
    assert!(blocked.is_empty());
    assert_eq!(ok[0].label, "fxvol");
    assert_eq!(ok[0].total, 3);
    // turned off for this filesystem
    let (ok, blocked) = candidates(&fs, &[UUID.to_uppercase()], &Guard::default(), &mps);
    assert!(ok.is_empty());
    assert_eq!(blocked[0].1, BtrfsBlocker::Disabled);
    // the device present was the missing one of the last degraded mount
    let mut g = Guard::default();
    g.record(UUID, &[member(2)], 1);
    let (_, blocked) = candidates(&fs, &[], &g, &mps);
    assert_eq!(blocked[0].1, BtrfsBlocker::SplitBrain);
    // a filesystem that is mounted is never a candidate
    let (ok, blocked) = candidates(&[degraded_fs()], &[], &Guard::default(), &mps);
    assert!(ok.is_empty() && blocked.is_empty());
}

#[test]
fn waits_for_uptime_and_stable_devices() {
    use crate::recover::{MAX_UPTIME_S, STABLE_MS};
    let mut w = Waiting::default();
    let t0 = 1_000_000;
    assert!(w.due(vec![candidate()], t0, 60).is_empty());
    assert!(w.due(vec![candidate()], t0 + STABLE_MS, 100).is_empty(), "boot too recent");
    // a device joined: the wait starts again
    let mut more = candidate();
    more.present.push(member(3));
    assert!(w.due(vec![more.clone()], t0 + STABLE_MS, 300).is_empty());
    assert_eq!(w.due(vec![more.clone()], t0 + 2 * STABLE_MS, 400).len(), 1);
    w.forget(UUID);
    assert!(w.finished(MAX_UPTIME_S + 1));
    // first seen past the boot window: never
    let mut w = Waiting::default();
    assert!(w.due(vec![candidate()], t0, 6 * 3600).is_empty());
    assert!(w.due(vec![candidate()], t0 + 10 * STABLE_MS, 6 * 3600 + 1200).is_empty());
    assert!(w.finished(6 * 3600));
}

#[test]
fn replace_plan() {
    let f = degraded_fs();
    let p = plan_replace(&f, 3, "sdd", 2 << 30);
    assert!(p.valid, "{:?}", p.errors);
    let kinds: Vec<_> = p.steps.iter().map(|s| s.kind.unwrap()).collect();
    assert_eq!(
        kinds,
        [
            PlanStepKind::WipeSignatures,
            PlanStepKind::BtrfsReplace,
            PlanStepKind::BtrfsWaitReplace,
            PlanStepKind::BtrfsResize,
            PlanStepKind::BtrfsConvert
        ]
    );
    assert_eq!(p.steps[0].destroys, ["sdd"]);
    assert_eq!(
        p.steps[1].action,
        PlanAction::Command { argv: ["btrfs", "replace", "start", "3", "/dev/sdd", "/fxvol"].map(String::from).into(), stdin: None }
    );
    // a disk too small, a device that is not missing
    let small = plan_replace(&f, 3, "sdd", 1 << 29);
    assert!(!small.valid && small.steps.is_empty());
    assert!(small.errors[0].contains("smaller"), "{:?}", small.errors);
    assert!(!plan_replace(&f, 1, "sdd", 2 << 30).valid);
    // not mounted: mount it first
    let mut un = f.clone();
    un.mountpoint = None;
    assert!(plan_replace(&un, 3, "sdd", 2 << 30).errors[0].contains("not mounted"));
    // removing is blocked (raid1c3 on two devices); converting waits for the repair
    let r = plan_remove_missing(&f);
    assert!(!r.valid && r.errors[0].contains("fewer"), "{:?}", r.errors);
    assert!(!plan_convert(&f).valid);
}

#[test]
fn remove_and_convert_plans() {
    let mut f = degraded_fs();
    let u = vec![
        dev(1, "/dev/sda", &[("data", "raid1", 10), ("metadata", "raid1", 2)], 100),
        dev(2, "/dev/sdb", &[("data", "raid1", 10), ("metadata", "raid1", 2), ("data", "single", 3)], 100),
        dev(3, "", &[("data", "raid1", 10), ("metadata", "raid1", 2)], 50),
    ];
    f.repair = repair_of(&f, &u);
    let p = plan_remove_missing(&f);
    assert!(p.valid, "{:?}", p.errors);
    assert_eq!(
        p.steps[0].action,
        PlanAction::Command { argv: ["btrfs", "device", "remove", "missing", "/fxvol"].map(String::from).into(), stdin: None }
    );
    assert_eq!(p.steps[1].kind, Some(PlanStepKind::BtrfsConvert));
    assert_eq!(p.steps[1].args.items, ["data raid1"]);
    // nothing missing any more, single chunks left: convert
    f.devices[2].missing = false;
    let mut u2 = u.clone();
    u2[2].path = "/dev/sdc".into();
    u2[2].missing = false;
    f.repair = repair_of(&f, &u2);
    let c = plan_convert(&f);
    assert!(c.valid, "{:?}", c.errors);
    assert_eq!(
        c.steps[0].action,
        PlanAction::Command { argv: ["btrfs", "balance", "start", "-dconvert=raid1,soft", "/fxvol"].map(String::from).into(), stdin: None }
    );
}

#[test]
fn replace_status() {
    assert_eq!(parse_replace_status("12.3% done, 0 write errs, 0 uncorr. read errs\n"), ReplaceState::Running(12.3));
    assert_eq!(
        parse_replace_status(include_str!("../tests/fixtures/btrfs-replace-status-finished.txt")),
        ReplaceState::Finished
    );
    assert!(matches!(parse_replace_status("Started on 18.Nov 12:05:56, canceled on 18.Nov 12:06:10 at 0.0%\n"), ReplaceState::Failed(_)));
    assert!(matches!(parse_replace_status("Never started\n"), ReplaceState::Failed(_)));
}

fn run_now<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

#[test]
fn the_replace_task_runs_waits_and_converts_what_is_weak_then() {
    let f = degraded_fs();
    let p = plan_replace(&f, 3, "sdd", 2 << 30);
    let mut m = FakeMachine::default();
    m.output("btrfs replace status -1 /fxvol", "Started on 18.Nov 12:05:56, finished on 18.Nov 12:05:56, 0 write errs, 0 uncorr. read errs\n")
        .output("btrfs device usage -b /fxvol", include_str!("../tests/fixtures/btrfs-device-usage-after-replace.txt"));
    run_now(run_plan(&m, &p)).unwrap();
    assert_eq!(
        m.ran(),
        [
            "wipefs -a /dev/sdd",
            "btrfs replace start 3 /dev/sdd /fxvol",
            "btrfs replace status -1 /fxvol",
            "btrfs filesystem resize 3:max /fxvol",
            "btrfs device usage -b /fxvol",
            "btrfs balance start -mconvert=raid1c3,soft /fxvol",
        ]
    );
    // the new disk is in use: nothing runs
    let mut m = FakeMachine { busy: vec!["/dev/sdd".into()], ..Default::default() };
    m.output("btrfs replace status -1 /fxvol", "");
    assert!(run_now(run_plan(&m, &p)).unwrap_err().contains("in use"));
    assert!(m.ran().is_empty());
    // the rebuild stops: the task fails
    let mut m = FakeMachine::default();
    m.output("btrfs replace status -1 /fxvol", "Started on 18.Nov 12:05:56, canceled on 18.Nov 12:06:10 at 0.0%\n");
    assert!(run_now(run_plan(&m, &p)).unwrap_err().contains("rebuild stopped"));
    // nothing weak after the rebuild: the conversion is skipped
    let mut m = FakeMachine::default();
    m.output("btrfs replace status -1 /fxvol", "Started on x, finished on y, 0 write errs\n")
        .output("btrfs device usage -b /fxvol", include_str!("../tests/fixtures/btrfs-device-usage-healthy.txt"));
    run_now(run_plan(&m, &p)).unwrap();
    assert!(!m.ran().iter().any(|l| l.starts_with("btrfs balance")));
}

#[test]
fn a_cancelled_balance_is_stopped_in_the_kernel() {
    let mut f = degraded_fs();
    f.devices[2].missing = false;
    f.repair = repair_of(&f, &usage("after"));
    let p = plan_convert(&f);
    let mut m = FakeMachine { cancel_at: Some("btrfs balance start".into()), ..Default::default() };
    m.output("btrfs device usage -b /fxvol", include_str!("../tests/fixtures/btrfs-device-usage-after-replace.txt"));
    m.fail.insert("btrfs balance start".into(), "killed".into());
    assert!(run_now(run_plan(&m, &p)).is_err());
    assert_eq!(m.ran().last().unwrap(), "btrfs balance cancel /fxvol");
}
