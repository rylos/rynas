//! A btrfs filesystem of several devices that lost one: what is missing, what the profiles
//! still cover, and the repairs (replace the missing device, remove it, convert the chunks a
//! degraded mount wrote with a weaker profile back), plus the degraded mount of a configured
//! volume that stays unmounted after boot because a device is gone.
//!
//! The reasons and the sources are in `docs/phase2/btrfs-degraded.md`. In short:
//! - a filesystem with a missing device mounts only with `-o degraded`; for a read-write
//!   mount the kernel (since 4.14) checks every chunk and refuses when one has lost more
//!   devices than its profile tolerates, so a degraded read-write mount never runs on
//!   incomplete data;
//! - while degraded, a new chunk whose profile needs more devices than are present is
//!   written `single` (metadata of a three-device raid1c3 on two devices, data of a raid1 on
//!   one): after the repair a soft balance (`-dconvert=<p>,soft -mconvert=<p>,soft`)
//!   converts exactly those chunks back;
//! - systemd waits for every device of a multi-device btrfs (udev's `btrfs ready` keeps the
//!   device units inactive), so the `/etc/fstab` mount unit of such a volume waits forever:
//!   the degraded mount runs `mount -o degraded` itself, in a transient unit;
//! - btrfs does not protect against a split brain: two halves of a mirror each mounted
//!   degraded and written diverge for good. The degraded mounts done here are recorded
//!   ([`Guard`]), and a later one whose devices present are all among those missing then is
//!   refused.
//!
//! Parsers are pure and tested on outputs of btrfs-progs 6.14 (Debian 13), captured on loop
//! devices.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rynas_api_types::storage::{
    BtrfsBlocker, BtrfsConvert, BtrfsFilesystem, BtrfsMember, BtrfsRepair, PlanAction, PlanArgs,
    PlanStep, PlanStepKind, StoragePlan,
};
use serde::{Deserialize, Serialize};

use crate::exec::{Machine, RunOpts};
use crate::lsblk::BlockGraph;
use crate::mounts::Mount;

// ------------------------------------------------------------------------------- parsers

/// One device of `btrfs device usage -b <mnt>`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DevUsage {
    pub devid: u64,
    /// Device path; empty for a missing device.
    pub path: String,
    pub missing: bool,
    /// `Device size` (0 for a missing device: see [`DevUsage::capacity`]).
    pub size: u64,
    /// (kind, profile, bytes): `("data", "raid1", 671088640)`, lower case.
    pub chunks: Vec<(String, String, u64)>,
    pub unallocated: u64,
}

impl DevUsage {
    /// The size of the device: its `Device size`, or for a missing device what it held
    /// (chunks plus unallocated space, which btrfs still knows).
    pub fn capacity(&self) -> u64 {
        if self.size > 0 {
            return self.size;
        }
        self.allocated() + self.unallocated
    }

    pub fn allocated(&self) -> u64 {
        self.chunks.iter().map(|c| c.2).sum()
    }
}

/// `btrfs device usage -b <mnt>`:
///
/// ```text
/// /dev/loop0, ID: 1
///    Device size:          1073741824
///    Data,RAID1:            671088640
///    Unallocated:           125829120
///
/// missing, ID: 3
///    Device size:                   0
/// ```
pub fn parse_device_usage(s: &str) -> Vec<DevUsage> {
    let mut out: Vec<DevUsage> = Vec::new();
    for line in s.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('\t') {
            if let Some((path, id)) = t.rsplit_once(", ID:") {
                let path = path.trim();
                let missing = path == "missing";
                out.push(DevUsage {
                    devid: id.trim().parse().unwrap_or(0),
                    path: if missing { String::new() } else { path.to_string() },
                    missing,
                    ..Default::default()
                });
            }
            continue;
        }
        let Some(d) = out.last_mut() else { continue };
        let Some((k, v)) = t.split_once(':') else { continue };
        let n: u64 = v.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0);
        match k.trim() {
            "Device size" => d.size = n,
            "Unallocated" => d.unallocated = n,
            "Device slack" => {}
            k => {
                if let Some((kind, profile)) = k.split_once(',') {
                    // striped profiles may carry the stripe count: "RAID0/3"
                    let profile = profile.split('/').next().unwrap_or(profile);
                    d.chunks.push((kind.trim().to_ascii_lowercase(), profile.trim().to_ascii_lowercase(), n));
                }
            }
        }
    }
    out
}

/// What `btrfs inspect-internal dump-super <dev>` says about a device.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Super {
    pub fsid: String,
    pub label: Option<String>,
    /// Devices of the filesystem.
    pub num_devices: u32,
    /// This device's id.
    pub devid: u64,
    /// Transaction generation of this copy of the superblock.
    pub generation: u64,
    /// This device's size.
    pub dev_bytes: u64,
}

