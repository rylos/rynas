use std::sync::Arc;

use rynas_api_types::removable::{DriveBus, RemoveMethod};

use crate::fixture::{Fixture, Fs};
use crate::host::{Call, FakeHost, Output};
use crate::ops::{Ctx, Removable, RemovableError};
use crate::{Scanner, Tree, mounts};

fn scan(f: &Fixture) -> Vec<rynas_api_types::removable::RemovableDrive> {
    Scanner::new(Tree::new(f.root())).tick(true)
}

fn ctx<'a>(log: &'a dyn Fn(&str), hook: &'a dyn Fn(&str)) -> Ctx<'a> {
    Ctx {
        log,
        gid: 100,
        read_only: false,
        before_unmount: hook,
    }
}

#[test]
fn demo_machine() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::demo(dir.path());
    let drives = scan(&f);
    let names: Vec<&str> = drives.iter().map(|d| d.name.as_str()).collect();
    // the SATA disk is not removable
    assert_eq!(names, ["mmcblk0", "sdc", "sdd"]);

    let stick = &drives[1];
    assert_eq!(stick.bus, DriveBus::Usb);
    assert_eq!(stick.title, "SanDisk Ultra Fit");
    assert_eq!(stick.serial, "4C530001");
    assert_eq!(stick.remove_method, RemoveMethod::PowerOff);
    let usb = stick.usb.as_ref().unwrap();
    assert_eq!((usb.speed_mbps, usb.version.as_str()), (5000, "3.20"));
    let v = &stick.volumes[0];
    assert_eq!(
        (v.name.as_str(), v.label.as_str(), v.fs_type.as_str()),
        ("sdc1", "SANDISK", "vfat")
    );
    assert_eq!(v.mount_point.as_deref(), Some("/media/SANDISK"));
    assert!(v.supported && !v.read_only);
    // statvfs of the directory created by the fixture
    assert!(v.total_bytes.unwrap() > 0);

    let disk = &drives[2];
    assert_eq!(disk.title, "WD Elements 25A3");
    assert_eq!(disk.volumes.len(), 2);
    assert!(disk.volumes[1].luks && !disk.volumes[1].unlocked);
    assert!(disk.volumes[1].mount_point.is_none());

    let sd = &drives[0];
    assert_eq!(sd.bus, DriveBus::Sd);
    assert_eq!(sd.title, "SD64G");
    assert_eq!(sd.remove_method, RemoveMethod::Release);
    assert_eq!(sd.volumes[0].fs_type, "exfat");

    let m = mounts(&drives);
    assert_eq!(m.len(), 1);
    assert!(m[0].removable);
    assert_eq!(
        (
            m[0].device.as_str(),
            m[0].drive.as_str(),
            m[0].device_name.as_str()
        ),
        ("sdc1", "sdc", "SanDisk Ultra Fit")
    );
}

#[test]
fn unlocked_luks_follows_the_cleartext_device() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = Fixture::demo(dir.path());
    f.unlock(
        "sdd",
        "sdd2",
        3,
        Fs {
            fs_type: "ext4",
            label: "Secret",
            uuid: "c1",
        },
    );
    f.mount("254:3", "/media/Secret", "ext4", false);
    let drives = scan(&f);
    let v = &drives[2].volumes[1];
    assert!(v.unlocked);
    assert_eq!(v.clear.as_ref().unwrap().name, "dm-3");
    assert_eq!(v.display_label(), Some("Secret"));
    assert_eq!(v.mount_point.as_deref(), Some("/media/Secret"));
}

