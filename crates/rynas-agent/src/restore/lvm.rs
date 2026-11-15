//! LVM in the bare-metal restore: simple layouts made again as the PC had them.
//!
//! - Physical volumes on partitions or on LUKS containers of the restored disks; volume
//!   groups with the same names (the boot command line and fstab may name `/dev/<vg>/<lv>` or
//!   `/dev/mapper/<vg>-<lv>`) and extent size; linear logical volumes with the same names and
//!   sizes, in the PC's order. Thin pools, snapshots, raid/mirror/striped and cache volumes
//!   are refused ([`LvmLayout::unsupported`]); so is a group with a PV on a disk that is not
//!   restored, a PV on a whole disk or on md, and a group whose name another attached disk
//!   uses. The rules are [`Manifest::lvm_vg_check`], which the NAS's readiness uses too; a
//!   group that cannot be made but holds nothing of the backup (a thin pool of virtual
//!   machines) is left out with a note, its partitions made again empty.
//! - Every LVM command of the restore sees only the group's PVs (no devices file, a filter on
//!   them: [`scope`]) and makes no system ID; the PC's system ID is set at the end
//!   ([`finish`]). The restored system's devices file and lvm.conf filters follow the new
//!   disk (in `exec`).
//! - PV, VG and LV UUIDs are the PC's unless an attached device has them (the rule of the GPT
//!   and filesystem IDs). `pvcreate --uuid` sets the PV's; `vgcreate`/`lvcreate` cannot set
//!   theirs, so the group is made by LVM, written out with `vgcfgbackup`, given the PC's UUIDs
//!   and written back with `vgcfgrestore` ([`swap_ids`]). A metadata backup of the PC is not
//!   restored as it is: it could not follow a smaller disk.
//! - Sizes: a target that gives fewer extents shrinks only the volumes whose filesystem holds
//!   less than their size, in proportion to their size and never below their data plus room
//!   (the plan's rule: used + 10 %, at least 256 MiB); the group's free space (what the
//!   agent's LVM snapshots use) is kept while it can be, and goes before the restore refuses.

use rynas_api_types::pcagent::*;

use super::exec::{Log, has, run};

/// LVM's default extent size, for a manifest that did not record it.
pub const DEFAULT_EXTENT: u64 = 4 << 20;
/// Where LVM puts the first extent of a new PV by default.
const PE_START: u64 = 1 << 20;
/// The smallest swap volume a smaller target shrinks a swap volume to (`mkswap` makes it
/// again with its UUID: nothing of it is in the backup).
pub const SWAP_MIN: u64 = 256 << 20;
/// The smallest XFS filesystem newer xfsprogs make.
pub const XFS_MIN: u64 = 300 << 20;