/// `btrfs inspect-internal dump-super <dev>` (key, then tabs or spaces, then the value).
/// `None` when it is no btrfs superblock.
pub fn parse_dump_super(s: &str) -> Option<Super> {
    let mut sb = Super::default();
    for line in s.lines() {
        let mut w = line.split_whitespace();
        let (Some(k), Some(v)) = (w.next(), w.next()) else { continue };
        let num = || v.parse::<u64>().unwrap_or(0);
        match k {
            "fsid" => sb.fsid = v.to_string(),
            "label" => sb.label = Some(line.trim_start()["label".len()..].trim().to_string()).filter(|l| !l.is_empty()),
            "num_devices" => sb.num_devices = num() as u32,
            "generation" => sb.generation = num(),
            "dev_item.devid" => sb.devid = num(),
            "dev_item.total_bytes" => sb.dev_bytes = num(),
            _ => {}
        }
    }
    (!sb.fsid.is_empty() && sb.devid > 0).then_some(sb)
}

/// One line of `/etc/fstab`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FstabEntry {
    /// `UUID=…`, `LABEL=…`, `/dev/…`.
    pub spec: String,
    pub mountpoint: String,
    pub fstype: String,
    pub options: Vec<String>,
}

/// The entries of `/etc/fstab` (comments and broken lines skipped, `\040` unescaped).
pub fn parse_fstab(s: &str) -> Vec<FstabEntry> {
    s.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let w: Vec<&str> = l.split_whitespace().collect();
            if w.len() < 3 {
                return None;
            }
            Some(FstabEntry {
                spec: w[0].replace("\\040", " "),
                mountpoint: w[1].replace("\\040", " "),
                fstype: w[2].to_string(),
                options: w.get(3).map(|o| o.split(',').map(str::to_string).collect()).unwrap_or_default(),
            })
        })
        .collect()
}

// ------------------------------------------------------------------------------- profiles

/// Devices a chunk of `profile` may lose and still be read (worst case).
pub fn tolerance(profile: &str) -> u32 {
    match profile {
        "raid1" | "raid10" | "raid5" => 1,
        "raid1c3" | "raid6" => 2,
        "raid1c4" => 3,
        // single, dup (both copies on one device), raid0
        _ => 0,
    }
}

/// Fewest devices that hold a chunk of `profile` (the kernel's `devs_min`).
pub fn min_devices(profile: &str) -> u32 {
    match profile {
        "raid1" | "raid10" | "raid5" => 2,
        "raid1c3" | "raid6" => 3,
        "raid1c4" => 4,
        _ => 1,
    }
}

/// Order of preference of the profiles of a kind: the most redundant is the volume's own.
fn strength(profile: &str) -> (u32, u32, bool) {
    (tolerance(profile), min_devices(profile), profile == "dup")
}

