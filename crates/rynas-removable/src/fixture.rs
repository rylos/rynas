//! Fake sysfs / udev / procfs trees with removable drives, for tests (this crate's and
//! rynasd's) and for screenshots of the web interface (`rynasd --hw-root`).
//!
//! The layout follows the kernel's: `/sys/block/<disk>/device` is a symlink into
//! `/sys/devices/…` below the USB device (which has `idVendor`, `speed`, `version`), so the
//! scanner walks up the physical path as on a real machine.

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

/// A filesystem on a partition (or the whole disk).
#[derive(Debug, Clone, Default)]
pub struct Fs<'a> {
    pub fs_type: &'a str,
    pub label: &'a str,
    pub uuid: &'a str,
}

pub struct Fixture {
    root: PathBuf,
    seq: u32,
    mountinfo: Vec<String>,
}

fn write(p: &Path, s: &str) {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).expect("fixture dir");
    }
    std::fs::write(p, s).expect("fixture file");
}

impl Fixture {
    /// An empty machine: no drive, the usual filesystem drivers.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let f = Self {
            root: root.into(),
            seq: 10,
            mountinfo: vec!["22 1 0:21 / / rw,relatime shared:1 - btrfs /dev/md0 rw".into()],
        };
        std::fs::create_dir_all(f.p("/sys/block")).expect("fixture dir");
        std::fs::create_dir_all(f.p("/run/udev/data")).expect("fixture dir");
        f.put(
            "/proc/filesystems",
            "nodev\tproc\n\text4\n\tvfat\n\tbtrfs\n\tiso9660\n\texfat\n\tntfs3\n\tudf\n",
        );
        f.put("/proc/swaps", "Filename\tType\tSize\tUsed\tPriority\n");
        f.put("/etc/fstab", "UUID=root-uuid / btrfs defaults 0 0\n");
        f.flush_mounts();
        f
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn p(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    pub fn put(&self, path: &str, content: &str) {
        write(&self.p(path), content);
    }

    fn flush_mounts(&self) {
        let mut s = self.mountinfo.join("\n");
        s.push('\n');
        self.put("/proc/self/mountinfo", &s);
    }

    fn udev(&self, majmin: &str, lines: &[(&str, &str)]) {
        let mut s = String::new();
        for (k, v) in lines {
            s.push_str(&format!("E:{k}={v}\n"));
        }
        self.put(&format!("/run/udev/data/b{majmin}"), &s);
    }

    fn disk_attrs(&mut self, name: &str, majmin: &str, size: u64, removable: bool, ro: bool) {
        self.seq += 1;
        let b = format!("/sys/block/{name}");
        self.put(&format!("{b}/dev"), &format!("{majmin}\n"));
        self.put(&format!("{b}/size"), &format!("{}\n", size / 512));
        self.put(
            &format!("{b}/removable"),
            if removable { "1\n" } else { "0\n" },
        );
        self.put(&format!("{b}/ro"), if ro { "1\n" } else { "0\n" });
        self.put(&format!("{b}/diskseq"), &format!("{}\n", self.seq));
        self.put(
            &format!("{b}/stat"),
            "100 0 800 10 50 0 400 20 0 30 30 0 0 0 0 0 0\n",
        );
        std::fs::create_dir_all(self.p(&format!("{b}/holders"))).expect("fixture dir");
    }

    /// A USB disk `name` (`sdc`, major 8) on USB port `port` (`2-1`) at `speed_mbps`.
    #[allow(clippy::too_many_arguments)]
    pub fn usb_disk(
        &mut self,
        name: &str,
        minor: u32,
        port: &str,
        vendor: &str,
        model: &str,
        serial: &str,
        size: u64,
        speed_mbps: u32,
        card_reader: bool,
    ) {
        let majmin = format!("8:{minor}");
        self.disk_attrs(name, &majmin, size, card_reader, false);
        let usb = format!("/sys/devices/pci0000:00/0000:00:14.0/usb2/{port}");
        self.put(&format!("{usb}/idVendor"), "0781\n");
        self.put(&format!("{usb}/speed"), &format!("{speed_mbps}\n"));
        self.put(
            &format!("{usb}/version"),
            if speed_mbps >= 5000 {
                " 3.20\n"
            } else {
                " 2.00\n"
            },
        );
        // one SCSI host per disk below the interface; LUN = minor / 16
        let lun = minor / 16;
        let scsi = format!("{usb}/{port}:1.0/host6/target6:0:0/6:0:0:{lun}");
        self.put(&format!("{scsi}/vendor"), &format!("{vendor}\n"));
        self.put(&format!("{scsi}/model"), &format!("{model}\n"));
        let link = self.p(&format!("/sys/block/{name}/device"));
        let _ = std::fs::remove_file(&link);
        symlink(self.p(&scsi), link).expect("fixture link");
        self.udev(
            &majmin,
            &[
                ("ID_BUS", "usb"),
                ("ID_VENDOR", &vendor.replace(' ', "_")),
                ("ID_MODEL", &model.replace(' ', "_")),
                ("ID_SERIAL_SHORT", serial),
                ("ID_VENDOR_ID", "0781"),
                ("ID_MODEL_ID", "5581"),
                (
                    "ID_PATH",
                    &format!(
                        "pci-0000:00:14.0-usb-0:{}:1.0-scsi-0:0:0:{lun}",
                        port.trim_start_matches("2-")
                    ),
                ),
            ],
        );
    }

    /// An internal SATA disk (never listed).
    pub fn sata_disk(&mut self, name: &str, minor: u32, size: u64) {
        let majmin = format!("8:{minor}");
        self.disk_attrs(name, &majmin, size, false, false);
        self.udev(&majmin, &[("ID_BUS", "ata"), ("ID_MODEL", "WDC_WD80EFZZ")]);
    }

    /// An SD card in a built-in reader (`mmcblk0`, major 179).
    pub fn sd_card(
        &mut self,
        name: &str,
        minor: u32,
        product: &str,
        size: u64,
        write_protected: bool,
    ) {
        let majmin = format!("179:{minor}");
        self.disk_attrs(name, &majmin, size, false, write_protected);
        let dev = "/sys/devices/pci0000:00/0000:00:1c.0/mmc_host/mmc0/mmc0:aaaa";
        self.put(&format!("{dev}/type"), "SD\n");
        self.put(&format!("{dev}/name"), &format!("{product}\n"));
        let link = self.p(&format!("/sys/block/{name}/device"));
        symlink(self.p(dev), link).expect("fixture link");
        self.udev(&majmin, &[("ID_NAME", product)]);
    }

    /// An optical drive (`sr0`, major 11); `medium` with a filesystem.
    pub fn optical(&mut self, name: &str, model: &str, medium: Option<(u64, Fs)>) {
        let majmin = "11:0".to_string();
        let size = medium.as_ref().map(|m| m.0).unwrap_or(0);
        self.disk_attrs(name, &majmin, size, true, true);
        let mut lines = vec![
            ("ID_BUS", "ata".to_string()),
            ("ID_MODEL", model.replace(' ', "_")),
            ("ID_CDROM", "1".into()),
        ];
        if let Some((_, fs)) = &medium {
            lines.push(("ID_FS_TYPE", fs.fs_type.into()));
            lines.push(("ID_FS_USAGE", "filesystem".into()));
            lines.push(("ID_FS_LABEL", fs.label.into()));
            lines.push(("ID_FS_UUID", fs.uuid.into()));
        }
        let l: Vec<(&str, &str)> = lines.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.udev(&majmin, &l);
    }

    /// Partition `part` of `disk` with filesystem `fs`; `major` of the disk.
    pub fn partition(&mut self, disk: &str, part: &str, major: u32, minor: u32, size: u64, fs: Fs) {
        let majmin = format!("{major}:{minor}");
        let b = format!("/sys/block/{disk}/{part}");
        self.put(&format!("{b}/dev"), &format!("{majmin}\n"));
        self.put(&format!("{b}/size"), &format!("{}\n", size / 512));
        self.put(&format!("{b}/partition"), "1\n");
        std::fs::create_dir_all(self.p(&format!("{b}/holders"))).expect("fixture dir");
        let usage = if fs.fs_type == "crypto_LUKS" {
            "crypto"
        } else {
            "filesystem"
        };
        let label_enc = fs.label.replace(' ', "\\x20");
        self.udev(
            &majmin,
            &[
                ("ID_FS_TYPE", fs.fs_type),
                ("ID_FS_USAGE", usage),
                ("ID_FS_LABEL", &fs.label.replace(' ', "_")),
                ("ID_FS_LABEL_ENC", &label_enc),
                ("ID_FS_UUID", fs.uuid),
                ("ID_PART_ENTRY_UUID", &format!("{}-part", fs.uuid)),
            ],
        );
    }

    /// The LUKS partition `part` of `disk` unlocked as `dm-<n>` (major 254) with `fs` inside.
    pub fn unlock(&mut self, disk: &str, part: &str, n: u32, fs: Fs) {
        let dm = format!("dm-{n}");
        let majmin = format!("254:{n}");
        std::fs::create_dir_all(self.p(&format!("/sys/block/{disk}/{part}/holders/{dm}")))
            .expect("fixture dir");
        self.put(&format!("/sys/block/{dm}/dev"), &format!("{majmin}\n"));
        self.put(
            &format!("/sys/block/{dm}/dm/name"),
            &format!("luks-{}\n", n),
        );
        self.udev(
            &majmin,
            &[
                ("ID_FS_TYPE", fs.fs_type),
                ("ID_FS_USAGE", "filesystem"),
                ("ID_FS_LABEL", fs.label),
                ("ID_FS_UUID", fs.uuid),
            ],
        );
    }

    /// `majmin` mounted at `path` (a directory is created there too, for `statvfs`).
    pub fn mount(&mut self, majmin: &str, path: &str, fs: &str, read_only: bool) {
        let id = 100 + self.mountinfo.len();
        let esc = path.replace(' ', "\\040");
        self.mountinfo.push(format!(
            "{id} 22 {majmin} / {esc} {},nosuid,nodev,noexec,noatime shared:{id} - {fs} /dev/x {}",
            if read_only { "ro" } else { "rw" },
            if read_only { "ro" } else { "rw" }
        ));
        self.flush_mounts();
        std::fs::create_dir_all(self.p(path)).expect("fixture dir");
    }

    /// Writes into the disk's `stat` (sectors written, requests in flight).
    pub fn io(&self, disk: &str, written: u64, in_flight: u64) {
        self.put(
            &format!("/sys/block/{disk}/stat"),
            &format!("100 0 800 10 50 0 {written} 20 {in_flight} 30 30 0 0 0 0 0 0\n"),
        );
    }

    /// Unplugs a disk (its sysfs directory and udev entry go away).
    pub fn unplug(&self, disk: &str) {
        let _ = std::fs::remove_dir_all(self.p(&format!("/sys/block/{disk}")));
    }

    /// `major:minor` of the block device `name` (a disk, a partition or `dm-N`).
    pub fn majmin(&self, name: &str) -> Option<String> {
        let direct = self.p(&format!("/sys/block/{name}/dev"));
        if let Ok(s) = std::fs::read_to_string(direct) {
            return Some(s.trim().to_string());
        }
        for disk in std::fs::read_dir(self.p("/sys/block")).ok()?.flatten() {
            if let Ok(s) = std::fs::read_to_string(disk.path().join(name).join("dev")) {
                return Some(s.trim().to_string());
            }
        }
        None
    }

    /// Drops the mount at `path`.
    pub fn unmount_path(&mut self, path: &str) {
        let esc = path.replace(' ', "\\040");
        self.mountinfo
            .retain(|l| l.split(' ').nth(4) != Some(esc.as_str()));
        self.flush_mounts();
    }

    /// Unplugs every disk below the sysfs directory `dir` (a USB device powered off).
    pub fn unplug_below(&self, dir: &str) {
        let below = self.p(dir);
        for disk in std::fs::read_dir(self.p("/sys/block"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if let Ok(target) = std::fs::canonicalize(disk.path().join("device"))
                && target.starts_with(std::fs::canonicalize(&below).unwrap_or(below.clone()))
            {
                let _ = std::fs::remove_dir_all(disk.path());
            }
        }
    }

    /// Drops every mount except the root.
    pub fn unmount_all(&mut self) {
        self.mountinfo.truncate(1);
        self.flush_mounts();
    }

    /// A demo machine: a USB 3 stick with a mounted FAT volume, a USB disk with an NTFS
    /// volume and a locked LUKS volume, an SD card. Used by the screenshots.
    pub fn demo(root: impl Into<PathBuf>) -> Self {
        let mut f = Self::new(root);
        f.sata_disk("sda", 0, 8_001_563_222_016);
        f.usb_disk(
            "sdc",
            32,
            "2-1",
            "SanDisk",
            "Ultra Fit",
            "4C530001",
            64_023_257_088,
            5000,
            false,
        );
        f.partition(
            "sdc",
            "sdc1",
            8,
            33,
            64_022_208_512,
            Fs {
                fs_type: "vfat",
                label: "SANDISK",
                uuid: "1A2B-3C4D",
            },
        );
        f.mount("8:33", "/media/SANDISK", "vfat", false);
        f.usb_disk(
            "sdd",
            48,
            "2-2",
            "WD",
            "Elements 25A3",
            "575833314142",
            2_000_365_289_472,
            5000,
            false,
        );
        f.partition(
            "sdd",
            "sdd1",
            8,
            49,
            1_500_000_000_000,
            Fs {
                fs_type: "ntfs",
                label: "Backup",
                uuid: "6A1C2F3E1C2F0A7B",
            },
        );
        f.partition(
            "sdd",
            "sdd2",
            8,
            50,
            500_000_000_000,
            Fs {
                fs_type: "crypto_LUKS",
                label: "",
                uuid: "5e0d1f62-luks",
            },
        );
        f.sd_card("mmcblk0", 0, "SD64G", 63_864_569_856, false);
        f.partition(
            "mmcblk0",
            "mmcblk0p1",
            179,
            1,
            63_863_521_280,
            Fs {
                fs_type: "exfat",
                label: "CAMERA",
                uuid: "9F3A-11B2",
            },
        );
        f
    }
}