/// What a LUKS header takes before the data, by LUKS version: 16 MiB for LUKS2 (cryptsetup's
/// default), 2 MiB for LUKS1.
pub fn luks_header(version: u32) -> u64 {
    if version == 1 { 2 << 20 } else { 16 << 20 }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PvAction {
    /// `/dev/…` it is made on: a partition, or `/dev/mapper/<name>` of a LUKS container.
    pub device: String,
    /// The PC's UUID (`None`: LVM makes a new one).
    pub uuid: Option<String>,
    /// The UUID the PV had on the PC (empty: not recorded), for the devices file.
    pub orig_uuid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LvAction {
    pub name: String,
    pub uuid: Option<String>,
    /// Extents on the PC.
    pub orig_extents: u64,
    /// The fewest extents it may get: its data plus room; the PC's size when it is not shrunk
    /// (no filesystem of the backup on it).
    pub min_extents: u64,
    /// Extents on the target.
    pub extents: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VgAction {
    pub name: String,
    pub uuid: Option<String>,
    pub extent_bytes: u64,
    pub pvs: Vec<PvAction>,
    pub lvs: Vec<LvAction>,
    /// Free extents on the PC (kept when there is room).
    pub orig_free: u64,
    /// Extents the target gives (from the partition sizes; the real run reads LVM's count).
    pub available: u64,
    /// The PC's LVM system ID of the group, set at the end (the group is made without one,
    /// whatever the live system's `system_id_source`); `None`: the group stays without.
    pub system_id: Option<String>,
}

impl VgAction {
    pub fn free(&self) -> u64 {
        self.available.saturating_sub(self.lvs.iter().map(|l| l.extents).sum())
    }
    /// The group or a volume keeps the PC's UUID (the `vgcfgrestore` step).
    pub fn sets_ids(&self) -> bool {
        self.uuid.is_some() || self.lvs.iter().any(|l| l.uuid.is_some())
    }
    fn sizes(&self) -> Vec<(u64, u64)> {
        self.lvs.iter().map(|l| (l.orig_extents, l.min_extents)).collect()
    }
}

/// Where a PV of the manifest goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PvTarget {
    /// Onto `device` of a restored disk, which has `bytes` for LVM (`orig_bytes` on the PC).
    On { device: String, bytes: u64, orig_bytes: u64 },
    /// Not onto a restored disk (or not something this version makes: [`Manifest::lvm_vg_check`]
    /// says why).
    NoTarget,
}

/// An LVM UUID as LVM writes it (`xxxxxx-xxxx-xxxx-xxxx-xxxx-xxxx-xxxxxx`, letters and digits).
pub fn valid_uuid(u: &str) -> bool {
    let parts: Vec<&str> = u.split('-').collect();
    parts.iter().map(|p| p.len()).eq([6, 4, 4, 4, 4, 4, 6]) && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn ceil_div(a: u64, b: u64) -> u64 {
    a.div_ceil(b.max(1))
}

/// The extents of the volumes (`(PC's size, fewest)` each) in `available` extents, with
/// `orig_free` left free when there is room: the PC's sizes when they fit; otherwise the
/// volumes that may shrink get the same share of their size (never below their fewest),
/// first keeping the free space, then giving it up. `None`: even the fewest do not fit.
pub fn allocate(lvs: &[(u64, u64)], orig_free: u64, available: u64) -> Option<Vec<u64>> {
    let total: u64 = lvs.iter().map(|l| l.0).sum();
    let floor: u64 = lvs.iter().map(|l| l.1.min(l.0)).sum();
    if total + orig_free <= available || (total <= available && floor == total) {
        return Some(lvs.iter().map(|l| l.0).collect());
    }
    let budget = if floor + orig_free <= available { available - orig_free } else { available };
    if floor > budget {
        return None;
    }
    if total <= budget {
        return Some(lvs.iter().map(|l| l.0).collect());
    }
    // water filling: volumes whose share would go below their fewest are held there
    let mut held = vec![false; lvs.len()];
    loop {
        let held_sum: u64 = lvs.iter().zip(&held).filter(|(_, h)| **h).map(|(l, _)| l.1).sum();
        let free_sizes: u64 = lvs.iter().zip(&held).filter(|(_, h)| !**h).map(|(l, _)| l.0).sum();
        let rest = budget.saturating_sub(held_sum);
        let share = |size: u64| (u128::from(size) * u128::from(rest) / u128::from(free_sizes.max(1))) as u64;
        let mut changed = false;
        for (i, l) in lvs.iter().enumerate() {
            if !held[i] && share(l.0) < l.1 {
                held[i] = true;
                changed = true;
            }
        }
        if !changed {
            return Some(lvs.iter().zip(&held).map(|(l, h)| if *h { l.1 } else { share(l.0).min(l.0) }).collect());
        }
    }
}

fn needed_on(m: &Manifest, vg: &str, lv: &str) -> Option<u64> {
    let on = |s: &FsSource| matches!(s, FsSource::Lvm { vg: v, lv: l } if v == vg && l == lv);
    // swap holds nothing of the backup: mkswap makes it again with its UUID
    if m.swap.iter().any(|s| !s.file && on(&s.source)) {
        return Some(SWAP_MIN);
    }
    let luks: Vec<usize> = m.luks.iter().enumerate().filter(|(_, l)| on(&l.source)).map(|(i, _)| i).collect();
    let f = m.filesystems.iter().find(|f| on(&f.source) || matches!(f.source, FsSource::Luks { luks: i } if luks.contains(&i)))?;
    if f.capture == Some(CaptureMethod::Skipped) || f.used_bytes == 0 || !["ext2", "ext3", "ext4", "xfs", "btrfs", "vfat", "f2fs"].contains(&f.fstype.as_str()) {
        return None;
    }
    let extra = luks.first().and_then(|i| m.luks.get(*i)).map(|l| luks_header(l.version)).unwrap_or(0);
    Some(super::plan::needed_fs(&f.fstype, f.used_bytes) + extra)
}

pub fn plan(m: &Manifest, layout: &LvmLayout, pv_target: &dyn Fn(&str) -> PvTarget, restored: &dyn Fn(usize) -> bool, in_use: &dyn Fn(&str) -> bool, notes: &mut Vec<String>) -> Result<Vec<VgAction>, String> {
    let mut out = Vec::new();
    for vg in &layout.vgs {
        match m.lvm_vg_check(vg, restored) {
            LvmVgCheck::Elsewhere => {
                if m.lvm_vg_used(&vg.name) {
                    notes.push(format!("volume group {}: not on a restored disk, left out", vg.name));
                }
                continue;
            }
            LvmVgCheck::Refuse(why) => {
                return Err(format!(
                    "volume group {}: this version of the restore recreates linear LVM volumes only, on partitions of the restored disk; not: {}; recreate the volume group by hand, then restore the files from the web interface",
                    vg.name,
                    why.join("; ")
                ));
            }
            LvmVgCheck::SkipUnused(why) => {
                notes.push(format!(
                    "volume group {} holds nothing of the backup and is not made again ({}): its partitions are made again, empty; recreate the group by hand if it is needed",
                    vg.name,
                    why.join("; ")
                ));
                continue;
            }
            LvmVgCheck::Recreate => {}
        }
        let places: Vec<(&LvmPv, PvTarget)> = vg.pvs.iter().map(|p| (p, pv_target(&p.device))).collect();
        if let Some((p, _)) = places.iter().find(|(_, t)| *t == PvTarget::NoTarget) {
            return Err(format!("volume group {}: no place on the target for its physical volume {}", vg.name, p.device));
        }
        if in_use(&format!("vg:{}", vg.name)) {
            return Err(format!(
                "a volume group named {} is on another attached disk: the restore needs that name (the boot command line and fstab can name /dev/{}/…); detach that disk, or rename its group (vgrename) first",
                vg.name, vg.name
            ));
        }
        let keep = |what: &str, u: &str, notes: &mut Vec<String>| -> Option<String> {
            if u.is_empty() {
                return None;
            }
            if !valid_uuid(u) {
                notes.push(format!("{what}: the UUID {u} in the manifest is not an LVM UUID, a new one is made"));
                return None;
            }
            if in_use(u) {
                notes.push(format!("{what}: UUID {u} is in use on another disk, a new one is made"));
                return None;
            }
            Some(u.to_string())
        };
        let extent = if vg.extent_bytes > 0 { vg.extent_bytes } else { DEFAULT_EXTENT };
        if vg.extent_bytes == 0 {
            notes.push(format!("volume group {}: the manifest does not record the extent size (older agent): LVM's default of 4 MiB", vg.name));
        }
        if extent < 1024 || !extent.is_power_of_two() {
            return Err(format!("volume group {}: extent size {extent} bytes is not one LVM makes", vg.name));
        }
        if vg.uuid.is_empty() && vg.lvs.iter().all(|l| l.uuid.is_empty()) {
            notes.push(format!(
                "volume group {}: the manifest has no LVM UUIDs (older agent): LVM makes new ones; the names stay, and fstab and the command line name filesystems and volume paths",
                vg.name
            ));
        }
        let mut pvs = Vec::new();
        let (mut available, mut orig_available) = (0u64, 0u64);
        for (p, t) in &places {
            let PvTarget::On { device, bytes, orig_bytes } = t else { continue };
            let start = if p.pe_start_bytes > 0 { p.pe_start_bytes } else { PE_START };
            available += bytes.saturating_sub(start) / extent;
            orig_available += if p.size_bytes > 0 { p.size_bytes.saturating_sub(start) } else { orig_bytes.saturating_sub(start) } / extent;
            pvs.push(PvAction { device: device.clone(), uuid: keep(&format!("physical volume {}", p.device), &p.uuid, notes), orig_uuid: p.uuid.clone() });
        }
        let mut lvs = Vec::new();
        for l in &vg.lvs {
            let orig = ceil_div(l.size_bytes, extent);
            if orig == 0 {
                return Err(format!("{}/{}: the manifest has no size for it", vg.name, l.name));
            }
            let min = needed_on(m, &vg.name, &l.name).map(|b| ceil_div(b, extent).min(orig)).unwrap_or(orig);
            lvs.push(LvAction { name: l.name.clone(), uuid: keep(&format!("{}/{}", vg.name, l.name), &l.uuid, notes), orig_extents: orig, min_extents: min, extents: orig });
        }
        let lv_sum: u64 = lvs.iter().map(|l| l.orig_extents).sum();
        let orig_free = if vg.size_bytes > 0 { vg.free_bytes / extent } else { orig_available.saturating_sub(lv_sum) };
        let system_id = Some(vg.system_id.clone()).filter(|s| !s.is_empty());
        if let Some(s) = &system_id {
            notes.push(format!("volume group {}: made without an LVM system ID, the PC's ({s}) set at the end", vg.name));
        }
        let mut a = VgAction { name: vg.name.clone(), uuid: keep(&format!("volume group {}", vg.name), &vg.uuid, notes), extent_bytes: extent, pvs, lvs, orig_free, available, system_id };
        let sizes = allocate(&a.sizes(), orig_free, available).ok_or_else(|| {
            let need: u64 = a.lvs.iter().map(|l| l.min_extents).sum();
            format!(
                "too small for volume group {}: its volumes need at least {} MiB (their data plus room), the target gives {} MiB",
                vg.name,
                (need * extent) >> 20,
                (available * extent) >> 20
            )
        })?;
        for (l, s) in a.lvs.iter_mut().zip(sizes) {
            if s < l.orig_extents {
                notes.push(format!(
                    "{}/{}: {} MiB → {} MiB (the target is smaller; the data needs about {} MiB)",
                    vg.name,
                    l.name,
                    (l.orig_extents * extent) >> 20,
                    (s * extent) >> 20,
                    (l.min_extents * extent) >> 20
                ));
            }
            l.extents = s;
        }
        if a.free() < orig_free && a.lvs.iter().all(|l| l.extents == l.orig_extents) {
            notes.push(format!("volume group {}: {} MiB free instead of {} MiB (the target is smaller)", vg.name, (a.free() * extent) >> 20, (orig_free * extent) >> 20));
        } else if a.free() < orig_free {
            notes.push(format!("volume group {}: {} MiB free instead of {} MiB", vg.name, (a.free() * extent) >> 20, (orig_free * extent) >> 20));
        }
        out.push(a);
    }
    Ok(out)
}

/// The group as plan lines.
pub fn describe(a: &VgAction) -> Vec<String> {
    let mib = |e: u64| (e * a.extent_bytes) >> 20;
    let id = |u: &Option<String>| u.as_ref().map(|u| format!(" UUID {u}")).unwrap_or_else(|| " (new UUID)".into());
    let mut out = vec![format!("# LVM volume group {}{}, extent {} KiB, {} MiB", a.name, id(&a.uuid), a.extent_bytes >> 10, mib(a.available))];
    for p in &a.pvs {
        match &p.uuid {
            Some(u) => out.push(format!("pvcreate --uuid {u} --norestorefile {}", p.device)),
            None => out.push(format!("pvcreate {}", p.device)),
        }
    }
    out.push(format!("vgcreate -s {}k {} {}", a.extent_bytes >> 10, a.name, a.pvs.iter().map(|p| p.device.as_str()).collect::<Vec<_>>().join(" ")));
    for l in &a.lvs {
        let shrunk = if l.extents < l.orig_extents { format!(", shrunk from {} MiB", mib(l.orig_extents)) } else { String::new() };
        out.push(format!("  lvcreate -l {} -n {} {}   (/dev/{}/{}, {} MiB{shrunk}{})", l.extents, l.name, a.name, a.name, l.name, mib(l.extents), id(&l.uuid)));
    }
    out.push(format!("  free: {} MiB (the PC had {} MiB)", mib(a.free()), mib(a.orig_free)));
    if a.sets_ids() {
        out.push(format!("  the PC's VG/LV UUIDs written with vgcfgbackup and vgcfgrestore, then vgchange -ay {}", a.name));
    }
    out.push("  (these LVM commands see only these physical volumes: no devices file, a filter on them, no system ID)".into());
    if let Some(s) = &a.system_id {
        out.push(format!("  at the end: vgchange -an {}; vgchange --systemid {s} {} (the PC's system ID)", a.name, a.name));
    }
    out
}

/// A device path as an exact LVM filter pattern (`a|^/dev/sda2$|`): characters other than
/// letters, digits, `/`, `_` and `-` go in brackets (no backslashes in LVM's config strings).
fn filter_pattern(path: &str) -> String {
    let mut p = String::from("a|^");
    for c in path.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-') {
            p.push(c);
        } else {
            p.push('[');
            p.push(c);
            p.push(']');
        }
    }
    p.push_str("$|");
    p
}

/// Options that make an LVM command of the restore independent of the live system's LVM
/// configuration: no devices file (`--devicesfile ""`, when this LVM has devices files:
/// `devicesfile`), a filter and global filter that accept `devices` only (`None`: every
/// device) and, for the commands that make groups (`no_system_id`), no system ID, so a
/// group is not made foreign to the restored system by the live system's `system_id_source`.
pub fn scope(devices: Option<&[&str]>, devicesfile: bool, no_system_id: bool) -> Vec<String> {
    let mut v = Vec::new();
    if devicesfile {
        v.extend(["--devicesfile".to_string(), String::new()]);
    }
    let list = match devices {
        Some(d) => {
            let mut l: Vec<String> = d.iter().map(|p| format!("\"{}\"", filter_pattern(p))).collect();
            l.push("\"r|.*|\"".into());
            l.join(",")
        }
        None => "\"a|.*|\"".into(),
    };
    let mut c = format!("devices{{filter=[{list}] global_filter=[{list}]}}");
    if no_system_id {
        c.push_str(" global{system_id_source=\"none\"}");
    }
    v.extend(["--config".to_string(), c]);
    v
}

pub fn has_devicesfile() -> bool {
    static KNOWS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *KNOWS.get_or_init(|| {
        let out = std::process::Command::new("lvm").arg("version").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        lvm_version_has_devicesfile(&out)
    })
}

/// `lvm version` names an LVM with devices files (2.03.12 and later; `lvmdevices` is not a
/// program of its own everywhere).
fn lvm_version_has_devicesfile(out: &str) -> bool {
    let Some(v) = out.lines().find_map(|l| l.trim().strip_prefix("LVM version:")) else { return false };
    let nums: Vec<u32> = v.trim().split(|c: char| !c.is_ascii_digit()).take(3).map(|n| n.parse().unwrap_or(0)).collect();
    nums.len() == 3 && (nums[0], nums[1], nums[2]) >= (2, 3, 12)
}

/// Runs an LVM command of the restore on the group's PVs only ([`scope`]).
fn run_lvm(log: &mut Log, a: &VgAction, argv: &[&str]) -> Result<String, String> {
    let devs: Vec<&str> = a.pvs.iter().map(|p| p.device.as_str()).collect();
    let extra = scope(Some(&devs), has_devicesfile(), true);
    let mut v: Vec<&str> = vec![argv[0]];
    v.extend(extra.iter().map(String::as_str));
    v.extend_from_slice(&argv[1..]);
    run(log, &v)
}

/// A private folder for the group's metadata file (mode 0700, a name nobody could have
/// made before: `mkdir` fails on anything that exists, a symlink too).
fn private_dir() -> Result<std::path::PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    let base = std::env::temp_dir();
    let mut last = String::new();
    for _ in 0..8 {
        let g = crate::gpt::random_guid();
        let d = base.join(format!("rynas-restore-{}", g.iter().map(|b| format!("{b:02x}")).collect::<String>()));
        match std::fs::DirBuilder::new().mode(0o700).create(&d) {
            Ok(()) => return Ok(d),
            Err(e) => last = format!("{}: {e}", d.display()),
        }
    }
    Err(last)
}