/// Chunk kinds with no complete copy on the devices present: `"data single"`, … A profile
/// is lost when a missing device holds one of its chunks and the missing devices outnumber
/// what it tolerates (counted over the whole profile: the worst case of every chunk).
pub fn lost(usage: &[DevUsage]) -> Vec<String> {
    let missing: Vec<&DevUsage> = usage.iter().filter(|d| d.missing).collect();
    let n = missing.len() as u32;
    let mut out: Vec<String> = Vec::new();
    for d in &missing {
        for (kind, profile, bytes) in &d.chunks {
            let name = format!("{kind} {profile}");
            if *bytes > 0 && n > tolerance(profile) && !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

/// The profiles in use per kind, strongest first.
fn profiles_by_kind(usage: &[DevUsage]) -> BTreeMap<String, Vec<String>> {
    let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in usage {
        for (kind, profile, bytes) in &d.chunks {
            let v = m.entry(kind.clone()).or_default();
            if *bytes > 0 && !v.contains(profile) {
                v.push(profile.clone());
            }
        }
    }
    for v in m.values_mut() {
        v.sort_by_key(|p| std::cmp::Reverse(strength(p)));
    }
    m
}

/// Kinds with chunks of a weaker profile beside their own (what a degraded mount wrote):
/// each to convert back to the strongest profile of the kind.
pub fn converts(usage: &[DevUsage]) -> Vec<BtrfsConvert> {
    profiles_by_kind(usage)
        .into_iter()
        .filter_map(|(kind, v)| {
            let to = v.first()?.clone();
            let from: Vec<String> = v.iter().skip(1).filter(|p| strength(p) < strength(&to)).cloned().collect();
            (!from.is_empty()).then_some(BtrfsConvert { kind, from, to })
        })
        .collect()
}

/// Why the missing devices cannot be removed now; `None` when the devices present can take
/// their chunks with every profile kept.
pub fn remove_blocker(usage: &[DevUsage]) -> Option<BtrfsBlocker> {
    if !lost(usage).is_empty() {
        return Some(BtrfsBlocker::Incomplete);
    }
    let present: Vec<&DevUsage> = usage.iter().filter(|d| !d.missing).collect();
    let n = present.len() as u32;
    let needs = profiles_by_kind(usage)
        .values()
        .filter_map(|v| v.first())
        .map(|p| min_devices(p))
        .max()
        .unwrap_or(1);
    if n < needs {
        return Some(BtrfsBlocker::TooFewDevices);
    }
    let moved: u64 = usage.iter().filter(|d| d.missing).map(DevUsage::allocated).sum();
    let free: u64 = present.iter().map(|d| d.unallocated).sum();
    if moved > free {
        return Some(BtrfsBlocker::NoSpace);
    }
    None
}

/// The soft balance that converts `c` back: `-dconvert=raid1,soft -mconvert=raid1c3,soft`;
/// system chunks follow the metadata, or get their own filter (which needs `-f`) when their
/// profile differs.
pub fn convert_argv(mountpoint: &str, c: &[BtrfsConvert]) -> Vec<String> {
    let to = |k: &str| c.iter().find(|x| x.kind == k).map(|x| x.to.clone());
    let mut argv: Vec<String> = ["btrfs", "balance", "start"].map(String::from).into();
    if let Some(p) = to("data") {
        argv.push(format!("-dconvert={p},soft"));
    }
    let meta = to("metadata");
    let sys = to("system");
    match (&meta, &sys) {
        (Some(m), Some(s)) if m != s => {
            argv.push(format!("-mconvert={m},soft"));
            argv.push(format!("-sconvert={s},soft"));
            argv.push("-f".into());
        }
        (Some(m), _) | (None, Some(m)) => argv.push(format!("-mconvert={m},soft")),
        (None, None) => {}
    }
    argv.push(mountpoint.to_string());
    argv
}

/// The repair facts of a mounted filesystem from its device usage; `None` when nothing is
/// missing and no chunk is weaker than its kind.
pub fn repair_of(fs: &BtrfsFilesystem, usage: &[DevUsage]) -> Option<BtrfsRepair> {
    let missing: Vec<&DevUsage> = usage.iter().filter(|d| d.missing).collect();
    let mut ids: Vec<u64> = missing.iter().map(|d| d.devid).collect();
    // `filesystem show` may know a missing device the usage did not print
    for d in fs.devices.iter().filter(|d| d.missing) {
        if !ids.contains(&d.devid) {
            ids.push(d.devid);
        }
    }
    ids.sort_unstable();
    let convert = converts(usage);
    let degraded_mount = fs.mount_options.iter().any(|o| o == "degraded");
    if ids.is_empty() && convert.is_empty() {
        return None;
    }
    let remove = match (missing.is_empty(), ids.is_empty()) {
        (false, _) => remove_blocker(usage),
        // the device usage is not known (a disk sleeps, the command failed)
        (true, false) => Some(BtrfsBlocker::Other),
        (true, true) => None,
    };
    Some(BtrfsRepair {
        mounted: fs.mountpoint.is_some(),
        degraded_mount,
        system: fs.mountpoints.iter().any(|m| m == "/"),
        total_devices: fs.devices.len().max(usage.len()) as u32,
        missing_count: ids.len() as u32,
        missing: ids,
        missing_bytes: missing.iter().map(|d| d.capacity()).max().unwrap_or(0),
        lost: lost(usage),
        convert,
        remove,
        mount: None,
        present: Vec::new(),
    })
}

// ------------------------------------------------------------------- configured volumes

/// Is `p` the mount point of a volume (`/<label>`)?
fn volume_mountpoint(p: &str) -> Option<&str> {
    let l = p.strip_prefix('/')?;
    crate::plan::valid_label(l).then_some(l)
}

/// The filesystem UUID an fstab `spec` names, from the device graph.
fn spec_uuid(spec: &str, g: &BlockGraph) -> Option<String> {
    if let Some(u) = spec.strip_prefix("UUID=") {
        return Some(u.trim_matches('"').to_ascii_lowercase());
    }
    if let Some(l) = spec.strip_prefix("LABEL=") {
        let l = l.trim_matches('"');
        return g
            .devs
            .values()
            .find(|d| d.fstype.as_deref() == Some("btrfs") && d.label.as_deref() == Some(l))
            .and_then(|d| d.uuid.clone());
    }
    if spec.starts_with('/') {
        return g.find(spec).and_then(|d| d.uuid.clone());
    }
    None
}

/// A btrfs volume of `/etc/fstab` (mount point `/<label>`) that is not mounted.
#[derive(Debug, Clone, PartialEq)]
pub struct Unmounted {
    pub uuid: String,
    pub label: Option<String>,
    pub mountpoint: String,
    /// The fstab options.
    pub options: Vec<String>,
    /// Members present: (kernel name, device path).
    pub present: Vec<(String, String)>,
}

/// The btrfs volumes of `fstab` not mounted now: mount point `/<label>`, nothing mounted
/// there, and no filesystem of `mounted` with that UUID.
pub fn unmounted(fstab: &[FstabEntry], g: &BlockGraph, mounts: &[Mount], mounted: &[BtrfsFilesystem]) -> Vec<Unmounted> {
    let mut out: Vec<Unmounted> = Vec::new();
    for e in fstab.iter().filter(|e| e.fstype == "btrfs") {
        if volume_mountpoint(&e.mountpoint).is_none() || mounts.iter().any(|m| m.mountpoint == e.mountpoint) {
            continue;
        }
        let Some(uuid) = spec_uuid(&e.spec, g) else { continue };
        if mounted.iter().any(|f| f.uuid.eq_ignore_ascii_case(&uuid)) || out.iter().any(|u| u.uuid == uuid) {
            continue;
        }
        let mut present: Vec<(String, String)> = g
            .devs
            .values()
            .filter(|d| d.fstype.as_deref() == Some("btrfs") && d.uuid.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(&uuid)))
            .map(|d| (d.kname.clone(), d.path.clone()))
            .collect();
        present.sort();
        let label = g
            .devs
            .values()
            .find(|d| d.uuid.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(&uuid)))
            .and_then(|d| d.label.clone());
        out.push(Unmounted { uuid, label, mountpoint: e.mountpoint.clone(), options: e.options.clone(), present });
    }
    out
}

/// The filesystem entry of an unmounted volume whose devices are not all present, from the
/// superblocks of the members present (`supers`, by device path; `None` while a disk under
/// it sleeps). `None` when all its devices are there (it is merely not mounted).
pub fn unmounted_filesystem(u: &Unmounted, supers: Option<&[(String, Super)]>) -> Option<BtrfsFilesystem> {
    let mut repair = BtrfsRepair { mounted: false, ..Default::default() };
    let mut f = BtrfsFilesystem {
        uuid: u.uuid.clone(),
        label: u.label.clone(),
        mountpoint: None,
        ..Default::default()
    };
    match supers {
        None if u.present.is_empty() => repair.mount = Some(BtrfsBlocker::NoDevices),
        None => repair.mount = Some(BtrfsBlocker::Asleep),
        Some(s) => {
            let total = s.iter().map(|(_, x)| x.num_devices).max().unwrap_or(0);
            if total > 0 && s.len() as u32 >= total {
                return None;
            }
            repair.total_devices = total;
            repair.missing_count = total.saturating_sub(s.len() as u32).max(u32::from(s.is_empty()));
            repair.present = s
                .iter()
                .map(|(path, x)| BtrfsMember { devid: x.devid, path: path.clone(), generation: x.generation })
                .collect();
            repair.present.sort_by_key(|m| m.devid);
            if s.is_empty() {
                repair.mount = Some(BtrfsBlocker::NoDevices);
            }
            f.devices = s
                .iter()
                .map(|(path, x)| rynas_api_types::storage::BtrfsDevice {
                    devid: x.devid,
                    path: path.clone(),
                    size: x.dev_bytes,
                    ..Default::default()
                })
                .collect();
            f.devices.sort_by_key(|d| d.devid);
            f.device_size = f.devices.iter().map(|d| d.size).sum();
        }
    }
    f.repair = Some(repair);
    Some(f)
}

// ------------------------------------------------------------------------------- guard

/// The record of one degraded mount: the devices it ran on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DegradedRecord {
    pub present: Vec<u64>,
    pub generation: u64,
    pub ms: u64,
}