#[test]
fn system_drives_are_never_listed() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = Fixture::new(dir.path());
    // a USB disk holding /boot
    f.usb_disk(
        "sdc",
        32,
        "2-1",
        "X",
        "Boot",
        "1",
        8_000_000_000,
        480,
        false,
    );
    f.partition(
        "sdc",
        "sdc1",
        8,
        33,
        1_000_000_000,
        Fs {
            fs_type: "ext4",
            label: "boot",
            uuid: "b1",
        },
    );
    f.mount("8:33", "/boot", "ext4", false);
    // a USB disk in /etc/fstab
    f.usb_disk(
        "sdd",
        48,
        "2-2",
        "X",
        "Backup",
        "2",
        8_000_000_000,
        480,
        false,
    );
    f.partition(
        "sdd",
        "sdd1",
        8,
        49,
        1_000_000_000,
        Fs {
            fs_type: "ext4",
            label: "bk",
            uuid: "fstab-uuid",
        },
    );
    f.put("/etc/fstab", "UUID=fstab-uuid /mnt/bk ext4 defaults 0 0\n");
    // a USB disk in a RAID
    f.usb_disk(
        "sde",
        64,
        "2-3",
        "X",
        "Raid",
        "3",
        8_000_000_000,
        480,
        false,
    );
    f.partition(
        "sde",
        "sde1",
        8,
        65,
        1_000_000_000,
        Fs {
            fs_type: "linux_raid_member",
            label: "",
            uuid: "r1",
        },
    );
    // the boot flash module of an appliance (USB ids f400:f400)
    f.usb_disk(
        "sdf",
        80,
        "2-4",
        "Synology",
        "DiskStation",
        "4",
        128_000_000,
        480,
        false,
    );
    f.put(
        "/run/udev/data/b8:80",
        "E:ID_BUS=usb\nE:ID_VENDOR_ID=f400\nE:ID_MODEL_ID=f400\n",
    );
    // udev says: ignore
    f.usb_disk(
        "sdg",
        96,
        "2-5",
        "X",
        "Ignored",
        "5",
        8_000_000_000,
        480,
        false,
    );
    f.put("/run/udev/data/b8:96", "E:ID_BUS=usb\nE:UDISKS_IGNORE=1\n");
    // synodeb's udev rule marks the boot flash by property, whatever its USB ids
    f.usb_disk("sdh", 112, "2-6", "Vendor", "Flash", "6", 128_000_000, 480, false);
    f.put("/run/udev/data/b8:112", "E:ID_BUS=usb\nE:SYNODEB_BOOT_FLASH=1\n");
    assert!(scan(&f).is_empty(), "{:#?}", scan(&f));
}

#[test]
fn optical_and_reader_without_medium() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = Fixture::new(dir.path());
    f.optical(
        "sr0",
        "HL-DT-ST DVDRAM",
        Some((
            4_000_000_000,
            Fs {
                fs_type: "iso9660",
                label: "DEBIAN_13",
                uuid: "2025",
            },
        )),
    );
    f.usb_disk("sdc", 32, "2-1", "Generic", "SD Reader", "9", 0, 480, true);
    let drives = scan(&f);
    let sr = drives.iter().find(|d| d.name == "sr0").unwrap();
    assert_eq!(sr.bus, DriveBus::Optical);
    assert_eq!(sr.remove_method, RemoveMethod::Eject);
    assert_eq!(sr.volumes[0].label, "DEBIAN_13");
    let reader = drives.iter().find(|d| d.name == "sdc").unwrap();
    assert!(reader.no_media && reader.volumes.is_empty());
    // a USB card reader is powered off like any USB drive
    assert_eq!(reader.remove_method, RemoveMethod::PowerOff);
}

#[test]
fn signature_and_activity() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = Fixture::demo(dir.path());
    let mut s = Scanner::new(Tree::new(f.root()));
    let first = s.tick(false);
    assert!(!first.iter().any(|d| d.busy));
    // sectors written since the last tick: busy
    f.io("sdc", 9999, 0);
    let d = s.tick(false);
    assert!(d.iter().find(|d| d.name == "sdc").unwrap().busy);
    let d = s.tick(false);
    assert!(!d.iter().find(|d| d.name == "sdc").unwrap().busy);
    // requests in flight: busy
    f.io("sdc", 9999, 2);
    assert!(s.tick(false).iter().find(|d| d.name == "sdc").unwrap().busy);
    // a mount change is seen without forcing
    f.unmount_all();
    let d = s.tick(false);
    assert!(d.iter().all(|d| d.mounted() == 0));
    f.unplug("sdc");
    assert!(!s.tick(false).iter().any(|d| d.name == "sdc"));
}