/// Writes a new file (never an existing one, never through a symlink), mode 0600.
fn write_new(path: &std::path::Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

/// The tools the LVM step runs on the live system.
pub fn tools(vgs: &[VgAction]) -> Vec<&'static str> {
    if vgs.is_empty() {
        return Vec::new();
    }
    let mut v = vec!["pvcreate", "vgcreate", "lvcreate", "vgchange", "vgs"];
    if vgs.iter().any(VgAction::sets_ids) {
        v.extend(["vgcfgbackup", "vgcfgrestore"]);
    }
    v
}

/// LVM metadata text (`vgcfgbackup`) with the group's and the volumes' `id` set to the PC's
/// UUIDs: the first `id` in the group's section, and the first in each volume's section of
/// `logical_volumes`. PV ids stay (`pvcreate --uuid` set them).
pub fn swap_ids(text: &str, a: &VgAction) -> String {
    let mut stack: Vec<String> = Vec::new();
    let mut done: Vec<Vec<String>> = Vec::new();
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        let code = t.split('#').next().unwrap_or("").trim();
        if let Some(name) = code.strip_suffix('{') {
            stack.push(name.trim().to_string());
        } else if code == "}" {
            stack.pop();
        } else if let Some(rest) = code.strip_prefix("id") {
            let want = match stack.as_slice() {
                [vg] if *vg == a.name => a.uuid.as_deref(),
                [vg, lvs, lv] if *vg == a.name && lvs == "logical_volumes" => a.lvs.iter().find(|l| l.name == *lv).and_then(|l| l.uuid.as_deref()),
                _ => None,
            };
            if let Some(u) = want
                && rest.trim_start().starts_with('=')
                && !done.contains(&stack)
                && let (Some(i), Some(j)) = (line.find('"'), line.rfind('"'))
                && j > i
            {
                done.push(stack.clone());
                out.push_str(&line[..=i]);
                out.push_str(u);
                out.push_str(&line[j..]);
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

fn extent_count(log: &mut Log, a: &VgAction) -> Result<u64, String> {
    let out = run_lvm(log, a, &["vgs", "--noheadings", "-o", "vg_extent_count", &a.name])?;
    out.trim().parse().map_err(|_| format!("vgs {}: unexpected answer {:?}", a.name, out.trim()))
}

/// Makes the group: PVs (with their UUIDs), the group, the volumes in the PC's order (sized
/// again for LVM's real count of extents), the PC's UUIDs, active with its device nodes.
/// Every LVM command sees only the group's PVs and makes no system ID ([`scope`]).
pub fn create(log: &mut Log, a: &VgAction) -> Result<(), String> {
    for p in &a.pvs {
        let mut argv = vec!["pvcreate", "--yes"];
        if let Some(u) = &p.uuid {
            argv.extend(["--uuid", u.as_str(), "--norestorefile"]);
        }
        argv.push(&p.device);
        run_lvm(log, a, &argv)?;
    }
    let ext = format!("{}k", a.extent_bytes >> 10);
    let mut argv = vec!["vgcreate", "--yes", "--autobackup", "n", "-s", &ext, &a.name];
    argv.extend(a.pvs.iter().map(|p| p.device.as_str()));
    run_lvm(log, a, &argv)?;
    let real = extent_count(log, a)?;
    let sizes = if real == a.available {
        a.lvs.iter().map(|l| l.extents).collect()
    } else {
        log.line(format!("volume group {}: {real} extents, the plan counted {}: sizes fitted again", a.name, a.available));
        allocate(&a.sizes(), a.orig_free, real).ok_or_else(|| format!("volume group {}: its volumes do not fit in its {real} extents", a.name))?
    };
    for (l, n) in a.lvs.iter().zip(sizes) {
        let n = n.to_string();
        run_lvm(log, a, &["lvcreate", "--yes", "--autobackup", "n", "--wipesignatures", "y", "-l", &n, "-n", &l.name, &a.name])?;
    }
    if a.sets_ids() {
        let dir = private_dir()?;
        let r = (|| -> Result<(), String> {
            let made = dir.join("made.vg");
            let file = dir.join("restore.vg");
            run_lvm(log, a, &["vgcfgbackup", "-f", &made.to_string_lossy(), &a.name])?;
            let text = std::fs::read_to_string(&made).map_err(|e| format!("{}: {e}", made.display()))?;
            write_new(&file, &swap_ids(&text, a))?;
            let f = file.to_string_lossy().into_owned();
            run_lvm(log, a, &["vgchange", "-an", &a.name])?;
            if let Err(e) = run_lvm(log, a, &["vgcfgrestore", "-f", &f, &a.name]) {
                // udev may have activated the new volumes again meanwhile (auto-activation
                // on the PV's uevent): once more, after udev is done
                log.line(format!("vgcfgrestore failed ({e}): once more after udev settles"));
                if has("udevadm") {
                    let _ = run(log, &["udevadm", "settle", "--timeout=30"]);
                }
                run_lvm(log, a, &["vgchange", "-an", &a.name])?;
                run_lvm(log, a, &["vgcfgrestore", "-f", &f, &a.name])?;
            }
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&dir);
        r?;
        log.line(format!("volume group {}: the PC's UUIDs set", a.name));
    }
    run_lvm(log, a, &["vgchange", "-ay", &a.name])?;
    if has("udevadm") {
        let _ = run(log, &["udevadm", "settle", "--timeout=30"]);
    }
    for l in &a.lvs {
        let p = std::path::PathBuf::from(format!("/dev/{}/{}", a.name, l.name));
        let mut ok = p.exists();
        for i in 0..100 {
            if ok {
                break;
            }
            if i == 50 && has("vgmknodes") {
                let _ = run_lvm(log, a, &["vgmknodes", &a.name]);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            ok = p.exists();
        }
        if !ok {
            return Err(format!("{} did not appear after activating the volume group", p.display()));
        }
    }
    Ok(())
}

pub fn deactivate(log: &mut Log, a: &VgAction) {
    let _ = run_lvm(log, a, &["vgchange", "-an", &a.name]);
}

/// The end of a restore that made the group: inactive, and with the PC's system ID when it
/// had one (after that the live system may not see the group as its own).
pub fn finish(log: &mut Log, a: &VgAction) {
    deactivate(log, a);
    if let Some(s) = &a.system_id {
        // a system ID other than the live system's asks for a confirmation: --yes
        let devs: Vec<&str> = a.pvs.iter().map(|p| p.device.as_str()).collect();
        let extra = scope(Some(&devs), has_devicesfile(), false);
        let mut v: Vec<&str> = vec!["vgchange"];
        v.extend(extra.iter().map(String::as_str));
        v.extend(["--yes", "--systemid", s.as_str(), a.name.as_str()]);
        if let Err(e) = run(log, &v) {
            log.line(format!("warning: volume group {}: the PC's system ID {s} could not be set ({e}); the group has none, which every system accepts", a.name));
        }
    }
}

pub fn add_lvm_hook(conf: &str) -> Option<String> {
    let arrays = hook_arrays(conf);
    // the last HOOKS= wins (the file is sourced by bash); HOOKS+= after it add to it
    let (k, a) = arrays.iter().enumerate().rev().find(|(_, a)| !a.append)?;
    if arrays[k..].iter().flat_map(|a| a.words.iter()).any(|(_, w)| is_lvm_hook(w)) {
        return None;
    }
    let (s, end) = (a.start, a.end);
    let inner = &conf[s..end];
    let mut t = conf.to_string();
    if let Some((i, _)) = a.words.iter().find(|(_, w)| unquote(w) == "filesystems") {
        t.insert_str(s + i, "lvm2 ");
    } else if let Some((i, w)) = a.words.iter().find(|(_, w)| unquote(w) == "block") {
        t.insert_str(s + i + w.len(), " lvm2");
    } else if inner.trim().is_empty() {
        t.insert_str(end, "lvm2");
    } else {
        t.insert_str(end, " lvm2");
    }
    Some(t)
}

/// One `HOOKS=(…)` or `HOOKS+=(…)` array of a mkinitcpio file: `start..end` is what is
/// between the parentheses, `words` its words (offset in it, as written: maybe quoted).
struct HookArray {
    append: bool,
    start: usize,
    end: usize,
    words: Vec<(usize, String)>,
}

fn unquote(w: &str) -> &str {
    w.trim_matches(|c| c == '"' || c == '\'')
}

fn is_lvm_hook(w: &str) -> bool {
    matches!(unquote(w), "lvm2" | "sd-lvm2")
}

/// The active `HOOKS` arrays of a mkinitcpio file, in order (commented lines and comments
/// inside an array left out).
fn hook_arrays(conf: &str) -> Vec<HookArray> {
    let mut out = Vec::new();
    let mut pos = 0;
    for line in conf.split_inclusive('\n') {
        let t = line.trim_start();
        let indent = line.len() - t.len();
        let found = [("HOOKS=(", false), ("HOOKS+=(", true)].into_iter().find(|(p, _)| t.starts_with(p));
        if let Some((prefix, append)) = found {
            let start = pos + indent + prefix.len();
            if let Some(len) = conf[start..].find(')') {
                let inner = &conf[start..start + len];
                let b = inner.as_bytes();
                let mut words = Vec::new();
                let mut i = 0;
                while i < b.len() {
                    if b[i] == b'#' {
                        while i < b.len() && b[i] != b'\n' {
                            i += 1;
                        }
                    } else if b[i].is_ascii_whitespace() {
                        i += 1;
                    } else {
                        let s = i;
                        while i < b.len() && !b[i].is_ascii_whitespace() {
                            i += 1;
                        }
                        words.push((s, inner[s..i].to_string()));
                    }
                }
                out.push(HookArray { append, start, end: start + len, words });
            }
        }
        pos += line.len();
    }
    out
}

/// The `HOOKS` mkinitcpio uses with these files sourced in order (`mkinitcpio.conf`, then its
/// drop-ins) have the `lvm2` hook (or the older `sd-lvm2`): `HOOKS=` sets the list, `HOOKS+=`
/// adds to it.
pub fn hooks_have_lvm(files: &[String]) -> bool {
    let mut hooks: Vec<String> = Vec::new();
    for text in files {
        for a in hook_arrays(text) {
            if !a.append {
                hooks.clear();
            }
            hooks.extend(a.words.into_iter().map(|(_, w)| w));
        }
    }
    hooks.iter().any(|w| is_lvm_hook(w))
}

/// The file that sets `HOOKS=` last (index in `files`), where a missing hook is added.
pub fn hooks_file(files: &[String]) -> Option<usize> {
    files.iter().rposition(|t| hook_arrays(t).iter().any(|a| !a.append))
}

/// The whole disks a block device sits on (`sda2` → `sda`; `dm-3` → the disks below it).
pub fn base_disks(name: &str) -> Vec<String> {
    let sys = std::path::Path::new("/sys/class/block").join(name);
    if sys.join("partition").exists() {
        return std::fs::canonicalize(&sys).ok().and_then(|p| p.parent().and_then(|d| d.file_name()).map(|n| vec![n.to_string_lossy().into_owned()])).unwrap_or_default();
    }
    let slaves: Vec<String> = std::fs::read_dir(sys.join("slaves")).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    if slaves.is_empty() {
        return vec![name.to_string()];
    }
    slaves.iter().flat_map(|s| base_disks(s)).collect()
}

/// A physical volume the live system has, seen on every device ([`live_pvs`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LivePv {
    /// Kernel name (`sda2`, `dm-3`).
    pub name: String,
    pub uuid: String,
    pub vg: String,
    pub vg_uuid: String,
    /// The whole disks below it.
    pub disks: Vec<String>,
}

/// The rows `key` of an LVM JSON report of `prog` with `fields`, on every device of the
/// live system: no devices file and no filter of its configuration hide a disk, and
/// foreign groups count too (reading only).
fn live_report(prog: &str, fields: &str, key: &str) -> Vec<serde_json::Map<String, serde_json::Value>> {
    let mut args = scope(None, has_devicesfile(), false);
    args.extend(["--foreign", "--reportformat", "json", "-o", fields].map(String::from));
    let Some(o) = std::process::Command::new(prog).args(&args).output().ok().filter(|o| o.status.success()) else {
        return Vec::new();
    };
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_default();
    v.get("report").and_then(|r| r.as_array()).into_iter().flatten().filter_map(|r| r.get(key).and_then(|a| a.as_array())).flatten().filter_map(|x| x.as_object().cloned()).collect()
}

fn field(r: &serde_json::Map<String, serde_json::Value>, k: &str) -> String {
    r.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string()
}

fn kernel_name(dev: &str) -> String {
    std::fs::canonicalize(dev).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default()
}

/// Every PV of the live system ([`live_report`]); nothing when LVM is not installed.
pub fn live_pvs() -> Vec<LivePv> {
    if !has("pvs") {
        return Vec::new();
    }
    live_report("pvs", "pv_name,pv_uuid,vg_name,vg_uuid", "pv")
        .into_iter()
        .map(|r| {
            let name = kernel_name(&field(&r, "pv_name"));
            let disks = if name.is_empty() { Vec::new() } else { base_disks(&name) };
            LivePv { name, uuid: field(&r, "pv_uuid"), vg: field(&r, "vg_name"), vg_uuid: field(&r, "vg_uuid"), disks }
        })
        .collect()
}

/// The volume UUIDs of the live system's groups `vgs`.
pub fn live_lv_uuids(vgs: &[String]) -> Vec<String> {
    if vgs.is_empty() || !has("lvs") {
        return Vec::new();
    }
    live_report("lvs", "vg_name,lv_uuid", "lv").into_iter().filter(|r| vgs.contains(&field(r, "vg_name"))).map(|r| field(&r, "lv_uuid")).filter(|u| !u.is_empty()).collect()
}

/// The PVs udev has seen (`/dev/disk/by-id/lvm-pv-uuid-<UUID>`), also those LVM itself
/// leaves out: (UUID, whole disks below it).
pub fn udev_pvs() -> Vec<(String, Vec<String>)> {
    let Ok(rd) = std::fs::read_dir("/dev/disk/by-id") else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let uuid = n.strip_prefix("lvm-pv-uuid-")?.to_string();
            let k = kernel_name(&e.path().to_string_lossy());
            Some((uuid, if k.is_empty() { Vec::new() } else { base_disks(&k) }))
        })
        .collect()
}

/// The groups with a PV on `disk` (a whole disk, or the partition itself) and others
/// elsewhere: (group, the other disks). Wiping `disk` would break such a group there.
pub fn split_groups(pvs: &[LivePv], disk: &str) -> Vec<(String, Vec<String>)> {
    let on = |p: &LivePv| p.name == disk || p.disks.iter().any(|d| d == disk);
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for p in pvs.iter().filter(|p| !p.vg.is_empty() && on(p)) {
        if out.iter().any(|(v, _)| *v == p.vg) {
            continue;
        }
        let mut others: Vec<String> = pvs.iter().filter(|q| q.vg == p.vg && !on(q)).flat_map(|q| if q.disks.is_empty() { vec![q.name.clone()] } else { q.disks.clone() }).collect();
        others.sort();
        others.dedup();
        if !others.is_empty() {
            out.push((p.vg.clone(), others));
        }
    }
    out
}

/// An LVM devices file (`system.devices`) without the lines of the PVs `pvids` (UUIDs, with
/// or without dashes): the restored ones, which `vgimportdevices` writes again with the new
/// disk's IDs; the lines of the PC's other disks stay. (text, lines taken out)
pub fn devices_file_without(text: &str, pvids: &[String]) -> (String, usize) {
    let norm = |s: &str| s.replace('-', "").to_ascii_lowercase();
    let drop: Vec<String> = pvids.iter().map(|p| norm(p)).filter(|p| !p.is_empty()).collect();
    let mut out = String::with_capacity(text.len());
    let mut n = 0;
    for line in text.split_inclusive('\n') {
        let pvid = line.split_whitespace().find_map(|w| w.strip_prefix("PVID=")).map(norm);
        if !line.trim_start().starts_with('#') && pvid.is_some_and(|p| drop.contains(&p)) {
            n += 1;
            continue;
        }
        out.push_str(line);
    }
    (out, n)
}

/// The byte ranges of the active `filter = […]` and `global_filter = […]` settings of an LVM
/// configuration text (a setting may span lines; brackets inside quotes do not count).
fn filter_spans(conf: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut skip_to = 0;
    for line in conf.split_inclusive('\n') {
        let start = pos;
        pos += line.len();
        if start < skip_to {
            continue;
        }
        let t = line.trim_start();
        let at = start + (line.len() - t.len());
        let Some(rest) = t.strip_prefix("global_filter").or_else(|| t.strip_prefix("filter")) else { continue };
        if !rest.trim_start().starts_with('=') {
            continue;
        }
        let Some(open) = conf[at..].find('[').map(|i| at + i) else { continue };
        let mut quote = false;
        let mut end = None;
        for (i, c) in conf[open..].char_indices() {
            match c {
                '"' => quote = !quote,
                ']' if !quote => {
                    end = Some(open + i + 1);
                    break;
                }
                _ => {}
            }
        }
        if let Some(e) = end {
            out.push((at, e));
            skip_to = e;
        }
    }
    out
}

/// The active device filter settings of an LVM configuration text, as written.
pub fn filter_settings(conf: &str) -> Vec<String> {
    filter_spans(conf).into_iter().map(|(s, e)| conf[s..e].to_string()).collect()
}

/// The text with the old device names in its filter settings changed: `renames` are kernel
/// names (`sda2` → `nvme0n1p2`, `sda` → `nvme0n1`), each matched as `/dev/<old>` not
/// followed by a letter or digit, the longest first. `None`: no change.
pub fn rewrite_filters(conf: &str, renames: &[(String, String)]) -> Option<String> {
    let mut r: Vec<&(String, String)> = renames.iter().filter(|(o, n)| o != n && !o.is_empty()).collect();
    r.sort_by_key(|(o, _)| std::cmp::Reverse(o.len()));
    let mut out = String::with_capacity(conf.len());
    let mut last = 0;
    for (s, e) in filter_spans(conf) {
        out.push_str(&conf[last..s]);
        let span = &conf[s..e];
        let mut i = 0;
        while i < span.len() {
            let rest = &span[i..];
            let hit = rest.strip_prefix("/dev/").and_then(|after| {
                r.iter().find(|(o, _)| after.starts_with(o.as_str()) && !after[o.len()..].starts_with(|c: char| c.is_ascii_alphanumeric()))
            });
            if let Some((o, n)) = hit {
                out.push_str("/dev/");
                out.push_str(n);
                i += 5 + o.len();
            } else {
                let c = rest.chars().next().unwrap_or(' ');
                out.push(c);
                i += c.len_utf8();
            }
        }
        last = e;
    }
    out.push_str(&conf[last..]);
    (out != conf).then_some(out)
}

/// A filter setting names devices by a link that follows the disk's identity
/// (`/dev/disk/by-id/…`, `by-path`, `by-uuid`…): the new disk has other names there.
pub fn filter_by_identity(setting: &str) -> bool {
    setting.contains("/dev/disk/by-")
}

/// Plan notes for the PC's LVM device filters (`settings`, from the manifest) with the disk
/// names that change (`renames`): what the restore rewrites, and what it cannot.
pub fn filter_notes(settings: &[String], renames: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    for s in settings {
        if filter_by_identity(s) {
            out.push(format!(
                "the PC's LVM device filter `{s}` names disks by their IDs, which the new disk does not have: check /etc/lvm/lvm.conf of the restored system before rebooting, or LVM may not find the volume groups at boot"
            ));
        }
        match rewrite_filters(s, renames) {
            Some(n) => out.push(format!("the PC's LVM device filter `{s}` names the old disk: rewritten to `{n}`")),
            None if !filter_by_identity(s) => out.push(format!("the PC's LVM device filter `{s}` is kept as it is: it must accept the new disk's physical volumes")),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation() {
        // fits: the PC's sizes
        assert_eq!(allocate(&[(10, 2), (20, 20)], 4, 40), Some(vec![10, 20]));
        // the free space goes before a volume shrinks only when nothing can shrink
        assert_eq!(allocate(&[(10, 10), (20, 20)], 4, 31), Some(vec![10, 20]));
        // the shrinkable volume gives room so the free space stays
        assert_eq!(allocate(&[(10, 2), (20, 20)], 4, 31), Some(vec![7, 20]));
        // in proportion to the size, a floor holds
        let a = allocate(&[(100, 10), (300, 10), (50, 45)], 0, 300).unwrap();
        assert!(a.iter().sum::<u64>() <= 300 && a[2] == 45 && a[0] >= 10 && a[1] > a[0], "{a:?}");
        // the free space is given up before refusing
        assert_eq!(allocate(&[(10, 8), (20, 20)], 4, 28), Some(vec![8, 20]));
        assert_eq!(allocate(&[(10, 9), (20, 20)], 4, 28), None);
        // bigger target: sizes kept
        assert_eq!(allocate(&[(10, 1)], 1, 1000), Some(vec![10]));
    }

    #[test]
    fn uuids() {
        assert!(valid_uuid("fhKzh3-wP2L-cSgr-pD3T-hY1n-pzVb-9tAeZn"));
        assert!(!valid_uuid("fhKzh3-wP2L-cSgr-pD3T-hY1n-pzVb"));
        assert!(!valid_uuid("2d5cddec-8777-4aef-9b8d-1d7093e60982"));
    }

    const META: &str = "# Generated by LVM2 version 2.03.31(2) (2025-02-27): Sat Nov 15 10:00:00 2026\n\ncontents = \"Text Format Volume Group\"\nversion = 1\n\ndescription = \"vgcfgbackup -f /tmp/x ArchinstallVg\"\n\ncreation_host = \"live\"\t# Linux live 6.12\ncreation_time = 1794700000\t# Sat Nov 15 10:00:00 2026\n\nArchinstallVg {\n\tid = \"AAAAAA-0000-0000-0000-0000-0000-AAAAAA\"\n\tseqno = 3\n\tformat = \"lvm2\"\n\tstatus = [\"RESIZEABLE\", \"READ\", \"WRITE\"]\n\tflags = []\n\textent_size = 8192\t\t# 4 Megabytes\n\n\tphysical_volumes {\n\n\t\tpv0 {\n\t\t\tid = \"PPPPPP-0000-0000-0000-0000-0000-PPPPPP\"\n\t\t\tdevice = \"/dev/vdc2\"\t# Hint only\n\t\t}\n\t}\n\n\tlogical_volumes {\n\n\t\troot {\n\t\t\tid = \"RRRRRR-0000-0000-0000-0000-0000-RRRRRR\"\n\t\t\tsegment_count = 1\n\n\t\t\tsegment1 {\n\t\t\t\tstart_extent = 0\n\t\t\t\ttype = \"striped\"\n\t\t\t\tstripes = [\n\t\t\t\t\t\"pv0\", 0\n\t\t\t\t]\n\t\t\t}\n\t\t}\n\n\t\thome {\n\t\t\tid = \"HHHHHH-0000-0000-0000-0000-0000-HHHHHH\"\n\t\t}\n\t}\n\n}\n";

    #[test]
    fn metadata_gets_the_pcs_uuids() {
        let a = VgAction {
            name: "ArchinstallVg".into(),
            uuid: Some("vgvgvg-1111-2222-3333-4444-5555-vgvgvg".into()),
            extent_bytes: 4 << 20,
            pvs: vec![],
            lvs: vec![
                LvAction { name: "root".into(), uuid: Some("rootrt-1111-2222-3333-4444-5555-rootrt".into()), orig_extents: 1, min_extents: 1, extents: 1 },
                LvAction { name: "home".into(), uuid: None, orig_extents: 1, min_extents: 1, extents: 1 },
            ],
            orig_free: 0,
            available: 2,
            system_id: None,
        };
        let t = swap_ids(META, &a);
        assert!(t.contains("\tid = \"vgvgvg-1111-2222-3333-4444-5555-vgvgvg\"\n\tseqno = 3"), "{t}");
        assert!(t.contains("\t\t\tid = \"rootrt-1111-2222-3333-4444-5555-rootrt\"\n\t\t\tsegment_count"), "{t}");
        assert!(t.contains("PPPPPP-0000-0000-0000-0000-0000-PPPPPP"), "the PV id stays");
        assert!(t.contains("HHHHHH-0000-0000-0000-0000-0000-HHHHHH"), "a volume without the PC's UUID keeps LVM's");
        assert_eq!(t.len(), META.len());
        assert_eq!(swap_ids(META, &VgAction { uuid: None, lvs: vec![], ..a }), META);
    }

    #[test]
    fn mkinitcpio_hooks() {
        let sd = "MODULES=()\nHOOKS=(base systemd autodetect microcode modconf kms keyboard sd-vconsole block filesystems fsck)\n";
        assert_eq!(add_lvm_hook(sd).unwrap(), "MODULES=()\nHOOKS=(base systemd autodetect microcode modconf kms keyboard sd-vconsole block lvm2 filesystems fsck)\n");
        assert_eq!(add_lvm_hook("HOOKS=(base udev block lvm2 filesystems)\n"), None);
        assert_eq!(add_lvm_hook("HOOKS=(base systemd block sd-lvm2 filesystems)\n"), None);
        // the commented example lines do not count, the last active array wins
        let c = "# HOOKS=(base udev lvm2)\nHOOKS=(base udev block)\n";
        assert_eq!(add_lvm_hook(c).unwrap(), "# HOOKS=(base udev lvm2)\nHOOKS=(base udev block lvm2)\n");
        // several lines
        assert_eq!(add_lvm_hook("HOOKS=(base\n  udev\n  filesystems\n)\n").unwrap(), "HOOKS=(base\n  udev\n  lvm2 filesystems\n)\n");
        assert_eq!(add_lvm_hook("HOOKS=(base udev)\n").unwrap(), "HOOKS=(base udev lvm2)\n");
        assert_eq!(add_lvm_hook("MODULES=()\n"), None);
    }

    #[test]
    fn mkinitcpio_hooks_in_drop_ins_and_quoted() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // a drop-in adds the hook with HOOKS+=: nothing to add, nothing doubled
        let files = s(&["HOOKS=(base udev block filesystems)\n", "HOOKS+=(lvm2)\n"]);
        assert!(hooks_have_lvm(&files));
        // a later HOOKS= drops what came before
        let files = s(&["HOOKS=(base udev block lvm2 filesystems)\n", "HOOKS=(base systemd block filesystems)\n"]);
        assert!(!hooks_have_lvm(&files));
        assert_eq!(hooks_file(&files), Some(1));
        // a drop-in with HOOKS+= only: the hook goes into the file that sets HOOKS
        let files = s(&["HOOKS=(base udev block filesystems)\n", "HOOKS+=(resume)\n"]);
        assert!(!hooks_have_lvm(&files));
        assert_eq!(hooks_file(&files), Some(0));
        assert_eq!(add_lvm_hook(&files[0]).unwrap(), "HOOKS=(base udev block lvm2 filesystems)\n");
        assert_eq!(add_lvm_hook("HOOKS=(base udev)\nHOOKS+=(lvm2)\n"), None);
        // quoted words
        assert_eq!(add_lvm_hook("HOOKS=(\"base\" \"udev\" \"block\" \"filesystems\")\n").unwrap(), "HOOKS=(\"base\" \"udev\" \"block\" lvm2 \"filesystems\")\n");
        assert_eq!(add_lvm_hook("HOOKS=('base' 'block')\n").unwrap(), "HOOKS=('base' 'block' lvm2)\n");
        assert_eq!(add_lvm_hook("HOOKS=(base \"lvm2\" filesystems)\n"), None);
        assert!(hooks_have_lvm(&s(&["HOOKS=(base 'sd-lvm2')\n"])));
        // a comment inside the array does not count
        assert_eq!(add_lvm_hook("HOOKS=(base # lvm2 later\n block filesystems)\n").unwrap(), "HOOKS=(base # lvm2 later\n block lvm2 filesystems)\n");
    }

    #[test]
    fn lvm_commands_see_only_the_restored_pvs() {
        let v = scope(Some(&["/dev/nvme1n1p2", "/dev/mapper/crypt.lvm"]), true, true);
        assert_eq!(v[..2], ["--devicesfile".to_string(), String::new()]);
        assert_eq!(v[2], "--config");
        assert_eq!(
            v[3],
            r#"devices{filter=["a|^/dev/nvme1n1p2$|","a|^/dev/mapper/crypt[.]lvm$|","r|.*|"] global_filter=["a|^/dev/nvme1n1p2$|","a|^/dev/mapper/crypt[.]lvm$|","r|.*|"]} global{system_id_source="none"}"#
        );
        // an older LVM without devices files; the look at every device of the live system
        let v = scope(None, false, false);
        assert_eq!(v, ["--config".to_string(), r#"devices{filter=["a|.*|"] global_filter=["a|.*|"]}"#.to_string()]);
        assert!(lvm_version_has_devicesfile("  LVM version:     2.03.31(2) (2025-02-27)\n  Library version: 1.02.205\n"));
        assert!(lvm_version_has_devicesfile("  LVM version:     2.03.12(2) (2021-05-07)\n"));
        assert!(!lvm_version_has_devicesfile("  LVM version:     2.03.11(2) (2021-01-08)\n"));
        assert!(!lvm_version_has_devicesfile("  LVM version:     2.02.187(2) (2020-03-24)\n"));
        assert!(!lvm_version_has_devicesfile(""));
    }

    #[test]
    fn the_metadata_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = private_dir().unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
        let f = d.join("x.vg");
        write_new(&f, "a").unwrap();
        assert!(write_new(&f, "b").is_err(), "never over a file that exists");
        let link = d.join("link");
        std::os::unix::fs::symlink(&f, &link).unwrap();
        assert!(write_new(&link, "c").is_err(), "never through a symlink");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "a");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn groups_split_over_the_target_and_another_disk() {
        let pv = |name: &str, vg: &str, disk: &str| LivePv { name: name.into(), vg: vg.into(), disks: vec![disk.into()], ..Default::default() };
        let pvs = [pv("sdb1", "data", "sdb"), pv("sdc1", "data", "sdc"), pv("sdb2", "solo", "sdb"), pv("sdb3", "", "sdb"), pv("sdd1", "other", "sdd")];
        assert_eq!(split_groups(&pvs, "sdb"), [("data".to_string(), vec!["sdc".to_string()])]);
        assert!(split_groups(&pvs, "sdd").is_empty());
        // a partition as the device (adding to a filesystem): only PVs on it
        assert_eq!(split_groups(&pvs, "sdb1"), [("data".to_string(), vec!["sdc".to_string()])]);
        assert!(split_groups(&pvs, "sdb2").is_empty());
    }

    #[test]
    fn devices_file_keeps_the_other_disks() {
        let t = "# LVM uses devices listed in this file.\nVERSION=1.1.3\nIDTYPE=sys_wwid IDNAME=eui.0001 DEVNAME=/dev/nvme0n1p2 PVID=Pv0AAA11112222333344445555Pv0AAA PART=2\nIDTYPE=sys_wwid IDNAME=t10.ATA DEVNAME=/dev/sda1 PVID=DataPV11112222333344445555DataPV PART=1\n";
        let (out, n) = devices_file_without(t, &["Pv0AAA-1111-2222-3333-4444-5555-Pv0AAA".into(), String::new()]);
        assert_eq!(n, 1);
        assert!(!out.contains("nvme0n1p2") && out.contains("DEVNAME=/dev/sda1") && out.starts_with("# LVM uses"), "{out}");
        assert_eq!(devices_file_without(t, &[]).0, t);
    }

    #[test]
    fn lvm_conf_filters_follow_the_new_disk() {
        let conf = "devices {\n    # filter = [ \"a|/dev/sda2|\" ]\n    filter = [ \"a|^/dev/sda2$|\", \"a|/dev/sda[0-9]|\",\n               \"r|.*|\" ]\n    global_filter = [ \"a|/dev/sdab1|\" ]\n    scan = [ \"/dev\" ]\n}\n";
        assert_eq!(filter_settings(conf).len(), 2, "{:?}", filter_settings(conf));
        let renames = [("sda".to_string(), "nvme1n1".to_string()), ("sda2".to_string(), "nvme1n1p2".to_string())];
        let new = rewrite_filters(conf, &renames).unwrap();
        assert!(new.contains("# filter = [ \"a|/dev/sda2|\" ]"), "a comment stays: {new}");
        assert!(new.contains("\"a|^/dev/nvme1n1p2$|\", \"a|/dev/nvme1n1[0-9]|\","), "{new}");
        assert!(new.contains("\"a|/dev/sdab1|\""), "another disk's name stays: {new}");
        assert_eq!(rewrite_filters(conf, &[("sdz".into(), "sdy".into())]), None);
        let notes = filter_notes(&filter_settings(conf), &renames);
        assert!(notes[0].contains("rewritten") && notes[1].contains("kept as it is"), "{notes:?}");
        assert!(filter_notes(&["filter = [ \"a|/dev/disk/by-id/wwn-1|\" ]".into()], &renames)[0].contains("by their IDs"));
    }
}
