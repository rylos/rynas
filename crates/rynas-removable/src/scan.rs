//! What is plugged in, read straight from the kernel and the udev database (no command):
//!
//! - `/sys/block/*`: disks, partitions, sizes, the removable flag, holders (LUKS), I/O;
//! - `/run/udev/data/b<major>:<minor>`: bus, vendor and model, filesystem type, label, UUID;
//! - `/proc/self/mountinfo`: what is mounted where, matched by device number, so encrypted
//!   volumes are followed to their cleartext device;
//! - `/proc/swaps`: swap partitions, never offered for removal;
//! - the USB device in sysfs (found by walking up from the disk): link speed and version.
//!
//! Every path is read below a root (`/`, or a fake tree in tests and screenshots). A
//! [`Scanner`] runs a cheap check on every tick (device list, mount table, sizes, udev entry
//! times) and the full scan only when that signature changes.
//!
//! What counts as removable: disks on the USB bus, SD cards (`mmcblk` with device type `SD`;
//! eMMC is the machine's own storage), optical drives, and anything the kernel flags as
//! removable. A drive is never listed when it holds a system mount point (`/`, `/boot*`,
//! `/efi*`, `/home`, `/usr`, `/var`), an active swap, a RAID or LVM member, a filesystem of
//! `/etc/fstab`, or when udev marks it `UDISKS_IGNORE=1` / `UDISKS_SYSTEM=1` (plus the boot
//! flash of some appliances, identified by its USB ids).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rynas_api_types::removable::{
    ClearDevice, DriveBus, RemovableDrive, RemovableVolume, RemoveMethod, UsbLink,
};

/// One mounted filesystem of the mount table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub majmin: String,
    pub path: String,
    pub fs_type: String,
    pub read_only: bool,
}

/// Root-relative file access.
#[derive(Debug, Clone)]
pub struct Tree {
    root: PathBuf,
}

impl Tree {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `path` (absolute) below the root.
    pub fn path(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    pub fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.path(path))
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn read_raw(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.path(path)).ok()
    }

    fn number(&self, path: &str) -> u64 {
        self.read(path).and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    pub fn exists(&self, path: &str) -> bool {
        self.path(path).exists()
    }

    /// Entry names, sorted.
    pub fn list(&self, path: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.path(path))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        v.sort();
        v
    }

    fn mtime(&self, path: &str) -> Option<i64> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(self.path(path))
            .ok()
            .map(|m| m.mtime_nsec() + m.mtime() * 1_000_000_000)
    }
}

/// Undoes the `\xNN` escapes of udev's `*_ENC` properties.
pub fn decode_udev(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1] == b'x'
            && let Ok(v) = u8::from_str_radix(&s[i + 2..i + 4], 16)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Undoes the octal escapes of mountinfo (`\040` = space).
pub fn decode_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `E:KEY=value` lines of a udev database entry.
pub fn udev_props(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("E:")?.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// `major:minor` → its mounts, in mount table order.
pub fn parse_mounts(text: &str) -> HashMap<String, Vec<MountEntry>> {
    let mut out: HashMap<String, Vec<MountEntry>> = HashMap::new();
    for line in text.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let l: Vec<&str> = left.split(' ').collect();
        let r: Vec<&str> = right.split(' ').collect();
        if l.len() < 6 || r.is_empty() {
            continue;
        }
        let e = MountEntry {
            majmin: l[2].to_string(),
            path: decode_mount(l[4]),
            fs_type: r[0].to_string(),
            read_only: l[5].split(',').any(|o| o == "ro"),
        };
        out.entry(e.majmin.clone()).or_default().push(e);
    }
    out
}

/// Kernel names of the active swap devices.
pub fn parse_swaps(text: &str) -> HashSet<String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("/dev/")?.split_whitespace().next())
        .map(|d| d.rsplit('/').next().unwrap_or(d).to_string())
        .collect()
}

