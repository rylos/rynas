//! The cheap, unprivileged storage overview of the dashboard (`GET /api/v1/storage`, served by
//! rynas-web without rynasd): disks from `/sys/block` + the udev database, md arrays from
//! `/proc/mdstat` + sysfs, mounted filesystems from `/proc/self/mountinfo`. No command, no
//! SMART, no ioctl: nothing here wakes a disk, and nothing needs root.
//!
//! It reuses the parsers of the full inventory ([`crate::md::parse_mdstat`],
//! [`crate::md::read_sysfs`], [`crate::mounts::parse_mountinfo`]) and its power-state enum;
//! the answer keeps the `Disk`/`RaidArray`/`Filesystem` shapes of `StorageOverview` (string
//! fields) until the coordinated move to the `DiskSummary`/`MdArray` subsets.

use rynas_api_types::storage::{M2Ref, PowerState};
use rynas_api_types::{Disk, Filesystem, HardwareStatus, RaidArray};

use crate::host::Host;
use crate::md::{StatArray, parse_mdstat, read_sysfs};
use crate::mounts::{Mount, parse_mountinfo};

/// synohwd data older than this is not trusted for the power state.
pub const HW_FRESH_S: u64 = 120;

/// Physical disks as the kernel names them: `sdX`, `vdX`, `xvdX`, `hdX`, `nvmeXnY`,
/// `mmcblkX` (no partitions, md, dm, loop, zram, optical drives).
pub fn is_physical_disk(name: &str) -> bool {
    fn letters_after<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
        name.strip_prefix(prefix)
            .filter(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_lowercase()))
    }
    fn digits(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
    }
    if ["sd", "vd", "xvd", "hd"]
        .iter()
        .any(|p| letters_after(name, p).is_some())
    {
        return true;
    }
    if let Some(r) = name.strip_prefix("nvme") {
        return r
            .split_once('n')
            .is_some_and(|(c, n)| digits(c) && digits(n));
    }
    name.strip_prefix("mmcblk").is_some_and(digits)
}

// ---------------------------------------------------------------- disks

/// Transport from the resolved sysfs path of the block device, falling back to udev's ID_BUS.
pub fn transport(sys_path: &str, name: &str, udev_bus: Option<&str>) -> String {
    let t = if sys_path.contains("/usb") {
        "usb"
    } else if name.starts_with("nvme") || sys_path.contains("/nvme/") {
        "nvme"
    } else if sys_path.contains("/end_device-") || sys_path.contains("/sas_") {
        "sas"
    } else if sys_path.contains("/ata") {
        "sata"
    } else if sys_path.contains("/virtio") {
        "virtio"
    } else if name.starts_with("mmcblk") {
        "mmc"
    } else {
        match udev_bus {
            Some("ata") => "sata",
            Some(b) if !b.is_empty() => b,
            _ => "unknown",
        }
    };
    t.to_string()
}

/// `E:KEY=VALUE` of a udev database file (`/run/udev/data/b<maj>:<min>`).
pub fn udev_prop<'a>(db: &'a str, key: &str) -> Option<&'a str> {
    db.lines()
        .filter_map(|l| l.strip_prefix("E:"))
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
}

/// Facts the hardware provider gives about a disk.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct HwDisk {
    pub bay: Option<u32>,
    /// The M.2 slot of an NVMe disk.
    pub m2: Option<M2Ref>,
    /// Some(true) standby, Some(false) awake, None unknown
    pub asleep: Option<bool>,
}

pub fn hw_disk(hw: Option<&HardwareStatus>, name: &str) -> HwDisk {
    let Some(h) = hw else {
        return HwDisk::default();
    };
    let Some(b) = h.bays.iter().find(|b| b.device.as_deref() == Some(name)) else {
        return HwDisk {
            m2: h
                .m2
                .iter()
                .find(|s| s.present && s.device.as_deref() == Some(name))
                .map(|s| M2Ref { number: s.number }),
            ..HwDisk::default()
        };
    };
    let fresh = h.age_s.is_some_and(|a| a <= HW_FRESH_S);
    HwDisk {
        bay: Some(b.number),
        m2: None,
        asleep: fresh.then_some(b.asleep),
    }
}