/// The degraded mounts done and not yet repaired, by filesystem UUID, kept in a file
/// (`<state>/storage/btrfs-degraded.json`) so that a reboot remembers them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Guard {
    #[serde(default)]
    pub mounts: BTreeMap<String, DegradedRecord>,
}

impl Guard {
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("storage").join("btrfs-degraded.json")
    }

    /// The guard of `path`; empty when the file is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        let tmp = path.with_extension("tmp");
        let body = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Would mounting `uuid` degraded on `present` risk a split brain? Yes when the last
    /// degraded mount ran on other devices only: the ones present now were missing then and
    /// hold older data (the others may have been written since).
    pub fn split_brain(&self, uuid: &str, present: &[BtrfsMember]) -> bool {
        self.mounts.get(uuid).is_some_and(|r| !r.present.is_empty() && !present.iter().any(|m| r.present.contains(&m.devid)))
    }

    /// Records a degraded mount of `uuid` on `present`. Devices kept from an earlier record
    /// stay: a second degraded mount on a subset of the first one's devices is fine.
    pub fn record(&mut self, uuid: &str, present: &[BtrfsMember], ms: u64) {
        let mut ids: Vec<u64> = present.iter().map(|m| m.devid).collect();
        ids.sort_unstable();
        ids.dedup();
        let generation = present.iter().map(|m| m.generation).max().unwrap_or(0);
        self.mounts.insert(uuid.to_string(), DegradedRecord { present: ids, generation, ms });
    }

    /// Forgets the filesystems seen whole again (`healthy`: mounted, no device missing).
    /// `true` when something was forgotten.
    pub fn forget_healthy(&mut self, healthy: &[String]) -> bool {
        let before = self.mounts.len();
        self.mounts.retain(|u, _| !healthy.iter().any(|h| h.eq_ignore_ascii_case(u)));
        self.mounts.len() != before
    }
}

