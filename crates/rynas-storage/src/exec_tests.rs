//! Golden command sequences of the executor on the fixture NAS, refusals and rollbacks.

use rynas_api_types::storage::{
    ArrayHealth, GrowPlanRequest, MemberRole, RaidLevel, ReplacePlanRequest, ScrubPlanRequest,
    VolumePlanRequest,
};

use crate::Inventory;
use crate::exec::*;
use crate::plan;
use crate::tests_support::nas_inventory;

const KEY: &str = "cbdefghi-jklnrtuv-cbdefghi-jklnrtuv-cbdefghi-jklnrtuv-cbdefghi-jklnrtuv";

fn volume_req(disks: &[&str], level: RaidLevel, encrypt: bool, label: &str) -> VolumePlanRequest {
    VolumePlanRequest {
        disks: disks.iter().map(|s| s.to_string()).collect(),
        level,
        encrypt,
        compress: true,
        label: label.into(),
    }
}

fn machine() -> FakeMachine {
    let mut m = FakeMachine::default();
    m.output(
        "mdadm --detail --export /dev/md/backup",
        "MD_LEVEL=raid1\nMD_DEVICES=2\nMD_UUID=0a1b2c3d:4e5f6071:8293a4b5:c6d7e8f9\n",
    )
    .output(
        "mdadm --detail --export /dev/md/vol2",
        "MD_UUID=11112222:33334444:55556666:77778888\n",
    )
    .output(
        "blkid --probe --match-tag UUID --output value /dev/md/backup",
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n",
    )
    .output(
        "blkid --probe --match-tag UUID --output value /dev/mapper/rynas-vol2",
        "ffffffff-bbbb-cccc-dddd-eeeeeeeeeeee\n",
    )
    .output(
        "cryptsetup luksUUID /dev/md/vol2",
        "12345678-1234-1234-1234-123456789abc\n",
    );
    m.outputs.insert(
        "systemd-cryptenroll --recovery-key --unlock-key-file=/dev/stdin /dev/md/vol2".into(),
        Output {
            stdout: format!("🔐 {KEY}\n"),
            stderr: "A secret recovery key has been generated for this volume:\n".into(),
        },
    );
    m
}

async fn create(
    m: &FakeMachine,
    inv: &Inventory,
    req: &VolumePlanRequest,
    pass: Option<&str>,
) -> Result<CreateOutcome, String> {
    let p = plan::plan_volume(inv, req);
    assert!(p.valid, "{:?}", p.errors);
    create_volume(
        m,
        inv,
        &p,
        req.level,
        &req.disks,
        &Secrets::new(pass.map(String::from)),
        42,
    )
    .await
}