/// Power state without touching the disk: recent I/O means awake, else the provider's flag.
pub fn power_state(asleep: Option<bool>, recent_io: bool) -> PowerState {
    match (asleep, recent_io) {
        (_, true) => PowerState::Active,
        (Some(true), false) => PowerState::Standby,
        (Some(false), false) => PowerState::Active,
        (None, false) => PowerState::Unknown,
    }
}

pub fn disks(
    host: &dyn Host,
    hw: Option<&HardwareStatus>,
    recent_io: &dyn Fn(&str) -> bool,
) -> Vec<Disk> {
    let mut names: Vec<String> = host
        .list("/sys/block")
        .into_iter()
        .filter(|n| is_physical_disk(n))
        .collect();
    names.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    let read = |p: String| host.read(&p).map(|s| s.trim().to_string());
    names
        .into_iter()
        .filter_map(|name| {
            let dir = format!("/sys/block/{name}");
            let size = read(format!("{dir}/size"))?.parse::<u64>().ok()? * 512;
            if size == 0 {
                return None; // empty card reader, ejected media
            }
            let udev = read(format!("{dir}/dev"))
                .and_then(|d| host.read(&format!("/run/udev/data/b{d}")))
                .unwrap_or_default();
            let sys_path = host.canonical(&dir).unwrap_or_default();
            let serial = udev_prop(&udev, "ID_SERIAL_SHORT")
                .map(str::to_string)
                .or_else(|| read(format!("{dir}/device/serial")))
                .unwrap_or_default();
            let model = read(format!("{dir}/device/model"))
                .filter(|m| !m.is_empty())
                .or_else(|| udev_prop(&udev, "ID_MODEL").map(|m| m.replace('_', " ")))
                .unwrap_or_default();
            let h = hw_disk(hw, &name);
            Some(Disk {
                transport: transport(&sys_path, &name, udev_prop(&udev, "ID_BUS")),
                rotational: read(format!("{dir}/queue/rotational")).as_deref() == Some("1"),
                power_state: power_state(h.asleep, recent_io(&name)),
                bay: h.bay,
                model,
                serial,
                size,
                name,
            })
        })
        .collect()
}

// ---------------------------------------------------------------- md arrays

/// The overview's one-word state of an array: the running action if any ("resyncing",
/// "recovering", "checking"...; "resync-delayed" while waiting), else "inactive",
/// "degraded", or the sysfs `array_state` ("clean", "active", "read-auto"...).
fn overview_state(a: &StatArray, array_state: Option<&str>) -> String {
    if let Some(s) = &a.sync {
        match (s.pct, &s.waiting) {
            (Some(_), _) => {
                return match s.action.as_str() {
                    "resync" => "resyncing",
                    "recovery" => "recovering",
                    "check" => "checking",
                    "repair" => "repairing",
                    "reshape" => "reshaping",
                    other => other,
                }
                .to_string();
            }
            (None, Some(w)) => return format!("{}-{w}", s.action),
            (None, None) => {}
        }
    }
    if !a.active {
        return "inactive".into();
    }
    let degraded =
        a.members.iter().any(|m| m.faulty) || a.map.as_deref().is_some_and(|m| m.contains('_'));
    if degraded {
        return "degraded".into();
    }
    match array_state {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "active".into(),
    }
}

/// The overview arrays from the text of `/proc/mdstat` and sysfs `array_state`.
pub fn arrays_from(mdstat: &str, array_state: &dyn Fn(&str) -> Option<String>) -> Vec<RaidArray> {
    parse_mdstat(mdstat)
        .iter()
        .map(|a| {
            let st = if a.active && a.sync.is_none() {
                array_state(&a.name)
            } else {
                None
            };
            RaidArray {
                name: a.name.clone(),
                level: a.level.clone(),
                size: a.blocks * 1024,
                members: a.members.iter().map(|m| m.device.clone()).collect(),
                state: overview_state(a, st.as_deref()),
                sync_pct: a.sync.as_ref().and_then(|s| s.pct),
            }
        })
        .collect()
}