/// `UUID=…`, `LABEL=…`, `PARTUUID=…` and `/dev/…` sources of `/etc/fstab`.
pub fn parse_fstab(text: &str) -> HashSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_whitespace().next())
        .map(|s| decode_mount(s).trim_matches('"').to_string())
        .collect()
}

/// Mount points that belong to the running system.
pub fn is_system_mount(path: &str) -> bool {
    matches!(path, "/" | "/home" | "/usr" | "/var")
        || path.starts_with("/boot")
        || path.starts_with("/efi")
}

/// Block devices that are never drives (virtual, md, dm, eMMC boot partitions).
pub fn candidate_name(name: &str) -> bool {
    !(name.starts_with("loop")
        || name.starts_with("ram")
        || name.starts_with("zram")
        || name.starts_with("dm-")
        || name.starts_with("md")
        || name.starts_with("nbd")
        || name.starts_with("nvme")
        || name.ends_with("rpmb")
        || name
            .strip_prefix("mmcblk")
            .is_some_and(|r| r.contains("boot")))
}

fn pretty(s: &str) -> String {
    s.replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// USB vendor:product ids of internal boot flash modules that sit on the USB bus (they hold
/// the firmware's boot loader and must never be powered off).
const BOOT_FLASH_IDS: &[(&str, &str)] = &[("f400", "f400")];

/// Filesystem types rynas mounts (when a driver or helper is installed).
pub const KNOWN_FS: &[&str] = &[
    "vfat", "exfat", "ntfs", "ext2", "ext3", "ext4", "btrfs", "xfs", "f2fs", "hfsplus", "iso9660",
    "udf",
];

/// The scanner's view of the system: which filesystem drivers exist.
#[derive(Debug, Clone, Default)]
pub struct FsSupport {
    /// `/proc/filesystems` names.
    pub kernel: HashSet<String>,
    /// Loadable modules found under `/lib/modules/<release>/kernel/fs`.
    pub modules: HashSet<String>,
    /// `mount.<type>` helpers (`mount.ntfs-3g`, `mount.exfat-fuse`).
    pub helpers: HashSet<String>,
}

impl FsSupport {
    pub fn read(t: &Tree) -> Self {
        let kernel = t
            .read("/proc/filesystems")
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_whitespace().last())
            .map(str::to_string)
            .collect();
        let release = t.read("/proc/sys/kernel/osrelease").unwrap_or_default();
        let modules = if release.is_empty() {
            HashSet::new()
        } else {
            t.list(&format!("/lib/modules/{release}/kernel/fs"))
                .into_iter()
                .collect()
        };
        let mut helpers = HashSet::new();
        for dir in ["/sbin", "/usr/sbin", "/bin", "/usr/bin"] {
            for f in t.list(dir) {
                if let Some(h) = f.strip_prefix("mount.") {
                    helpers.insert(h.to_string());
                }
            }
        }
        Self {
            kernel,
            modules,
            helpers,
        }
    }

    fn has(&self, driver: &str) -> bool {
        self.kernel.contains(driver)
            || self.modules.contains(driver)
            || self.helpers.contains(driver)
    }

    /// The kernel driver or helper type to mount `fs` with, `None` when there is none.
    /// NTFS: the kernel's ntfs3 first, ntfs-3g (FUSE) as the fallback.
    pub fn driver(&self, fs: &str) -> Option<&'static str> {
        let candidates: &[&'static str] = match fs {
            "ntfs" => &["ntfs3", "ntfs-3g"],
            "exfat" => &["exfat", "exfat-fuse"],
            "vfat" => &["vfat"],
            "ext2" | "ext3" => &["ext4", "ext2", "ext3"],
            "ext4" => &["ext4"],
            "btrfs" => &["btrfs"],
            "xfs" => &["xfs"],
            "f2fs" => &["f2fs"],
            "hfsplus" => &["hfsplus"],
            "iso9660" => &["iso9660", "isofs"],
            "udf" => &["udf"],
            _ => &[],
        };
        let found = candidates.iter().copied().find(|d| self.has(d))?;
        // the module directory of iso9660 is called isofs, the type stays iso9660
        Some(if found == "isofs" { "iso9660" } else { found })
    }
}