#[tokio::test]
async fn create_raid1_golden_sequence_and_record() {
    let inv = nas_inventory();
    let m = machine();
    let out = create(
        &m,
        &inv,
        &volume_req(&["sdd", "sde"], RaidLevel::Raid1, false, "backup"),
        None,
    )
    .await
    .unwrap();
    let ran = m.ran();
    let commit = ran
        .iter()
        .find(|l| l.starts_with("commit"))
        .unwrap()
        .clone();
    assert_eq!(
        ran,
        [
            "wipefs --all /dev/sdd",
            "sgdisk --zap-all /dev/sdd",
            "sgdisk --new=1:0:-128M --typecode=1:FD00 --change-name=1:rynas-backup /dev/sdd",
            "wipefs --all /dev/sde",
            "sgdisk --zap-all /dev/sde",
            "sgdisk --new=1:0:-128M --typecode=1:FD00 --change-name=1:rynas-backup /dev/sde",
            "udevadm settle",
            "mdadm --create /dev/md/backup --run --level=1 --raid-devices=2 --metadata=1.2 --name=backup --bitmap=internal /dev/sdd1 /dev/sde1",
            "mdadm --detail --export /dev/md/backup",
            "mkfs.btrfs --label backup --data single --metadata dup /dev/md/backup",
            "blkid --probe --match-tag UUID --output value /dev/md/backup",
            commit.as_str(),
            "mkdir -p /backup",
            "systemctl start backup.mount",
            "btrfs subvolume create /backup/.snapshots",
            "btrfs subvolume create /backup/@rynas-cache",
            "chattr +C /backup/@rynas-cache",
        ]
    );
    let r = &out.record;
    assert_eq!(r.label, "backup");
    assert_eq!(r.md_uuid, "0a1b2c3d:4e5f6071:8293a4b5:c6d7e8f9");
    assert_eq!(r.fs_uuid, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    assert_eq!(r.luks_uuid, None);
    assert_eq!(r.options, "noatime,compress=zstd:1");
    assert_eq!(r.created_ms, 42);
    assert_eq!(out.recovery_key, None);
    assert_eq!(
        m.commits.lock().unwrap().as_slice(),
        [VolumeChange::Add(r.clone())]
    );
}

#[tokio::test]
async fn create_encrypted_passes_secrets_on_stdin_and_hides_the_recovery_key() {
    let inv = nas_inventory();
    let m = machine();
    let out = create(
        &m,
        &inv,
        &volume_req(&["sdd"], RaidLevel::Single, true, "vol2"),
        Some("correct horse"),
    )
    .await
    .unwrap();
    let ran = m.ran();
    let with_stdin: Vec<&String> = ran.iter().filter(|l| l.ends_with("<stdin>")).collect();
    assert_eq!(
        with_stdin,
        [
            "cryptsetup luksFormat --batch-mode --type luks2 --sector-size 4096 --pbkdf argon2id --pbkdf-memory 262144 --label rynas-vol2 --key-file - /dev/md/vol2 <stdin>",
            "systemd-cryptenroll --recovery-key --unlock-key-file=/dev/stdin /dev/md/vol2 <stdin>",
            "cryptsetup open --key-file - /dev/md/vol2 rynas-vol2 <stdin>",
        ]
    );
    // a stale header backup is removed before a new one is written
    let rm = ran
        .iter()
        .position(|l| l == "rm -f /run/rynas/luks-header-vol2.img")
        .unwrap();
    assert!(ran[rm + 1].starts_with("cryptsetup luksHeaderBackup /dev/md/vol2"));
    assert_eq!(out.recovery_key.as_deref(), Some(KEY));
    assert_eq!(
        out.header_backup.as_deref(),
        Some("/run/rynas/luks-header-vol2.img")
    );
    assert_eq!(
        out.record.luks_uuid.as_deref(),
        Some("12345678-1234-1234-1234-123456789abc")
    );
    // neither the passphrase nor the key reach the log
    let log = m.logged();
    assert!(!log.contains("correct horse"));
    assert!(!log.contains(KEY), "{log}");
    assert!(!format!("{out:?}").contains(KEY));
    // a missing passphrase is refused before anything runs
    let m = machine();
    let e = create(
        &m,
        &inv,
        &volume_req(&["sdd"], RaidLevel::Single, true, "vol2"),
        None,
    )
    .await
    .unwrap_err();
    assert!(e.contains("passphrase"), "{e}");
    assert!(m.ran().is_empty());
}

#[tokio::test]
async fn mkfs_failure_stops_the_array_and_removes_the_partitions() {
    let inv = nas_inventory();
    let mut m = machine();
    m.fail.insert(
        "mkfs.btrfs".into(),
        "exit code 1: ERROR: unable to open /dev/md/backup".into(),
    );
    let e = create(
        &m,
        &inv,
        &volume_req(&["sdd", "sde"], RaidLevel::Raid1, false, "backup"),
        None,
    )
    .await
    .unwrap_err();
    let ran = m.ran();
    let at = ran
        .iter()
        .position(|l| l.starts_with("mkfs.btrfs"))
        .unwrap();
    assert_eq!(
        &ran[at + 1..],
        [
            "mdadm --stop /dev/md/backup",
            "mdadm --zero-superblock /dev/sdd1",
            "mdadm --zero-superblock /dev/sde1",
            "sgdisk --zap-all /dev/sde",
            "wipefs --all /dev/sde",
            "sgdisk --zap-all /dev/sdd",
            "wipefs --all /dev/sdd",
        ]
    );
    assert!(
        e.starts_with("step 10/16 (Create the btrfs filesystem (data single, metadata DUP)) failed: exit code 1: ERROR: unable to open /dev/md/backup; rolled back: stopped /dev/md/backup and cleared its member superblocks, removed the partitions of /dev/sde, removed the partitions of /dev/sdd"),
        "{e}"
    );
    assert!(m.commits.lock().unwrap().is_empty());
}

#[tokio::test]
async fn late_failure_undoes_mount_config_luks_and_disks() {
    let inv = nas_inventory();
    let mut m = machine();
    m.fail.insert(
        "btrfs subvolume create /vol2/@rynas-cache".into(),
        "exit code 1: ERROR: no space".into(),
    );
    // the partition node exists: its signatures are wiped too
    m.files
        .lock()
        .unwrap()
        .insert("/dev/sdd1".into(), String::new());
    let e = create(
        &m,
        &inv,
        &volume_req(&["sdd"], RaidLevel::Single, true, "vol2"),
        Some("correct horse"),
    )
    .await
    .unwrap_err();
    let ran = m.ran();
    let at = ran
        .iter()
        .position(|l| l == "btrfs subvolume create /vol2/@rynas-cache")
        .unwrap();
    assert_eq!(
        &ran[at + 1..],
        [
            "systemctl stop vol2.mount",
            "rmdir /vol2",
            "commit Remove(\"vol2\")",
            "cryptsetup close rynas-vol2",
            "rm -f /run/rynas/luks-header-vol2.img",
            "mdadm --stop /dev/md/vol2",
            "mdadm --zero-superblock /dev/sdd1",
            "wipefs --all /dev/sdd1",
            "sgdisk --zap-all /dev/sdd",
            "wipefs --all /dev/sdd",
        ]
    );
    assert!(
        e.contains("removed vol2 from fstab, crypttab and mdadm.conf"),
        "{e}"
    );
    assert!(!e.contains("NOT rolled back"), "{e}");
}

#[tokio::test]
async fn failed_rollback_steps_are_reported() {
    let inv = nas_inventory();
    let mut m = machine();
    m.fail
        .insert("mkfs.btrfs".into(), "exit code 1: boom".into());
    m.fail
        .insert("mdadm --stop".into(), "exit code 1: device busy".into());
    let e = create(
        &m,
        &inv,
        &volume_req(&["sdd"], RaidLevel::Single, false, "backup"),
        None,
    )
    .await
    .unwrap_err();
    assert!(e.contains("NOT rolled back, check by hand: Array"), "{e}");
    assert!(
        e.contains("rolled back: removed the partitions of /dev/sdd"),
        "{e}"
    );
}

#[tokio::test]
async fn busy_disks_are_refused_before_anything_runs() {
    let inv = nas_inventory();
    // sde1 holds a LUKS container: opened elsewhere, the kernel refuses the claim
    let mut m = machine();
    m.busy.push("/dev/sde1".into());
    let e = create(
        &m,
        &inv,
        &volume_req(&["sdd", "sde"], RaidLevel::Raid1, false, "backup"),
        None,
    )
    .await
    .unwrap_err();
    assert!(e.contains("/dev/sde1 is in use"), "{e}");
    assert!(m.ran().is_empty());
}

#[tokio::test]
async fn cancel_rolls_back() {
    let inv = nas_inventory();
    let mut m = machine();
    m.cancel_at = Some("mdadm --create".into());
    let e = create(
        &m,
        &inv,
        &volume_req(&["sdd"], RaidLevel::Single, false, "backup"),
        None,
    )
    .await
    .unwrap_err();
    assert!(e.contains("failed: cancelled"), "{e}");
    let ran = m.ran();
    // the create was killed: no array to stop, the partitions go
    assert_eq!(
        &ran[ran.len() - 2..],
        ["sgdisk --zap-all /dev/sdd", "wipefs --all /dev/sdd"]
    );
}

#[test]
fn confirmation() {
    let inv = nas_inventory();
    let d: Vec<_> = inv
        .disks
        .iter()
        .filter(|d| d.name == "sdd" || d.name == "sde")
        .collect();
    let ok = ["ZFL0CCC5".to_string(), "WD-WX0BBBB4".to_string()];
    assert!(check_confirmation("backup", "backup", &d, &ok).is_ok());
    assert!(check_confirmation("backup", "Backup", &d, &ok).is_err());
    assert!(check_confirmation("backup", "", &d, &ok).is_err());
    let e = check_confirmation("backup", "backup", &d, &ok[..1]).unwrap_err();
    assert!(e.contains("sdd: WD-WX0BBBB4"), "{e}");
    let extra = [ok[0].clone(), ok[1].clone(), "WD-CA0AAAA1".into()];
    assert!(check_confirmation("backup", "backup", &d, &extra).is_err());
    // a disk without serial is named by its kernel name
    let mut loop0 = d[0].clone();
    loop0.name = "loop0".into();
    loop0.serial = String::new();
    assert!(check_confirmation("t", "t", &[&loop0], &["loop0".into()]).is_ok());
}

fn healthy(inv: &mut Inventory) {
    let a = inv.arrays.iter_mut().find(|a| a.name == "md2").unwrap();
    a.sync = None;
    a.health = ArrayHealth::Healthy;
    a.degraded = 0;
    for m in &mut a.members {
        m.role = MemberRole::Active;
    }
}

fn add_disk(inv: &mut Inventory, name: &str) {
    let mut d = inv.disks.iter().find(|d| d.name == "sda").unwrap().clone();
    d.name = name.into();
    d.path = format!("/dev/{name}");
    d.serial = format!("SER-{name}");
    d.used_by.clear();
    inv.disks.push(d);
}

#[tokio::test]
async fn grow_waits_for_the_reshape_and_undoes_only_before_the_array_changes() {
    let mut inv = nas_inventory();
    healthy(&mut inv);
    add_disk(&mut inv, "sdf");
    let p = plan::plan_grow(
        &inv,
        &GrowPlanRequest {
            array: "md2".into(),
            disks: vec!["sdf".into()],
            level: None,
        },
    );
    assert!(p.valid, "{:?}", p.errors);
    let m = machine();
    m.files
        .lock()
        .unwrap()
        .insert("/sys/block/md2/md/sync_action".into(), "idle\n".into());
    run_plan(
        &m,
        &inv,
        &p,
        &["sdf".into()],
        &Secrets::new(Some("pw".into())),
    )
    .await
    .unwrap();
    assert_eq!(
        m.ran(),
        [
            "wipefs --all /dev/sdf",
            "sgdisk --zap-all /dev/sdf",
            "sgdisk --new=1:0:-128M --typecode=1:FD00 --change-name=1:rynas-vol1 /dev/sdf",
            "udevadm settle",
            "mdadm /dev/md2 --add /dev/sdf1",
            "mdadm --grow /dev/md2 --raid-devices=4",
            "cryptsetup resize --key-file - rynas-vol1 <stdin>",
            "btrfs filesystem resize max /vol1",
        ]
    );

    // failure while preparing the new disk: its partitions are removed
    let mut m = machine();
    m.fail
        .insert("sgdisk --new".into(), "exit code 4: boom".into());
    let e = run_plan(&m, &inv, &p, &["sdf".into()], &Secrets::default())
        .await
        .unwrap_err();
    assert!(
        e.contains("rolled back: removed the partitions of /dev/sdf"),
        "{e}"
    );
    // failure after the array changed: nothing is undone
    let mut m = machine();
    m.fail
        .insert("mdadm --grow".into(), "exit code 1: boom".into());
    let e = run_plan(&m, &inv, &p, &["sdf".into()], &Secrets::default())
        .await
        .unwrap_err();
    assert!(e.contains("nothing was rolled back"), "{e}");
    assert!(!m.ran().iter().any(|l| l.starts_with("sgdisk --zap-all /dev/sdf") && l != "sgdisk --zap-all /dev/sdf"));
}

#[tokio::test]
async fn replace_rebuild_waits_until_not_degraded() {
    let mut inv = nas_inventory();
    add_disk(&mut inv, "sdf");
    let p = plan::plan_replace(
        &inv,
        &ReplacePlanRequest {
            array: "md2".into(),
            old: "sdc".into(),
            new_disk: "sdf".into(),
        },
    );
    assert!(p.valid, "{:?}", p.errors);
    let m = machine();
    {
        let mut f = m.files.lock().unwrap();
        f.insert("/sys/block/md2/md/sync_action".into(), "idle\n".into());
        f.insert("/sys/block/md2/md/degraded".into(), "0\n".into());
    }
    run_plan(&m, &inv, &p, &["sdf".into()], &Secrets::default())
        .await
        .unwrap();
    let ran = m.ran();
    assert_eq!(ran[1], "sgdisk --replicate=/dev/sdf /dev/sda");
    assert!(ran.contains(&"mdadm /dev/md2 --fail /dev/sdc1".to_string()));
    assert_eq!(ran.last().unwrap(), "mdadm /dev/md2 --add /dev/sdf1");
    // the kernel's word wins over the plan: a busy new disk is refused
    let mut m = machine();
    m.busy.push("/dev/sdf".into());
    assert!(
        run_plan(&m, &inv, &p, &["sdf".into()], &Secrets::default())
            .await
            .unwrap_err()
            .contains("in use")
    );
}

#[tokio::test]
async fn scrub_cancel_stops_the_kernel_side() {
    let mut inv = nas_inventory();
    for a in &mut inv.arrays {
        a.sync = None;
    }
    let p = plan::plan_scrub(
        &inv,
        &ScrubPlanRequest {
            filesystem: "/vol1".into(),
            md_check: true,
        },
    );
    assert!(p.valid, "{:?}", p.errors);
    // a full run reads the mismatch count
    let m = machine();
    {
        let mut f = m.files.lock().unwrap();
        f.insert("/sys/block/md2/md/sync_action".into(), "idle\n".into());
        f.insert("/sys/block/md2/md/mismatch_cnt".into(), "8\n".into());
    }
    run_plan(&m, &inv, &p, &[], &Secrets::default())
        .await
        .unwrap();
    assert_eq!(
        m.ran(),
        [
            "btrfs scrub start -B -d /vol1",
            "echo check > /sys/block/md2/md/sync_action",
        ]
    );
    assert!(m.logged().contains("mismatch_cnt 8"));
    // cancelled while the md check runs: btrfs scrub cancelled, md back to idle
    let mut m = machine();
    m.files
        .lock()
        .unwrap()
        .insert("/sys/block/md2/md/sync_action".into(), "check\n".into());
    m.cancel_at = Some("btrfs scrub start".into());
    let e = run_plan(&m, &inv, &p, &[], &Secrets::default())
        .await
        .unwrap_err();
    assert!(e.contains("scrub and md check stopped"), "{e}");
    assert_eq!(m.ran().last().unwrap(), "btrfs scrub cancel /vol1");
}

#[tokio::test]
async fn unlock_and_lock() {
    let v = LuksVolume {
        label: "vol2".into(),
        device: "/dev/md/vol2".into(),
        mapper: "rynas-vol2".into(),
        mountpoint: "/vol2".into(),
    };
    let m = machine();
    unlock(&m, &v, &Secrets::new(Some("pw".into())))
        .await
        .unwrap();
    assert_eq!(
        m.ran(),
        [
            "cryptsetup open --key-file - /dev/md/vol2 rynas-vol2 <stdin>",
            "systemctl start vol2.mount",
            "systemctl try-reload-or-restart smbd.service nfs-server.service",
        ]
    );
    // wrong passphrase
    let mut m = machine();
    m.fail.insert(
        "cryptsetup open".into(),
        "exit code 2: No key available with this passphrase.".into(),
    );
    let e = unlock(&m, &v, &Secrets::new(Some("bad".into())))
        .await
        .unwrap_err();
    assert_eq!(e, "wrong passphrase or recovery key");
    // mount fails: locked again
    let mut m = machine();
    m.fail
        .insert("systemctl start".into(), "exit code 1: failed".into());
    let e = unlock(&m, &v, &Secrets::new(Some("pw".into())))
        .await
        .unwrap_err();
    assert!(e.contains("locked again"), "{e}");
    assert_eq!(m.ran().last().unwrap(), "cryptsetup close rynas-vol2");
    // lock, without systemd: plain umount
    let mut m = machine();
    m.no_systemd = true;
    m.files
        .lock()
        .unwrap()
        .insert("/dev/mapper/rynas-vol2".into(), String::new());
    lock(&m, &v).await.unwrap();
    assert_eq!(m.ran(), ["umount /vol2", "cryptsetup close rynas-vol2"]);
}

#[test]
fn recovery_key_and_md_uuid_parsing() {
    assert_eq!(
        find_recovery_key(&format!("A key:\n\n    🔐 {KEY}\n\nPlease save")).as_deref(),
        Some(KEY)
    );
    assert_eq!(find_recovery_key("abcdefgh-abcdefgh"), None);
    assert_eq!(
        md_uuid_from_export("MD_LEVEL=raid1\nMD_UUID=a:b:c:d\n").as_deref(),
        Some("a:b:c:d")
    );
}