// ------------------------------------------------------------------- degraded mount after boot

/// A configured volume not mounted because devices are missing, that may be mounted
/// degraded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountCandidate {
    pub uuid: String,
    /// The volume label (`/<label>`).
    pub label: String,
    pub mountpoint: String,
    pub options: Vec<String>,
    pub present: Vec<BtrfsMember>,
    pub total: u32,
}

/// The mount options for a degraded mount: the fstab ones without those mount(8) or systemd
/// handle (`nofail`, `noauto`, `x-…`, `defaults`), plus `degraded` and a `device=` for each
/// member present (registered with the kernel even without udev).
pub fn mount_options(options: &[String], present: &[BtrfsMember]) -> String {
    let mut v: Vec<String> = options
        .iter()
        .filter(|o| {
            !o.is_empty()
                && !o.starts_with("x-")
                && !o.starts_with("device=")
                && !matches!(o.as_str(), "defaults" | "nofail" | "noauto" | "auto" | "_netdev" | "degraded" | "user" | "users" | "owner")
        })
        .cloned()
        .collect();
    v.push("degraded".into());
    for m in present {
        v.push(format!("device={}", m.path));
    }
    v.join(",")
}

/// The command that mounts `c` degraded: `mount -t btrfs -o … <member> <mountpoint>`, in a
/// transient unit when systemd is PID 1 (rynasd's own mount namespace and seccomp filter
/// forbid mounts; the fstab unit itself would wait for the device units).
pub fn mount_argv(c: &MountCandidate, systemd: bool, unit_suffix: &str) -> Vec<String> {
    let dev = c.present.first().map(|m| m.path.clone()).unwrap_or_else(|| format!("UUID={}", c.uuid));
    let mount: Vec<String> = vec![
        "mount".into(),
        "-t".into(),
        "btrfs".into(),
        "-o".into(),
        mount_options(&c.options, &c.present),
        dev,
        c.mountpoint.clone(),
    ];
    if !systemd {
        return mount;
    }
    let mut v: Vec<String> = [
        "systemd-run",
        "--wait",
        "--pipe",
        "--collect",
        "--quiet",
        "--service-type=exec",
        "--property=RuntimeMaxSec=300",
    ]
    .map(String::from)
    .into();
    v.push(format!("--unit=rynas-btrfs-degraded-{unit_suffix}"));
    v.push(format!("--description=rynas: degraded mount of {}", c.mountpoint));
    v.push("--".into());
    v.extend(mount);
    v
}

/// The candidates for a degraded mount among the unmounted volumes of `filesystems`
/// (entries with `repair.mounted == false`), with the blocked ones and why: the setting off
/// for the UUID (`manual`), no device present, a superblock not read, or a split brain.
pub fn candidates(filesystems: &[BtrfsFilesystem], manual: &[String], guard: &Guard, mountpoints: &HashMap<String, (String, Vec<String>)>) -> (Vec<MountCandidate>, Vec<(MountCandidate, BtrfsBlocker)>) {
    let mut ok = Vec::new();
    let mut blocked = Vec::new();
    for f in filesystems {
        let Some(r) = f.repair.as_ref().filter(|r| !r.mounted) else { continue };
        let Some((mp, options)) = mountpoints.get(&f.uuid) else { continue };
        let Some(label) = volume_mountpoint(mp) else { continue };
        let c = MountCandidate {
            uuid: f.uuid.clone(),
            label: label.to_string(),
            mountpoint: mp.clone(),
            options: options.clone(),
            present: r.present.clone(),
            total: r.total_devices,
        };
        let why = if let Some(b) = r.mount {
            Some(b)
        } else if manual.iter().any(|m| m.eq_ignore_ascii_case(&f.uuid)) {
            Some(BtrfsBlocker::Disabled)
        } else if guard.split_brain(&f.uuid, &r.present) {
            Some(BtrfsBlocker::SplitBrain)
        } else {
            None
        };
        match why {
            Some(b) => blocked.push((c, b)),
            None => ok.push(c),
        }
    }
    (ok, blocked)
}

/// Remembers since when each unmounted volume has been waiting with the same devices: the
/// same rules as the md arrays left inactive ([`crate::recover`]): the machine up for
/// [`crate::recover::MIN_UPTIME_S`], the devices unchanged for [`crate::recover::STABLE_MS`],
/// first seen within [`crate::recover::MAX_UPTIME_S`] of the boot.
#[derive(Debug, Default)]
pub struct Waiting {
    seen: HashMap<String, (u64, Vec<u64>)>,
}