pub fn arrays(host: &dyn Host) -> Vec<RaidArray> {
    arrays_from(&host.read("/proc/mdstat").unwrap_or_default(), &|md| {
        read_sysfs(host, md).array_state
    })
}

// ---------------------------------------------------------------- filesystems

const PSEUDO_FS: &[&str] = &[
    "proc",
    "sysfs",
    "tmpfs",
    "devtmpfs",
    "devpts",
    "cgroup",
    "cgroup2",
    "overlay",
    "squashfs",
    "autofs",
    "mqueue",
    "hugetlbfs",
    "debugfs",
    "tracefs",
    "securityfs",
    "pstore",
    "bpf",
    "configfs",
    "fusectl",
    "binfmt_misc",
    "efivarfs",
    "ramfs",
    "rpc_pipefs",
    "nsfs",
    "selinuxfs",
    "nfsd",
    "fuse",
    "iso9660",
    "udf",
    "ecryptfs",
    "zram",
];
const USEFUL_FUSE: &[&str] = &[
    "fuse.mergerfs",
    "fuse.sshfs",
    "fuse.rclone",
    "fuse.glusterfs",
    "fuse.s3fs",
    "fuse.ceph",
];
/// Filesystems whose `statvfs` may hang on a dead server.
pub const NETWORK_FS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "ceph",
    "glusterfs",
    "fuse.sshfs",
    "fuse.rclone",
    "fuse.glusterfs",
    "fuse.s3fs",
    "fuse.ceph",
];

fn shown(m: &Mount) -> bool {
    if PSEUDO_FS.contains(&m.fstype.as_str()) {
        return false;
    }
    if m.fstype.starts_with("fuse.") && !USEFUL_FUSE.contains(&m.fstype.as_str()) {
        return false;
    }
    let under = |p: &str| m.mountpoint == p || m.mountpoint.starts_with(&format!("{p}/"));
    if [
        "/proc",
        "/sys",
        "/dev",
        "/snap",
        "/var/lib/docker",
        "/var/lib/containers",
    ]
    .iter()
    .any(|p| under(p))
    {
        return false;
    }
    !(under("/run") && !under("/run/media"))
}

/// Real filesystems only, one entry per filesystem: bind mounts and btrfs subvolumes of the
/// same superblock (same `major:minor`) collapse into the main mountpoint (root "/" first,
/// else the shortest path).
pub fn select_filesystems(mounts: Vec<Mount>) -> Vec<Mount> {
    let mut chosen: Vec<Mount> = Vec::new();
    for m in mounts.into_iter().filter(shown) {
        match chosen.iter_mut().find(|c| c.majmin == m.majmin) {
            Some(c) => {
                let better = (m.root == "/" && c.root != "/")
                    || ((m.root == "/") == (c.root == "/")
                        && m.mountpoint.len() < c.mountpoint.len());
                if better {
                    *c = m;
                }
            }
            None => chosen.push(m),
        }
    }
    chosen.sort_by(|a, b| a.mountpoint.cmp(&b.mountpoint));
    chosen
}

pub fn mounts(host: &dyn Host) -> Vec<Mount> {
    select_filesystems(parse_mountinfo(
        &host.read("/proc/self/mountinfo").unwrap_or_default(),
    ))
}

/// Sizes of a mounted filesystem in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsSizes {
    pub size: u64,
    pub used: u64,
    pub avail: u64,
}

/// `statvfs(2)`. May block forever on a dead network mount: callers run it on a thread they
/// can abandon (rynas-web's mount prober), never on a shared pool.
pub fn statvfs(path: &str) -> Option<FsSizes> {
    let c = std::ffi::CString::new(path).ok()?;
    // SAFETY: an all-zero statvfs is a valid value of this plain C struct.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid NUL-terminated path and a properly sized out struct.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let f = st.f_frsize as u64;
    let blocks = st.f_blocks as u64;
    Some(FsSizes {
        size: blocks * f,
        used: blocks.saturating_sub(st.f_bfree as u64) * f,
        avail: st.f_bavail as u64 * f,
    })
}