#[test]
fn mount_chooses_a_path_and_options() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::demo(dir.path());
    let drives = scan(&f);
    let host = Arc::new(FakeHost::new(f.root()));
    let r = Removable::new(host.clone(), None);
    let (log, hook) = (|_: &str| {}, |_: &str| {});
    let m = r.mount(&drives, "sdd1", false, &ctx(&log, &hook)).unwrap();
    assert_eq!(m.mount_point, "/media/Backup");
    let cmds = host.commands();
    assert_eq!(
        cmds,
        [
            "systemd-mount --no-ask-password --collect --fsck=no -t ntfs3 -o nosuid,nodev,noexec,noatime,uid=0,gid=100,fmask=0117,dmask=0007,iocharset=utf8,windows_names /dev/sdd1 /media/Backup"
        ]
    );
    // already mounted: nothing to do
    let m = r.mount(&drives, "sdc1", false, &ctx(&log, &hook)).unwrap();
    assert_eq!(m.mount_point, "/media/SANDISK");
    assert_eq!(host.commands().len(), 1);
    // locked LUKS: refused
    assert!(matches!(
        r.mount(&drives, "sdd2", false, &ctx(&log, &hook)),
        Err(RemovableError::Conflict(_))
    ));
    assert!(matches!(
        r.mount(&drives, "nope", false, &ctx(&log, &hook)),
        Err(RemovableError::NotFound(_))
    ));
}

#[test]
fn dirty_ntfs_is_mounted_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::demo(dir.path());
    let drives = scan(&f);
    let mut host = FakeHost::new(f.root());
    host.no_systemd = true;
    host.answer(
        &["mount", "-t", "ntfs3", "-o", "nosuid,nodev,noexec,noatime,uid=0,gid=100,fmask=0117,dmask=0007,iocharset=utf8,windows_names"],
        Output {
            code: Some(32),
            stderr: "mount: /media/Backup: wrong fs type.\nntfs3: sdd1: volume is dirty and \"force\" flag is not set!\n".into(),
            ..Default::default()
        },
    );
    let host = Arc::new(host);
    let r = Removable::new(host.clone(), None);
    let (log, hook) = (|_: &str| {}, |_: &str| {});
    let m = r.mount(&drives, "sdd1", false, &ctx(&log, &hook)).unwrap();
    assert!(m.read_only && m.ntfs_dirty);
    let cmds = host.commands();
    assert_eq!(cmds.len(), 2);
    assert!(cmds[1].starts_with("mount -t ntfs3 -o ro,"), "{cmds:?}");
}

#[test]
fn unlock_passes_the_passphrase_on_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::demo(dir.path());
    let drives = scan(&f);
    let host = Arc::new(FakeHost::new(f.root()));
    let r = Removable::new(host.clone(), None);
    let lines = std::sync::Mutex::new(Vec::<String>::new());
    let log = |l: &str| lines.lock().unwrap().push(l.to_string());
    let hook = |_: &str| {};
    r.unlock(&drives, "sdd2", "s3cret pass", &ctx(&log, &hook))
        .unwrap();
    let calls = host.calls();
    let Call::Run(argv, stdin) = &calls[0] else {
        panic!()
    };
    assert_eq!(
        argv.join(" "),
        "cryptsetup open --type luks --key-file - /dev/sdd2 luks-5e0d1f62-luks"
    );
    assert_eq!(stdin.as_deref(), Some(b"s3cret pass".as_slice()));
    assert!(!lines.lock().unwrap().join("\n").contains("s3cret"));

    let host = FakeHost::new(f.root());
    host.answer(
        &["cryptsetup"],
        Output {
            code: Some(2),
            stderr: "No key available with this passphrase.".into(),
            ..Default::default()
        },
    );
    let r = Removable::new(Arc::new(host), None);
    assert_eq!(
        r.unlock(&drives, "sdd2", "bad", &ctx(&log, &hook)),
        Err(RemovableError::WrongPassphrase)
    );
    assert!(matches!(
        r.unlock(&drives, "sdd1", "x", &ctx(&log, &hook)),
        Err(RemovableError::Conflict(_))
    ));
}

#[test]
fn lock_unmounts_then_closes() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = Fixture::demo(dir.path());
    f.unlock(
        "sdd",
        "sdd2",
        3,
        Fs {
            fs_type: "ext4",
            label: "Secret",
            uuid: "c1",
        },
    );
    f.mount("254:3", "/media/Secret", "ext4", false);
    let drives = scan(&f);
    let host = Arc::new(FakeHost::new(f.root()));
    let r = Removable::new(host.clone(), None);
    let hooked = std::sync::Mutex::new(Vec::<String>::new());
    let hook = |mp: &str| hooked.lock().unwrap().push(mp.to_string());
    let log = |_: &str| {};
    r.lock(&drives, "sdd2", &ctx(&log, &hook)).unwrap();
    assert_eq!(
        host.commands(),
        [
            "systemd-mount --umount /media/Secret",
            "cryptsetup close luks-3"
        ]
    );
    assert_eq!(*hooked.lock().unwrap(), ["/media/Secret"]);
}