impl Waiting {
    fn devids(c: &MountCandidate) -> Vec<u64> {
        let mut v: Vec<u64> = c.present.iter().map(|m| m.devid).collect();
        v.sort_unstable();
        v
    }

    /// The candidates of `found` that are due at `now_ms` with the machine up `uptime_s`.
    pub fn due(&mut self, found: Vec<MountCandidate>, now_ms: u64, uptime_s: u64) -> Vec<MountCandidate> {
        use crate::recover::{MAX_UPTIME_S, MIN_UPTIME_S, STABLE_MS};
        self.seen.retain(|k, _| found.iter().any(|c| &c.uuid == k));
        for c in &found {
            let ids = Self::devids(c);
            match self.seen.get(&c.uuid) {
                Some((_, m)) if *m == ids => {}
                _ if uptime_s > MAX_UPTIME_S => {
                    self.seen.remove(&c.uuid);
                }
                _ => {
                    self.seen.insert(c.uuid.clone(), (now_ms, ids));
                }
            }
        }
        if uptime_s < MIN_UPTIME_S {
            return Vec::new();
        }
        found
            .into_iter()
            .filter(|c| self.seen.get(&c.uuid).is_some_and(|(t, _)| now_ms.saturating_sub(*t) >= STABLE_MS))
            .collect()
    }

    /// The boot window is over and nothing seen inside it waits.
    pub fn finished(&self, uptime_s: u64) -> bool {
        uptime_s > crate::recover::MAX_UPTIME_S && self.seen.is_empty()
    }

    pub fn forget(&mut self, uuid: &str) {
        self.seen.remove(uuid);
    }
}

// ------------------------------------------------------------------------------- plans

fn step(kind: PlanStepKind, args: PlanArgs, argv: Vec<String>) -> PlanStep {
    PlanStep {
        description: crate::plan::step_english(kind, &args),
        kind: Some(kind),
        args,
        action: PlanAction::Command { argv, stdin: None },
        destroys: Vec::new(),
        best_effort: false,
    }
}

fn plan(operation: &str, errors: Vec<String>, steps: Vec<PlanStep>) -> StoragePlan {
    StoragePlan {
        operation: operation.into(),
        valid: errors.is_empty(),
        steps: if errors.is_empty() { steps } else { Vec::new() },
        errors,
        ..Default::default()
    }
}

/// The convert step of a repair, when chunks need it.
fn convert_step(mnt: &str, c: &[BtrfsConvert]) -> PlanStep {
    let items: Vec<String> = c.iter().map(|x| format!("{} {}", x.kind, x.to)).collect();
    step(
            PlanStepKind::BtrfsConvert,
        PlanArgs { subject: Some(mnt.into()), items, ..Default::default() },
        convert_argv(mnt, c),
    )
}

/// What can be repaired on `fs`: its mount point and repair facts, or the reason it cannot.
fn mounted_repair(fs: &BtrfsFilesystem) -> Result<(String, &BtrfsRepair), String> {
    let r = fs.repair.as_ref().ok_or("nothing is missing on this filesystem")?;
    let mnt = fs.mountpoint.clone().filter(|_| r.mounted).ok_or("the filesystem is not mounted: mount it degraded first")?;
    Ok((mnt, r))
}

/// Replace missing device `devid` of `fs` with the whole disk `new_disk` (`size` bytes):
/// clear its signatures, `btrfs replace start`, wait, grow the device to the disk, convert
/// what a degraded mount wrote back.
pub fn plan_replace(fs: &BtrfsFilesystem, devid: u64, new_disk: &str, size: u64) -> StoragePlan {
    let op = "btrfs_replace";
    let (mnt, r) = match mounted_repair(fs) {
        Ok(x) => x,
        Err(e) => return plan(op, vec![e], Vec::new()),
    };
    let mut errors = Vec::new();
    if !r.missing.contains(&devid) {
        errors.push(format!("device {devid} is not missing from this filesystem"));
    }
    if !r.lost.is_empty() {
        errors.push(format!("chunks without a complete copy ({}): a rebuild cannot read them", r.lost.join(", ")));
    }
    if size < r.missing_bytes {
        errors.push(format!("disk {new_disk} ({size} bytes) is smaller than the missing device ({} bytes)", r.missing_bytes));
    }
    let dev = format!("/dev/{new_disk}");
    let id = devid.to_string();
    let mut wipe = step(
            PlanStepKind::WipeSignatures,
        PlanArgs { subject: Some(new_disk.into()), ..Default::default() },
        ["wipefs", "-a", &dev].map(String::from).into(),
    );
    wipe.destroys = vec![new_disk.to_string()];
    let mut steps = vec![
        wipe,
        step(
            PlanStepKind::BtrfsReplace,
            PlanArgs { subject: Some(new_disk.into()), other: Some(id.clone()), ..Default::default() },
            ["btrfs", "replace", "start", &id, &dev, &mnt].map(String::from).into(),
        ),
        PlanStep {
            description: crate::plan::step_english(PlanStepKind::BtrfsWaitReplace, &PlanArgs { subject: Some(mnt.clone()), ..Default::default() }),
            kind: Some(PlanStepKind::BtrfsWaitReplace),
            args: PlanArgs { subject: Some(mnt.clone()), ..Default::default() },
            action: PlanAction::Wait { condition: format!("btrfs replace {mnt}") },
            destroys: Vec::new(),
            best_effort: false,
        },
        step(
            PlanStepKind::BtrfsResize,
            PlanArgs { subject: Some(mnt.clone()), other: Some(id.clone()), ..Default::default() },
            ["btrfs", "filesystem", "resize", &format!("{devid}:max"), &mnt].map(String::from).into(),
        ),
    ];
    if !r.convert.is_empty() || r.degraded_mount {
        steps.push(convert_step(&mnt, &r.convert));
    }
    let mut p = plan(op, errors, steps);
    p.summary = vec![("mountpoint".into(), mnt), ("devid".into(), devid.to_string()), ("disk".into(), new_disk.into())];
    p
}