/// Classification of one block device instance (`diskseq` changes with every new device,
/// so a reused name is never mistaken for the previous drive).
#[derive(Debug, Clone)]
struct Classified {
    seq: String,
    candidate: bool,
}

/// Turns the system's block devices into [`RemovableDrive`]s.
pub struct Scanner {
    tree: Tree,
    classified: HashMap<String, Classified>,
    last_signature: Option<String>,
    drives: Vec<RemovableDrive>,
    last_written: HashMap<String, u64>,
    support: FsSupport,
}

impl Scanner {
    pub fn new(tree: Tree) -> Self {
        let support = FsSupport::read(&tree);
        Self {
            tree,
            classified: HashMap::new(),
            last_signature: None,
            drives: Vec::new(),
            last_written: HashMap::new(),
            support,
        }
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn support(&self) -> &FsSupport {
        &self.support
    }

    /// Reads the filesystem drivers again (after a package was installed).
    pub fn refresh_support(&mut self) {
        self.support = FsSupport::read(&self.tree);
    }

    fn udev(&self, majmin: &str) -> HashMap<String, String> {
        self.tree
            .read_raw(&format!("/run/udev/data/b{majmin}"))
            .map(|t| udev_props(&t))
            .unwrap_or_default()
    }

    fn bus_of(&self, name: &str, props: &HashMap<String, String>) -> Option<DriveBus> {
        if props.get("ID_BUS").map(String::as_str) == Some("usb")
            || props.contains_key("ID_USB_DRIVER")
            || props.get("ID_PATH").is_some_and(|p| p.contains("-usb-"))
        {
            return Some(DriveBus::Usb);
        }
        if name.starts_with("mmcblk") {
            // only SD cards: eMMC is the machine's own storage
            return (self
                .tree
                .read(&format!("/sys/block/{name}/device/type"))
                .as_deref()
                == Some("SD"))
            .then_some(DriveBus::Sd);
        }
        if name.starts_with("sr") && name[2..].chars().all(|c| c.is_ascii_digit()) {
            return Some(DriveBus::Optical);
        }
        None
    }

    fn classify(&mut self, name: &str) -> bool {
        let sys = format!("/sys/block/{name}");
        let seq = self
            .tree
            .read(&format!("{sys}/diskseq"))
            .unwrap_or_default();
        if let Some(c) = self.classified.get(name)
            && c.seq == seq
        {
            return c.candidate;
        }
        let Some(majmin) = self.tree.read(&format!("{sys}/dev")) else {
            return false;
        };
        // udev writes its entry a moment after the kernel creates the device: until then
        // nothing is cached and the next tick looks again
        if !self.tree.exists(&format!("/run/udev/data/b{majmin}")) {
            return false;
        }
        let props = self.udev(&majmin);
        let candidate = !ignored(&props)
            && (self.bus_of(name, &props).is_some()
                || self.tree.number(&format!("{sys}/removable")) == 1);
        self.classified
            .insert(name.to_string(), Classified { seq, candidate });
        candidate
    }

    /// What a full scan depends on, read cheaply.
    fn signature(
        &self,
        names: &[String],
        candidates: &[String],
        mountinfo: &str,
        swaps: &str,
    ) -> String {
        let mut parts = vec![names.join(" "), mountinfo.to_string(), swaps.to_string()];
        for name in candidates {
            let sys = format!("/sys/block/{name}");
            parts.push(name.clone());
            parts.push(
                self.tree
                    .read(&format!("{sys}/diskseq"))
                    .unwrap_or_default(),
            );
            parts.push(self.tree.read(&format!("{sys}/size")).unwrap_or_default());
            parts.push(self.tree.read(&format!("{sys}/ro")).unwrap_or_default());
            let majmin = self.tree.read(&format!("{sys}/dev")).unwrap_or_default();
            parts.push(format!(
                "{:?}",
                self.tree.mtime(&format!("/run/udev/data/b{majmin}"))
            ));
            for entry in self.tree.list(&sys) {
                if !entry.starts_with(name.as_str()) {
                    continue;
                }
                parts.push(entry.clone());
                if let Some(mm) = self.tree.read(&format!("{sys}/{entry}/dev")) {
                    parts.push(format!(
                        "{:?}",
                        self.tree.mtime(&format!("/run/udev/data/b{mm}"))
                    ));
                }
                parts.extend(self.tree.list(&format!("{sys}/{entry}/holders")));
            }
            parts.extend(self.tree.list(&format!("{sys}/holders")));
        }
        parts.join("\n")
    }

    /// One tick: the cheap signature, the full scan when it changed (or `force`), then the
    /// live values (write activity, usage of mounted volumes). Returns the drives.
    pub fn tick(&mut self, force: bool) -> Vec<RemovableDrive> {
        let names: Vec<String> = self
            .tree
            .list("/sys/block")
            .into_iter()
            .filter(|n| candidate_name(n))
            .collect();
        let mut candidates = Vec::new();
        for n in &names {
            if self.classify(n) {
                candidates.push(n.clone());
            }
        }
        self.classified.retain(|n, _| names.contains(n));
        let mountinfo = self
            .tree
            .read_raw("/proc/self/mountinfo")
            .unwrap_or_default();
        let swaps = self.tree.read_raw("/proc/swaps").unwrap_or_default();
        let sig = self.signature(&names, &candidates, &mountinfo, &swaps);
        if force || self.last_signature.as_deref() != Some(sig.as_str()) {
            self.last_signature = Some(sig);
            let mounts = parse_mounts(&mountinfo);
            let swaps = parse_swaps(&swaps);
            let fstab = parse_fstab(&self.tree.read_raw("/etc/fstab").unwrap_or_default());
            self.drives = candidates
                .iter()
                .filter_map(|n| self.scan_drive(n, &mounts, &swaps, &fstab))
                .collect();
            let present: HashSet<&str> = self.drives.iter().map(|d| d.name.as_str()).collect();
            self.last_written
                .retain(|n, _| present.contains(n.as_str()));
        }
        self.refresh_live();
        self.drives.clone()
    }

    /// The drives of the last scan (live values of the last tick).
    pub fn drives(&self) -> &[RemovableDrive] {
        &self.drives
    }

    fn refresh_live(&mut self) {
        let mut drives = std::mem::take(&mut self.drives);
        for d in &mut drives {
            let stat = self
                .tree
                .read(&format!("/sys/block/{}/stat", d.name))
                .unwrap_or_default();
            let f: Vec<u64> = stat
                .split_whitespace()
                .filter_map(|v| v.parse().ok())
                .collect();
            let written = f.get(6).copied().unwrap_or(0);
            let in_flight = f.get(8).copied().unwrap_or(0);
            d.busy = in_flight > 0
                || self
                    .last_written
                    .get(&d.name)
                    .is_some_and(|w| *w != written);
            self.last_written.insert(d.name.clone(), written);
            for v in &mut d.volumes {
                (v.total_bytes, v.used_bytes, v.avail_bytes) = (None, None, None);
                if let Some(mp) = &v.mount_point
                    && let Some((total, free, avail)) = statvfs(&self.tree.path(mp))
                    && total > 0
                {
                    v.total_bytes = Some(total);
                    v.used_bytes = Some(total.saturating_sub(free));
                    v.avail_bytes = Some(avail);
                }
            }
        }
        self.drives = drives;
    }

    fn scan_drive(
        &self,
        name: &str,
        mounts: &HashMap<String, Vec<MountEntry>>,
        swaps: &HashSet<String>,
        fstab: &HashSet<String>,
    ) -> Option<RemovableDrive> {
        let t = &self.tree;
        let sys = format!("/sys/block/{name}");
        let majmin = t.read(&format!("{sys}/dev"))?;
        let props = self.udev(&majmin);
        let bus = self.bus_of(name, &props);
        let removable = t.number(&format!("{sys}/removable")) == 1;
        if (bus.is_none() && !removable) || ignored(&props) {
            return None;
        }
        if let (Some(v), Some(m)) = (props.get("ID_VENDOR_ID"), props.get("ID_MODEL_ID"))
            && BOOT_FLASH_IDS
                .iter()
                .any(|(bv, bm)| v.eq_ignore_ascii_case(bv) && m.eq_ignore_ascii_case(bm))
        {
            return None;
        }
        let bus = bus.unwrap_or_default();
        let size = t.number(&format!("{sys}/size")) * 512;
        let mut d = RemovableDrive {
            name: name.into(),
            dev_path: format!("/dev/{name}"),
            bus,
            vendor: pretty(
                props
                    .get("ID_VENDOR")
                    .map(String::as_str)
                    .or(t.read(&format!("{sys}/device/vendor")).as_deref())
                    .unwrap_or(""),
            ),
            model: pretty(
                props
                    .get("ID_MODEL")
                    .map(String::as_str)
                    .or(t.read(&format!("{sys}/device/model")).as_deref())
                    .unwrap_or(""),
            ),
            serial: props.get("ID_SERIAL_SHORT").cloned().unwrap_or_default(),
            size_bytes: size,
            removable_media: removable,
            no_media: size == 0,
            read_only: t.number(&format!("{sys}/ro")) == 1,
            ..Default::default()
        };
        match bus {
            DriveBus::Usb => d.usb = self.usb_link(name),
            // SD cards report their product name in the MMC device, no vendor string
            DriveBus::Sd => {
                d.vendor = String::new();
                d.model = pretty(&t.read(&format!("{sys}/device/name")).unwrap_or_default());
            }
            _ => {}
        }
        // power off cuts the USB port; media ejection is for card readers and optical
        // drives, which keep their device when the medium is out
        d.remove_method = if bus == DriveBus::Usb {
            RemoveMethod::PowerOff
        } else if removable && !d.no_media {
            RemoveMethod::Eject
        } else {
            RemoveMethod::Release
        };

        let mut system = t
            .list(&format!("{sys}/holders"))
            .iter()
            .any(|h| h.starts_with("md"));
        for entry in t.list(&sys) {
            if !entry.starts_with(name) || !t.exists(&format!("{sys}/{entry}/partition")) {
                continue;
            }
            let part = format!("{sys}/{entry}");
            if swaps.contains(&entry)
                || t.list(&format!("{part}/holders"))
                    .iter()
                    .any(|h| h.starts_with("md"))
            {
                system = true;
            }
            match self.volume_of(&entry, &part, mounts, swaps, fstab) {
                Vol::Some(v) => d.volumes.push(*v),
                Vol::System => system = true,
                Vol::None => {}
            }
        }
        if d.volumes.is_empty() && !d.no_media {
            // a filesystem on the whole disk (superfloppy layout, optical media)
            match self.volume_of(name, &sys, mounts, swaps, fstab) {
                Vol::Some(v) => d.volumes.push(*v),
                Vol::System => system = true,
                Vol::None => {}
            }
        }
        if system
            || d.volumes
                .iter()
                .any(|v| v.mount_point.as_deref().is_some_and(is_system_mount))
        {
            return None;
        }
        // ATA bridges often repeat the vendor at the start of the model
        let model_has_vendor =
            !d.vendor.is_empty() && d.model.to_lowercase().starts_with(&d.vendor.to_lowercase());
        d.title = if !d.vendor.is_empty() && !model_has_vendor {
            format!("{} {}", d.vendor, d.model).trim().to_string()
        } else {
            d.model.clone()
        };
        if d.title.trim().is_empty() {
            d.title = d.dev_path.clone();
        }
        Some(d)
    }

    /// The USB device above the disk: the first directory up the physical sysfs path with
    /// `idVendor` (`..` after a sysfs symlink resolves on the physical path).
    fn usb_link(&self, name: &str) -> Option<UsbLink> {
        let dir = usb_device_dir(&self.tree, name)?;
        let speed = std::fs::read_to_string(dir.join("speed"))
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok())
            .unwrap_or(0.0);
        Some(UsbLink {
            speed_mbps: speed.ceil() as u32,
            version: std::fs::read_to_string(dir.join("version"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default(),
        })
    }

    fn volume_of(
        &self,
        name: &str,
        sys: &str,
        mounts: &HashMap<String, Vec<MountEntry>>,
        swaps: &HashSet<String>,
        fstab: &HashSet<String>,
    ) -> Vol {
        let t = &self.tree;
        let Some(majmin) = t.read(&format!("{sys}/dev")) else {
            return Vol::None;
        };
        let props = self.udev(&majmin);
        let fs = props.get("ID_FS_TYPE").cloned().unwrap_or_default();
        let usage = props.get("ID_FS_USAGE").map(String::as_str).unwrap_or("");
        if usage == "raid" || fs.ends_with("_member") {
            return Vol::System;
        }
        if fs.is_empty() || fs == "swap" || swaps.contains(name) {
            return Vol::None;
        }
        if usage != "filesystem" && usage != "crypto" {
            return Vol::None;
        }
        let uuid = props.get("ID_FS_UUID").cloned().filter(|u| !u.is_empty());
        let label = label_of(&props);
        let in_fstab = uuid
            .as_ref()
            .is_some_and(|u| fstab.contains(&format!("UUID={u}")))
            || (!label.is_empty() && fstab.contains(&format!("LABEL={label}")))
            || props
                .get("ID_PART_ENTRY_UUID")
                .is_some_and(|u| fstab.contains(&format!("PARTUUID={u}")))
            || fstab.contains(&format!("/dev/{name}"));
        if in_fstab {
            return Vol::System;
        }
        let luks = fs == "crypto_LUKS";
        let mut v = RemovableVolume {
            name: name.into(),
            dev_path: format!("/dev/{name}"),
            label,
            supported: luks || self.support.driver(&fs).is_some(),
            fs_type: fs,
            uuid,
            size_bytes: t.number(&format!("{sys}/size")) * 512,
            luks,
            ..Default::default()
        };
        let mut mounted = mounts.get(&majmin).cloned().unwrap_or_default();
        if luks {
            // an unlocked container has the dm device mapping it as a holder
            if let Some(holder) = t
                .list(&format!("{sys}/holders"))
                .into_iter()
                .find(|h| h.starts_with("dm-"))
            {
                let csys = format!("/sys/block/{holder}");
                let cmm = t.read(&format!("{csys}/dev")).unwrap_or_default();
                let cprops = self.udev(&cmm);
                let cfs = cprops.get("ID_FS_TYPE").cloned().unwrap_or_default();
                v.unlocked = true;
                v.supported = self.support.driver(&cfs).is_some() || cfs.is_empty();
                v.clear = Some(ClearDevice {
                    name: holder.clone(),
                    dev_path: format!("/dev/{holder}"),
                    label: label_of(&cprops),
                    fs_type: cfs,
                });
                mounted = mounts.get(&cmm).cloned().unwrap_or_default();
            }
        }
        if let Some(m) = mounted.first() {
            v.mount_point = Some(m.path.clone());
            v.read_only = m.read_only;
        }
        Vol::Some(Box::new(v))
    }
}

enum Vol {
    Some(Box<RemovableVolume>),
    /// Belongs to the system (RAID/LVM member, in fstab): the drive is not offered.
    System,
    None,
}

fn ignored(props: &HashMap<String, String>) -> bool {
    props.get("UDISKS_IGNORE").map(String::as_str) == Some("1")
        || props.get("UDISKS_SYSTEM").map(String::as_str) == Some("1")
}

fn label_of(props: &HashMap<String, String>) -> String {
    props
        .get("ID_FS_LABEL_ENC")
        .map(|l| decode_udev(l))
        .or_else(|| props.get("ID_FS_LABEL").cloned())
        .unwrap_or_default()
}

/// The sysfs directory of the USB device the disk `name` hangs off.
pub fn usb_device_dir(t: &Tree, name: &str) -> Option<PathBuf> {
    let mut path = t.path(&format!("/sys/block/{name}/device"));
    for _ in 0..8 {
        path.push("..");
        if path.join("idVendor").exists() {
            return std::fs::canonicalize(&path).ok().or(Some(path));
        }
    }
    None
}

/// `(total, free, available)` bytes of the filesystem at `path`.
pub fn statvfs(path: &Path) -> Option<(u64, u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `s` a properly sized out buffer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let f = s.f_frsize as u64;
    Some((
        s.f_blocks as u64 * f,
        s.f_bfree as u64 * f,
        s.f_bavail as u64 * f,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes() {
        assert_eq!(decode_udev(r"My\x20Stick"), "My Stick");
        assert_eq!(decode_udev(r"a\xzz"), r"a\xzz");
        assert_eq!(decode_mount(r"/media/My\040Stick"), "/media/My Stick");
    }

    #[test]
    fn mounts_and_swaps() {
        let m = parse_mounts(
            "36 1 8:33 / /media/My\\040Stick rw,nosuid,nodev,noexec,relatime shared:1 - vfat /dev/sdc1 rw,fmask=0117\n\
             37 1 254:3 / /media/Secret ro,relatime - ext4 /dev/mapper/x ro\n",
        );
        assert_eq!(m["8:33"][0].path, "/media/My Stick");
        assert!(!m["8:33"][0].read_only);
        assert!(m["254:3"][0].read_only);
        let s = parse_swaps(
            "Filename Type Size Used Priority\n/dev/sdb2 partition 1 0 -2\n/dev/zram0 partition 1 0 100\n",
        );
        assert!(s.contains("sdb2") && s.contains("zram0"));
        assert!(parse_fstab("# x\nUUID=abcd / btrfs defaults 0 0\n").contains("UUID=abcd"));
    }

    #[test]
    fn names() {
        for n in ["sda", "sdc", "mmcblk0", "sr0"] {
            assert!(candidate_name(n), "{n}");
        }
        for n in [
            "loop0",
            "dm-1",
            "md127",
            "zram0",
            "mmcblk0boot0",
            "mmcblk0rpmb",
            "nvme0n1",
        ] {
            assert!(!candidate_name(n), "{n}");
        }
        assert!(
            is_system_mount("/boot/efi") && is_system_mount("/") && !is_system_mount("/media/x")
        );
    }

    #[test]
    fn drivers() {
        let s = FsSupport {
            kernel: ["vfat", "ext4", "iso9660"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            modules: ["ntfs3", "exfat"].iter().map(|s| s.to_string()).collect(),
            helpers: ["ntfs-3g"].iter().map(|s| s.to_string()).collect(),
        };
        assert_eq!(s.driver("ntfs"), Some("ntfs3"));
        assert_eq!(s.driver("exfat"), Some("exfat"));
        assert_eq!(s.driver("ext3"), Some("ext4"));
        assert_eq!(s.driver("hfsplus"), None);
        assert_eq!(s.driver("apfs"), None);
        let only3g = FsSupport {
            helpers: ["ntfs-3g"].iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        assert_eq!(only3g.driver("ntfs"), Some("ntfs-3g"));
    }
}