pub fn filesystem(m: &Mount, sizes: Option<FsSizes>) -> Filesystem {
    let s = sizes.unwrap_or_default();
    Filesystem {
        mountpoint: m.mountpoint.clone(),
        source: m.source.clone(),
        fstype: m.fstype.clone(),
        size: s.size,
        used: s.used,
        avail: s.avail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeHost;
    use rynas_api_types::Bay;

    #[test]
    fn mdstat() {
        let v = arrays_from(
            include_str!("../tests/fixtures/overview-mdstat.txt"),
            &|_| None,
        );
        let names: Vec<_> = v.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["md2", "md1", "md0", "md3", "md4"]);
        let md2 = &v[0];
        assert_eq!(md2.level, "raid5");
        // in descriptor order (mdstat lists the newest member first)
        assert_eq!(md2.members, ["sda3", "sdb3", "sdc3", "sdd3"]);
        assert_eq!(md2.size, 11_706_562_560 * 1024);
        assert_eq!(md2.state, "resyncing");
        assert_eq!(md2.sync_pct, Some(8.5));
        assert_eq!(v[1].state, "recovering"); // degraded and rebuilding
        assert_eq!(v[1].sync_pct, Some(41.3));
        assert_eq!(v[2].state, "active");
        assert_eq!(v[2].level, "raid1");
        assert_eq!(v[3].state, "inactive");
        assert_eq!(v[3].members, ["sde1"]);
        assert_eq!(v[4].state, "degraded");
        assert_eq!(v[4].members, ["sdf1", "sdg1"]);
        // sysfs refines a quiet array
        let v = arrays_from(
            include_str!("../tests/fixtures/overview-mdstat.txt"),
            &|md| (md == "md0").then(|| "clean".to_string()),
        );
        assert_eq!(v[2].state, "clean");
    }

    #[test]
    fn mdstat_delayed() {
        let s = "md5 : active raid1 sdb2[1] sda2[0]\n      1048512 blocks [2/2] [UU]\n        resync=DELAYED\n";
        assert_eq!(arrays_from(s, &|_| None)[0].state, "resync-delayed");
    }

    #[test]
    fn mountinfo() {
        let all = parse_mountinfo(include_str!("../tests/fixtures/overview-mountinfo.txt"));
        assert_eq!(all[0].mountpoint, "/");
        let fs = select_filesystems(all);
        let mps: Vec<_> = fs.iter().map(|m| m.mountpoint.as_str()).collect();
        assert_eq!(
            mps,
            [
                "/",
                "/boot/efi",
                "/mnt/backup",
                "/mnt/nas share",
                "/volume1"
            ]
        );
        let vol = fs.iter().find(|m| m.mountpoint == "/volume1").unwrap();
        assert_eq!(vol.source, "/dev/md2");
        assert_eq!(vol.fstype, "btrfs");
        assert_eq!(fs[0].root, "/@");
    }

    #[test]
    fn transports() {
        let ata = "/sys/devices/pci0000:00/0000:00:12.0/ata1/host0/target0:0:0/0:0:0:0/block/sda";
        let usb = "/sys/devices/pci0000:00/0000:00:15.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0/block/sdf";
        let nvme = "/sys/devices/pci0000:00/0000:00:01.3/0000:01:00.0/nvme/nvme0/nvme0n1";
        let virt = "/sys/devices/pci0000:00/0000:00:05.0/virtio2/block/vda";
        assert_eq!(transport(ata, "sda", None), "sata");
        assert_eq!(transport(usb, "sdf", Some("usb")), "usb");
        assert_eq!(transport(nvme, "nvme0n1", None), "nvme");
        assert_eq!(transport(virt, "vda", None), "virtio");
        assert_eq!(
            transport("/sys/devices/x/block/sdz", "sdz", Some("scsi")),
            "scsi"
        );
        assert_eq!(
            transport("/sys/devices/x/block/sdz", "sdz", None),
            "unknown"
        );
    }

    #[test]
    fn udev() {
        let db = "S:disk/by-id/ata-WDC\nE:ID_BUS=ata\nE:ID_SERIAL=WDC_WD40EFRX_WD-ABC123\nE:ID_SERIAL_SHORT=WD-ABC123\n";
        assert_eq!(udev_prop(db, "ID_SERIAL_SHORT"), Some("WD-ABC123"));
        assert_eq!(udev_prop(db, "ID_SERIAL"), Some("WDC_WD40EFRX_WD-ABC123"));
        assert_eq!(udev_prop(db, "ID_BUS"), Some("ata"));
        assert_eq!(udev_prop(db, "ID_MODEL"), None);
    }

    #[test]
    fn power() {
        let mut h = HardwareStatus {
            bays: vec![
                Bay {
                    number: 4,
                    device: Some("sdc".into()),
                    asleep: true,
                    ..Default::default()
                },
                Bay {
                    number: 1,
                    device: Some("sda".into()),
                    asleep: false,
                    ..Default::default()
                },
            ],
            age_s: Some(5),
            ..Default::default()
        };
        let sdc = hw_disk(Some(&h), "sdc");
        assert_eq!(sdc.bay, Some(4));
        assert_eq!(power_state(sdc.asleep, false), PowerState::Standby);
        assert_eq!(
            power_state(hw_disk(Some(&h), "sda").asleep, false),
            PowerState::Active
        );
        assert_eq!(
            power_state(hw_disk(Some(&h), "nvme0n1").asleep, false),
            PowerState::Unknown
        );
        assert_eq!(power_state(None, true), PowerState::Active);
        assert_eq!(hw_disk(Some(&h), "nvme0n1").m2, None);
        h.m2 = vec![
            rynas_api_types::M2Slot {
                number: Some(2),
                present: true,
                device: Some("nvme0n1".into()),
                celsius: None,
            },
            rynas_api_types::M2Slot {
                number: None,
                present: true,
                device: Some("nvme1n1".into()),
                celsius: None,
            },
        ];
        assert_eq!(hw_disk(Some(&h), "nvme0n1").m2, Some(M2Ref { number: Some(2) }));
        assert_eq!(hw_disk(Some(&h), "nvme1n1").m2, Some(M2Ref { number: None }));
        assert_eq!(hw_disk(Some(&h), "sdc").m2, None);
        h.age_s = Some(3600); // stale daemon: do not trust it
        assert_eq!(
            power_state(hw_disk(Some(&h), "sdc").asleep, false),
            PowerState::Unknown
        );
        // the wire words stay those of the former string field
        assert_eq!(
            serde_json::to_value(PowerState::Standby).unwrap(),
            "standby"
        );
    }

    #[test]
    fn disks_from_sysfs() {
        let mut h = FakeHost::default();
        h.file("/sys/block/sda/size", "7814037168\n")
            .file("/sys/block/sda/dev", "8:0\n")
            .file("/sys/block/sda/queue/rotational", "1\n")
            .file("/sys/block/sda/device/model", "WDC WD40EFPX\n")
            .file(
                "/run/udev/data/b8:0",
                "E:ID_BUS=ata\nE:ID_SERIAL_SHORT=WD-1\n",
            )
            .file("/sys/block/sdb/size", "0\n")
            .file("/sys/block/loop0/size", "100\n")
            .file("/sys/block/md0/size", "100\n");
        h.links.insert(
            "/sys/block/sda".into(),
            "/sys/devices/pci0000:00/ata1/host0/block/sda".into(),
        );
        let v = disks(&h, None, &|d| d == "sda");
        assert_eq!(v.len(), 1, "{v:?}");
        let d = &v[0];
        assert_eq!(
            (
                d.name.as_str(),
                d.size,
                d.transport.as_str(),
                d.serial.as_str()
            ),
            ("sda", 7814037168 * 512, "sata", "WD-1")
        );
        assert!(d.rotational);
        assert_eq!(d.power_state, PowerState::Active);
        assert_eq!(d.model, "WDC WD40EFPX");
    }

    #[test]
    fn physical_disk_names() {
        for n in [
            "sda", "sdab", "vda", "xvdb", "nvme0n1", "nvme10n2", "mmcblk0",
        ] {
            assert!(is_physical_disk(n), "{n}");
        }
        for n in [
            "sda1",
            "nvme0n1p1",
            "md0",
            "dm-0",
            "loop0",
            "zram0",
            "sr0",
            "mmcblk0p1",
        ] {
            assert!(!is_physical_disk(n), "{n}");
        }
    }
}