/// Remove the missing devices of `fs` (the others take their chunks), then convert.
pub fn plan_remove_missing(fs: &BtrfsFilesystem) -> StoragePlan {
    let op = "btrfs_remove_missing";
    let (mnt, r) = match mounted_repair(fs) {
        Ok(x) => x,
        Err(e) => return plan(op, vec![e], Vec::new()),
    };
    let mut errors = Vec::new();
    if r.missing_count == 0 {
        errors.push("no device is missing".into());
    }
    match r.remove {
        Some(BtrfsBlocker::Incomplete) => errors.push(format!("chunks without a complete copy ({})", r.lost.join(", "))),
        Some(BtrfsBlocker::TooFewDevices) => errors.push("the devices left are fewer than the profiles need: replace the missing device instead".into()),
        Some(BtrfsBlocker::NoSpace) => errors.push("the devices left have not enough free space for the missing device's chunks".into()),
        Some(_) | None => {}
    }
    let mut argv: Vec<String> = ["btrfs", "device", "remove"].map(String::from).into();
    for _ in 0..r.missing_count.max(1) {
        argv.push("missing".into());
    }
    argv.push(mnt.clone());
    let mut steps = vec![step(
            PlanStepKind::BtrfsRemoveMissing,
        PlanArgs { subject: Some(mnt.clone()), count: Some(u64::from(r.missing_count)), ..Default::default() },
        argv,
    )];
    if !r.convert.is_empty() || r.degraded_mount {
        steps.push(convert_step(&mnt, &r.convert));
    }
    let mut p = plan(op, errors, steps);
    p.summary = vec![("mountpoint".into(), mnt)];
    p
}

/// Convert the weaker chunks of `fs` back to their kind's profile.
pub fn plan_convert(fs: &BtrfsFilesystem) -> StoragePlan {
    let op = "btrfs_convert";
    let (mnt, r) = match mounted_repair(fs) {
        Ok(x) => x,
        Err(e) => return plan(op, vec![e], Vec::new()),
    };
    let mut errors = Vec::new();
    if r.convert.is_empty() {
        errors.push("no chunk has a weaker profile than its kind".into());
    }
    if r.missing_count > 0 {
        errors.push("a device is missing: replace or remove it first (chunks would be written weak again)".into());
    }
    let mut p = plan(op, errors, vec![convert_step(&mnt, &r.convert)]);
    p.summary = vec![("mountpoint".into(), mnt)];
    p
}

/// Mount `c` degraded.
pub fn plan_mount(c: &MountCandidate, systemd: bool, unit_suffix: &str) -> StoragePlan {
    let mut errors = Vec::new();
    if c.present.is_empty() {
        errors.push("no device of the filesystem is present".into());
    }
    let mut p = plan(
        "btrfs_mount_degraded",
        errors,
        vec![step(
            PlanStepKind::BtrfsMountDegraded,
            PlanArgs { subject: Some(c.mountpoint.clone()), count: Some(c.present.len() as u64), ..Default::default() },
            mount_argv(c, systemd, unit_suffix),
        )],
    );
    p.summary = vec![("mountpoint".into(), c.mountpoint.clone())];
    p
}

/// Mounts `c` degraded now, outside a task (the mount after boot): the command of
/// [`mount_argv`] through `host`, its last error line on failure.
pub fn mount_now(host: &dyn crate::host::Host, c: &MountCandidate, systemd: bool, unit_suffix: &str) -> Result<(), String> {
    let argv = mount_argv(c, systemd, unit_suffix);
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let o = host.run(&argv[0], &args, Duration::from_secs(330)).map_err(|e| format!("{}: {e}", argv[0]))?;
    if o.success() {
        return Ok(());
    }
    let why = o.stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
    Err(format!("mount failed (exit code {:?}): {why}", o.status))
}

// ------------------------------------------------------------------------------- execution

