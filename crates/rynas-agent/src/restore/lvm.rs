//! LVM in the bare-metal restore: simple layouts made again as the PC had them.
//!
//! - Physical volumes on partitions or on LUKS containers of the restored disks; volume
//!   groups with the same names (the boot command line and fstab may name `/dev/<vg>/<lv>` or
//!   `/dev/mapper/<vg>-<lv>`) and extent size; linear logical volumes with the same names and
//!   sizes, in the PC's order. Thin pools, snapshots, raid/mirror/striped and cache volumes
//!   are refused ([`LvmLayout::unsupported`]); so is a group with a PV on a disk that gets no
//!   target, a PV on a whole disk or on md, and a group whose name another attached disk uses.
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
/// What a LUKS2 header takes before the data (LUKS1: 2 MiB; the larger one is assumed).
pub const LUKS_HEADER: u64 = 16 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PvAction {
    /// `/dev/…` it is made on: a partition, or `/dev/mapper/<name>` of a LUKS container.
    pub device: String,
    /// The PC's UUID (`None`: LVM makes a new one).
    pub uuid: Option<String>,
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
    /// Its disk gets no target.
    NoTarget,
    /// Not something this version recreates (why).
    Unsupported(String),
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

/// The filesystem bytes a volume must hold (used + room), when the backup restores one on it
/// (directly or inside LUKS); `None`: keep its size.
fn needed_on(m: &Manifest, vg: &str, lv: &str) -> Option<u64> {
    let on = |s: &FsSource| matches!(s, FsSource::Lvm { vg: v, lv: l } if v == vg && l == lv);
    let luks: Vec<usize> = m.luks.iter().enumerate().filter(|(_, l)| on(&l.source)).map(|(i, _)| i).collect();
    let f = m.filesystems.iter().find(|f| on(&f.source) || matches!(f.source, FsSource::Luks { luks: i } if luks.contains(&i)))?;
    if f.capture == Some(CaptureMethod::Skipped) || f.used_bytes == 0 || !["ext2", "ext3", "ext4", "xfs", "btrfs", "vfat", "f2fs"].contains(&f.fstype.as_str()) {
        return None;
    }
    let extra = if luks.is_empty() { 0 } else { LUKS_HEADER };
    Some(super::plan::needed(f.used_bytes) + extra)
}

/// The groups to make again: those with a PV on a restored disk. `pv_target` says where a PV
/// of the manifest goes; `in_use` whether an attached device other than the targets has an ID
/// (`vg:<name>` for a group's name).
pub fn plan(m: &Manifest, layout: &LvmLayout, pv_target: &dyn Fn(&str) -> PvTarget, in_use: &dyn Fn(&str) -> bool, notes: &mut Vec<String>) -> Result<Vec<VgAction>, String> {
    let mut out = Vec::new();
    for vg in &layout.vgs {
        let places: Vec<(&LvmPv, PvTarget)> = vg.pvs.iter().map(|p| (p, pv_target(&p.device))).collect();
        if places.is_empty() || places.iter().all(|(_, t)| *t == PvTarget::NoTarget) {
            if m.filesystems.iter().any(|f| matches!(&f.source, FsSource::Lvm { vg: v, .. } if *v == vg.name)) {
                notes.push(format!("volume group {}: not on a restored disk, left out", vg.name));
            }
            continue;
        }
        if let Some((p, PvTarget::Unsupported(why))) = places.iter().find(|(_, t)| matches!(t, PvTarget::Unsupported(_))) {
            return Err(format!("volume group {}: {} ({why}); recreate it by hand, then restore the files from the web interface", vg.name, p.device));
        }
        let missing: Vec<&str> = places.iter().filter(|(_, t)| *t == PvTarget::NoTarget).map(|(p, _)| p.device.as_str()).collect();
        if !missing.is_empty() {
            return Err(format!("volume group {} also has physical volumes on disks that get no target ({}): restore onto as many disks, or recreate it by hand", vg.name, missing.join(", ")));
        }
        let refused: Vec<String> = LvmLayout { vgs: vec![vg.clone()] }.unsupported();
        if !refused.is_empty() {
            return Err(format!(
                "this version of the restore recreates linear LVM volumes only, not: {}; recreate the volume group by hand, then restore the files from the web interface",
                refused.join("; ")
            ));
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
            pvs.push(PvAction { device: device.clone(), uuid: keep(&format!("physical volume {}", p.device), &p.uuid, notes) });
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
        let mut a = VgAction { name: vg.name.clone(), uuid: keep(&format!("volume group {}", vg.name), &vg.uuid, notes), extent_bytes: extent, pvs, lvs, orig_free, available };
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
    out
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

/// LVM's count of the group's extents (after `vgcreate`).
fn extent_count(log: &mut Log, name: &str) -> Result<u64, String> {
    let out = run(log, &["vgs", "--noheadings", "-o", "vg_extent_count", name])?;
    out.trim().parse().map_err(|_| format!("vgs {name}: unexpected answer {:?}", out.trim()))
}

/// Makes the group: PVs (with their UUIDs), the group, the volumes in the PC's order (sized
/// again for LVM's real count of extents), the PC's UUIDs, active with its device nodes.
pub fn create(log: &mut Log, a: &VgAction) -> Result<(), String> {
    for p in &a.pvs {
        let mut argv = vec!["pvcreate", "--yes"];
        if let Some(u) = &p.uuid {
            argv.extend(["--uuid", u.as_str(), "--norestorefile"]);
        }
        argv.push(&p.device);
        run(log, &argv)?;
    }
    let ext = format!("{}k", a.extent_bytes >> 10);
    let mut argv = vec!["vgcreate", "--yes", "--autobackup", "n", "-s", &ext, &a.name];
    argv.extend(a.pvs.iter().map(|p| p.device.as_str()));
    run(log, &argv)?;
    let real = extent_count(log, &a.name)?;
    let sizes = if real == a.available {
        a.lvs.iter().map(|l| l.extents).collect()
    } else {
        log.line(format!("volume group {}: {real} extents, the plan counted {}: sizes fitted again", a.name, a.available));
        allocate(&a.sizes(), a.orig_free, real).ok_or_else(|| format!("volume group {}: its volumes do not fit in its {real} extents", a.name))?
    };
    for (l, n) in a.lvs.iter().zip(sizes) {
        let n = n.to_string();
        run(log, &["lvcreate", "--yes", "--autobackup", "n", "--wipesignatures", "y", "-l", &n, "-n", &l.name, &a.name])?;
    }
    if a.sets_ids() {
        let file = std::env::temp_dir().join(format!("rynas-restore-{}-{}.vg", a.name, std::process::id()));
        let f = file.to_string_lossy().into_owned();
        let r = (|| -> Result<(), String> {
            run(log, &["vgcfgbackup", "-f", &f, &a.name])?;
            let text = std::fs::read_to_string(&file).map_err(|e| format!("{f}: {e}"))?;
            std::fs::write(&file, swap_ids(&text, a)).map_err(|e| format!("{f}: {e}"))?;
            run(log, &["vgchange", "-an", &a.name])?;
            run(log, &["vgcfgrestore", "-f", &f, &a.name])?;
            Ok(())
        })();
        let _ = std::fs::remove_file(&file);
        r?;
        log.line(format!("volume group {}: the PC's UUIDs set", a.name));
    }
    run(log, &["vgchange", "-ay", &a.name])?;
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
                let _ = run(log, &["vgmknodes", &a.name]);
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

/// Deactivates a group the restore made (best effort: the end, or a failure).
pub fn deactivate(log: &mut Log, name: &str) {
    let _ = run(log, &["vgchange", "-an", name]);
}

/// `HOOKS=(…)` of a mkinitcpio configuration with `lvm2` added when neither `lvm2` nor the
/// older `sd-lvm2` is there: before `filesystems`, else after `block`, else at the end
/// (today's `lvm2` hook serves both the udev and the systemd initramfs). `None`: no change
/// (no active `HOOKS=` array, or the hook is there).
pub fn add_lvm_hook(conf: &str) -> Option<String> {
    // the last active HOOKS= wins (the file is sourced by bash)
    let mut start = None;
    let mut pos = 0;
    for line in conf.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("HOOKS=(") {
            start = Some(pos + (line.len() - t.len()) + "HOOKS=(".len());
        }
        pos += line.len();
    }
    let s = start?;
    let end = s + conf[s..].find(')')?;
    let inner = &conf[s..end];
    let words: Vec<(usize, &str)> = inner
        .char_indices()
        .filter(|(i, c)| !c.is_whitespace() && (*i == 0 || inner[..*i].ends_with(char::is_whitespace)))
        .map(|(i, _)| (i, inner[i..].split_whitespace().next().unwrap_or("")))
        .collect();
    if words.iter().any(|(_, w)| *w == "lvm2" || *w == "sd-lvm2") {
        return None;
    }
    let mut t = conf.to_string();
    if let Some((i, _)) = words.iter().find(|(_, w)| *w == "filesystems") {
        t.insert_str(s + i, "lvm2 ");
    } else if let Some((i, w)) = words.iter().find(|(_, w)| *w == "block") {
        t.insert_str(s + i + w.len(), " lvm2");
    } else if inner.trim().is_empty() {
        t.insert_str(end, "lvm2");
    } else {
        t.insert_str(end, " lvm2");
    }
    Some(t)
}

/// The disk index of a PV device of the manifest (a partition, or a LUKS container on one).
pub fn pv_disk(m: &Manifest, device: &str) -> Option<usize> {
    let part = |disk: usize, number: u32| m.disks.get(disk).map(|d| format!("/dev/{}", super::plan::part_name(&d.name, number)));
    for (i, d) in m.disks.iter().enumerate() {
        if d.table.as_ref().is_some_and(|t| t.partitions.iter().any(|p| part(i, p.number).as_deref() == Some(device))) {
            return Some(i);
        }
    }
    m.luks.iter().find(|l| l.device == device || (!l.mapper.is_empty() && format!("/dev/mapper/{}", l.mapper) == device)).and_then(|l| match l.source {
        FsSource::Partition { disk, .. } => Some(disk),
        _ => None,
    })
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
}