#[test]
fn safe_removal_per_drive_type() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = Fixture::demo(dir.path());
    f.optical(
        "sr0",
        "DVD",
        Some((
            1_000_000,
            Fs {
                fs_type: "iso9660",
                label: "CD",
                uuid: "cd",
            },
        )),
    );
    let drives = scan(&f);
    let (log, hook) = (|_: &str| {}, |_: &str| {});

    // USB: unmount, flush, stop, power off the USB device
    let host = Arc::new(FakeHost::new(f.root()));
    let r = Removable::new(host.clone(), None);
    assert_eq!(
        r.safely_remove(&drives, "sdc", &ctx(&log, &hook)).unwrap(),
        RemoveMethod::PowerOff
    );
    let calls = host.calls();
    assert_eq!(
        calls[0],
        Call::Run(
            vec![
                "systemd-mount".into(),
                "--umount".into(),
                "/media/SANDISK".into()
            ],
            None
        )
    );
    assert!(calls.contains(&Call::RemoveDir("/media/SANDISK".into())));
    assert!(calls.contains(&Call::Flush("/dev/sdc".into())));
    assert!(calls.contains(&Call::ScsiStop("/dev/sdc".into(), false)));
    assert_eq!(
        calls.last().unwrap(),
        &Call::Sysfs(
            "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/remove".into(),
            "1".into()
        )
    );

    // optical: eject
    let host = Arc::new(FakeHost::new(f.root()));
    let r = Removable::new(host.clone(), None);
    assert_eq!(
        r.safely_remove(&drives, "sr0", &ctx(&log, &hook)).unwrap(),
        RemoveMethod::Eject
    );
    assert!(host.calls().contains(&Call::CdromEject("/dev/sr0".into())));

    // SD card: release only
    let host = Arc::new(FakeHost::new(f.root()));
    let r = Removable::new(host.clone(), None);
    assert_eq!(
        r.safely_remove(&drives, "mmcblk0", &ctx(&log, &hook))
            .unwrap(),
        RemoveMethod::Release
    );
    assert!(
        !host
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Sysfs(..) | Call::CdromEject(_)))
    );
}

#[test]
fn card_reader_slots_are_released_together() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = Fixture::new(dir.path());
    // two LUNs of one USB card reader on port 2-1
    f.usb_disk(
        "sdc",
        32,
        "2-1",
        "Generic",
        "CF",
        "R1",
        8_000_000_000,
        480,
        true,
    );
    f.partition(
        "sdc",
        "sdc1",
        8,
        33,
        8_000_000_000,
        Fs {
            fs_type: "vfat",
            label: "CF",
            uuid: "c",
        },
    );
    f.mount("8:33", "/media/CF", "vfat", false);
    f.usb_disk(
        "sdd",
        48,
        "2-1",
        "Generic",
        "SD",
        "R1",
        16_000_000_000,
        480,
        true,
    );
    f.partition(
        "sdd",
        "sdd1",
        8,
        49,
        16_000_000_000,
        Fs {
            fs_type: "vfat",
            label: "SD",
            uuid: "s",
        },
    );
    f.mount("8:49", "/media/SD", "vfat", false);
    let drives = scan(&f);
    let host = Arc::new(FakeHost::new(f.root()));
    let r = Removable::new(host.clone(), None);
    let (log, hook) = (|_: &str| {}, |_: &str| {});
    r.safely_remove(&drives, "sdc", &ctx(&log, &hook)).unwrap();
    let cmds = host.commands();
    assert_eq!(
        cmds,
        [
            "systemd-mount --umount /media/CF",
            "systemd-mount --umount /media/SD"
        ]
    );
}

#[test]
fn missing_cryptsetup_is_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::demo(dir.path());
    let drives = scan(&f);
    let mut host = FakeHost::new(f.root());
    host.missing = vec!["cryptsetup".into()];
    let r = Removable::new(Arc::new(host), None);
    assert!(matches!(
        r.check_unlock(&drives, "sdd2"),
        Err(RemovableError::Unsupported(_))
    ));
}