/// Progress of `btrfs replace status -1`: `Some(pct)` while running, `Ok` when finished.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplaceState {
    Running(f32),
    Finished,
    Failed(String),
}

/// `btrfs replace status -1 <mnt>`: `12.3% done, 0 write errs, 0 uncorr. read errs`,
/// `Started on …, finished on …, 0 write errs, …`, `Started on …, canceled on …`,
/// `Never started`.
pub fn parse_replace_status(s: &str) -> ReplaceState {
    let t = s.trim();
    if let Some(p) = t.split("% done").next().filter(|_| t.contains("% done")) {
        return ReplaceState::Running(p.trim().parse().unwrap_or(0.0));
    }
    if t.contains("finished on") {
        return ReplaceState::Finished;
    }
    ReplaceState::Failed(if t.is_empty() { "no status".into() } else { t.lines().last().unwrap_or(t).to_string() })
}

/// How long a step may run.
fn timeout_of(argv: &[String]) -> Duration {
    let long = Duration::from_secs(30 * 24 * 3600);
    match (argv.get(1).map(String::as_str), argv.get(2).map(String::as_str)) {
        (Some("balance"), _) | (Some("device"), Some("remove")) => long,
        _ => Duration::from_secs(10 * 60),
    }
}

async fn run(m: &impl Machine, argv: Vec<String>, cancellable: bool) -> Result<String, String> {
    let mut o = RunOpts::new(timeout_of(&argv));
    o.cancellable = cancellable;
    m.run(argv, o).await.map(|o| o.stdout)
}

/// Stops what a cancelled long step left running in the kernel.
async fn stop_kernel_work(m: &impl Machine, argv: &[String], mnt: &str) {
    let stop: Option<Vec<String>> = match argv.get(1).map(String::as_str) {
        Some("balance") => Some(["btrfs", "balance", "cancel", mnt].map(String::from).into()),
        Some("replace") => Some(["btrfs", "replace", "cancel", mnt].map(String::from).into()),
        _ => None,
    };
    if let Some(a) = stop {
        let _ = run(m, a, false).await;
    }
}

/// Runs a plan of this module step by step: commands in order; the rebuild wait follows
/// `btrfs replace status`; the convert step reads the device usage again first and converts
/// what is weak then (nothing: skipped). A cancel stops a running replace or balance.
pub async fn run_plan<M: Machine>(m: &M, p: &StoragePlan) -> Result<(), String> {
    let total = p.steps.len().max(1);
    let mnt = p.summary.iter().find(|(k, _)| k == "mountpoint").map(|(_, v)| v.clone()).unwrap_or_default();
    for (i, st) in p.steps.iter().enumerate() {
        if m.is_cancelled() {
            return Err("cancelled".into());
        }
        m.progress((i * 100 / total) as u8);
        m.log(&format!("[{}/{total}] {}", i + 1, st.description));
        match &st.action {
            PlanAction::Wait { condition } if condition.starts_with("btrfs replace ") => {
                let base = i * 100 / total;
                loop {
                    let s = run(m, ["btrfs", "replace", "status", "-1", &mnt].map(String::from).into(), false).await?;
                    match parse_replace_status(&s) {
                        ReplaceState::Finished => break,
                        ReplaceState::Running(pct) => {
                            m.progress((base as f32 + pct / total as f32).min(99.0) as u8);
                        }
                        ReplaceState::Failed(e) => return Err(format!("the rebuild stopped: {e}")),
                    }
                    if m.is_cancelled() {
                        stop_kernel_work(m, &["btrfs".into(), "replace".into()], &mnt).await;
                        return Err("cancelled: the rebuild was stopped".into());
                    }
                    m.sleep(Duration::from_secs(5)).await;
                }
            }
            PlanAction::Wait { condition } => return Err(format!("cannot wait for {condition:?}")),
            PlanAction::Command { argv, .. } => {
                let mut argv = argv.clone();
                if st.kind == Some(PlanStepKind::BtrfsConvert) {
                    let u = run(m, ["btrfs", "device", "usage", "-b", &mnt].map(String::from).into(), false).await?;
                    let c = converts(&parse_device_usage(&u));
                    if c.is_empty() {
                        m.log("no chunk has a weaker profile: nothing to convert");
                        continue;
                    }
                    argv = convert_argv(&mnt, &c);
                }
                if let Some(d) = st.destroys.first() {
                    m.claim_free(&format!("/dev/{d}"))?;
                }
                if let Err(e) = run(m, argv.clone(), true).await {
                    if m.is_cancelled() {
                        stop_kernel_work(m, &argv, &mnt).await;
                    }
                    return Err(e);
                }
            }
            PlanAction::WriteSysfs { .. } | PlanAction::Config { .. } => {
                return Err(format!("step {:?} is not a btrfs repair step", st.description));
            }
        }
    }
    m.progress(100);
    Ok(())
}

#[cfg(test)]
#[path = "btrfs_repair_tests.rs"]
mod tests;
