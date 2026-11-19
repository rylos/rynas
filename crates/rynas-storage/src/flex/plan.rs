//! Plans of flexible RAID (the write side): a new volume, adding disks, replacing a disk (with
//! a bigger one, or one that failed or vanished) and completing an interrupted growth.
//!
//! A new volume is a fixed sequence: partitions, one md array per tier, the volume group and
//! its logical volume, then LUKS and btrfs as for any volume. A growth is planned one step at
//! a time on a [`View`] of the volume: [`next`] picks the step the state calls for and
//! [`View::apply`] predicts what it does, until nothing is left. Every step is chosen from
//! state (a partition already in its array is not added again, a logical volume is extended
//! only while its group has free space), so planning again after a crash, from the inventory
//! of that moment, plans only what is left ([`plan_complete`]). The same `apply` is the
//! tier-state model of the tests.
//!
//! Order of a growth, lowest tier first so that a crash leaves less undone:
//! - partitions for every tier a disk joins (a disk in use gets its new partition with
//!   `sgdisk --new` and `partx --update`: the kernel cannot re-read the table of a busy
//!   disk, and a recent udev may have added the partition already);
//! - new top tiers created and **recorded before** they become physical volumes (a volume
//!   group with an array the boot does not assemble would come up partial);
//! - per tier: bitmap off, level change, `--add`, `--grow --raid-devices`, wait, bitmap on;
//!   then `pvresize`, `lvextend`, `cryptsetup resize` and `btrfs filesystem resize max`
//!   before the next tier.
//!
//! A three-way mirror of a 2-disk tolerance volume becomes RAID6 through RAID5 (md has no
//! direct path): one mirror is taken out, the 2-disk mirror becomes RAID5, grows over the
//! disks, then takes the Q parity (`--layout=preserve`). The tier tolerates one disk until
//! then, and the plan says so. The RAID6 keeps that layout (Q on one disk, not rotated)
//! until its next reshape: normalising it at once would be a second full reshape of the
//! tier, while a later growth restripes it anyway and takes the standard layout on the way
//! (`--raid-devices=N --layout=normalise`; mdadm refuses to grow a RAID6 of that layout
//! without being told which layout to keep).
//!
//! A growth or replace is recorded in the volume's record before its first step
//! ([`FlexChange`], cleared when it ends): the same request again, or complete, plans from
//! that record and the state of the moment ([`recorded_change`], [`resume_shapes`]), so a
//! change that stopped at any step (power loss, rynasd restarted during a long wait, a
//! failed or cancelled step) goes on to its end, the replaced disk cleared included. Without
//! that record nothing outside the tiers is taken: a disk in no tier is never adopted, and
//! a disk is only ever erased when a request named it.

use rynas_api_types::storage::{
    CompletePlanRequest, DiskRef, DiskSummary, FlexChange, FlexLayout, FlexTier, GrowPlanRequest,
    MemberRole, PlanAction, PlanArgs, PlanIssueCode, PlanStep, PlanStepKind, PlanWarningCode,
    RaidLevel, ReplacePlanRequest, ScrubState, StoragePlan, VolumePlanRequest,
};

use crate::Inventory;
use crate::flex::detect::{change_disks, find_disk, mdadm_uuid, outside_tier, resolve_pv};
use crate::flex::tiers::{self, DiskSize, Tier};
use crate::plan::{
    D, END_RESERVE, LUKS_HEADER, MIB, PART_START, check_new_disk, cmd, cmd_secret,
    config, d, d_on, destroying, fail, finish, level_name, mix_warnings, part_name, valid_label,
    volume_top, wait, warn, warn_on,
};

/// Extent size of a new volume group (the LVM default); LVM arithmetic shared with the web
/// interface's capacity calculator.
pub use rynas_api_types::capacity::EXTENT;
use rynas_api_types::capacity::{lvm_usable, md_array_size, tier_pv_bytes};
/// A LUKS mapping smaller than its logical volume by more than this was not resized.
const LUKS_SLACK: u64 = 32 * MIB;
/// Reshape speed the time estimate assumes, per tier (bytes/s).
const RESHAPE_SPEED: u64 = 50_000_000;
/// A scrub older than this (days) is not recent.
pub const SCRUB_MAX_AGE_DAYS: u64 = 30;
/// `old` of a replace request: the disk gone from the degraded tiers.
pub const MISSING: &str = "missing";
/// The logical volume of every flexible volume.
pub const LV_NAME: &str = "data";
/// mdadm.conf, where the record of a tier ends up.
const MDADM_CONF: &str = "/etc/mdadm/mdadm.conf";

/// Volume group of a volume.
pub fn vg_name(label: &str) -> String {
    format!("rynas_{label}")
}

/// Device of the logical volume of a volume.
pub fn lv_path(label: &str) -> String {
    format!("/dev/{}/{LV_NAME}", vg_name(label))
}

/// Device name of a tier array a plan creates.
pub fn tier_dev(label: &str, index: u32) -> String {
    format!("/dev/md/{label}_t{index}")
}

/// Partition label of a tier slice.
pub fn tier_partlabel(label: &str, index: u32) -> String {
    format!("rynas-{label}-t{index}")
}

fn md_level_arg(l: RaidLevel) -> &'static str {
    match l {
        RaidLevel::Raid5 => "5",
        RaidLevel::Raid6 => "6",
        _ => "1",
    }
}

fn natural(v: &mut [String]) {
    v.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
}

// ------------------------------------------------------------------------------- view

/// A partition of a tier slice (label `rynas-<label>-t<tier>`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VPart {
    pub tier: u32,
    pub kname: String,
    pub start: u64,
    pub end: u64,
    /// Carries no md superblock: never joined, or cleared after a removal. Only fresh
    /// partitions are added to an array or make a new one; a stale one (a member taken out,
    /// a failed disk, a superblock of an array that is not running) is left alone.
    pub fresh: bool,
    /// Partition number (the tier's, as the plans make them).
    pub partn: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VDisk {
    pub name: String,
    pub size: u64,
    pub parts: Vec<VPart>,
    /// Partitions that are not slices of this volume: the disk is never cleared whole.
    pub others: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VMember {
    pub part: String,
    pub disk: String,
    pub role: MemberRole,
}

/// One tier: its array (or the array a plan will create) and its LVM physical volume.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VTier {
    pub index: u32,
    pub start: u64,
    pub end: u64,
    /// The device commands name (`/dev/md126`; `/dev/md/<label>_tN` for an array a plan
    /// creates); empty while only planned.
    pub dev: String,
    /// Kernel name, for the sysfs waits.
    pub kname: String,
    pub level: Option<RaidLevel>,
    pub raid_disks: u32,
    pub members: Vec<VMember>,
    /// Disks of a planned tier.
    pub planned: Vec<String>,
    pub bitmap: bool,
    /// md operation running ("resync", "recovery", "reshape", "check").
    pub sync: Option<String>,
    /// A reshape stopped half way.
    pub interrupted: bool,
    /// Member being copied away (`--replace`).
    pub replacing: Option<String>,
    pub size: u64,
    /// Size of its physical volume; `None`: not one yet.
    pub pv: Option<u64>,
    pub in_vg: bool,
    pub recorded: bool,
}

impl VTier {
    pub fn created(&self) -> bool {
        !self.dev.is_empty()
    }

    fn count(&self, role: MemberRole) -> usize {
        self.members.iter().filter(|m| m.role == role).count()
    }

    pub fn active(&self) -> usize {
        self.count(MemberRole::Active)
    }

    /// Slots without an in-sync member.
    pub fn degraded(&self) -> u32 {
        self.raid_disks.saturating_sub(self.active() as u32)
    }

    fn member(&self, part: &str) -> Option<&VMember> {
        self.members.iter().find(|m| m.part == part)
    }

    /// Disks holding the slice in the array (any role).
    fn member_disks(&self) -> Vec<String> {
        let mut v: Vec<String> = self.members.iter().map(|m| m.disk.clone()).collect();
        v.sort();
        v.dedup();
        natural(&mut v);
        v
    }

    fn height(&self) -> u64 {
        self.end - self.start
    }

    /// The array size md gives the level and member count.
    fn formula_size(&self) -> u64 {
        self.level
            .map_or(0, |l| md_array_size(l, self.raid_disks as usize, self.height()))
    }

    /// The wait for the md operation running: `sync_action` idle, and no more slots empty
    /// than the step leaves (another disk still missing; none on a healthy tier, the
    /// executor's default).
    fn wait_condition(&self) -> String {
        let filled = self.active() + self.count(MemberRole::Rebuilding);
        let left = self.raid_disks.saturating_sub(filled as u32);
        let idle = format!("/sys/block/{}/md/sync_action == idle", self.kname);
        if left == 0 {
            idle
        } else {
            format!("{idle}, degraded <= {left}")
        }
    }

    fn to_api(&self) -> FlexTier {
        let mut members = if self.created() {
            self.members
                .iter()
                .filter(|m| m.role != MemberRole::Faulty)
                .map(|m| m.disk.clone())
                .collect()
        } else {
            self.planned.clone()
        };
        members.sort();
        members.dedup();
        natural(&mut members);
        FlexTier {
            index: self.index,
            start: self.start,
            end: self.end,
            level: self.level,
            members,
            array: (!self.kname.is_empty()).then(|| self.kname.clone()),
            usable: self.formula_size().max(self.size),
            ..Default::default()
        }
    }
}

/// The state of one flexible volume as the growth planner sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct View {
    pub label: String,
    pub tolerance: u32,
    pub extent: u64,
    pub vg_exists: bool,
    pub vg_free: u64,
    pub lv_exists: bool,
    pub lv_size: u64,
    /// Open LUKS mapping on the logical volume: (mapper name, size).
    pub luks: Option<(String, u64)>,
    /// Mounted btrfs: (mount point, bytes it uses of its device).
    pub fs: Option<(String, u64)>,
    pub disks: Vec<VDisk>,
    pub tiers: Vec<VTier>,
    /// The records of rynas are known: tiers are recorded before they join the group.
    pub records_known: bool,
}

/// The md level of an array as lsblk/mdadm name it.
fn raid_level(s: &str) -> Option<RaidLevel> {
    match s {
        "raid1" => Some(RaidLevel::Raid1),
        "raid5" => Some(RaidLevel::Raid5),
        "raid6" => Some(RaidLevel::Raid6),
        _ => None,
    }
}

impl View {
    /// A volume that does not exist yet over blank `disks` (the model of a new volume).
    pub fn blank(label: &str, tolerance: u32, disks: &[DiskSize]) -> Self {
        View {
            label: label.into(),
            tolerance,
            extent: EXTENT,
            disks: disks
                .iter()
                .map(|(n, s)| VDisk {
                    name: n.clone(),
                    size: *s,
                    ..Default::default()
                })
                .collect(),
            records_known: true,
            ..Default::default()
        }
    }

    /// The view of the flexible volume `layout` (label `label`) in the inventory.
    pub fn from_inventory(inv: &Inventory, layout: &FlexLayout, label: &str) -> Self {
        let g = &inv.graph;
        let vg = &layout.vg;
        let record = inv.record(label);
        let vg_report = inv.lvm.vg(vg);
        let mut v = View {
            label: label.into(),
            // the record's; without one, from the tiers below
            tolerance: record.map_or(0, |r| r.level.tolerance()),
            extent: vg_report.map_or(EXTENT, |x| x.extent_size.max(MIB)),
            vg_exists: true,
            vg_free: vg_report.map_or(0, |x| x.free),
            lv_exists: true,
            lv_size: inv.lvm.lv(vg, &layout.lv).map_or(layout.size, |l| l.size),
            records_known: inv.records.is_some(),
            ..Default::default()
        };
        let lvdev = g.get(&layout.lv_device);
        let crypt = lvdev.and_then(|l| {
            l.children
                .iter()
                .filter_map(|c| g.get(c))
                .find(|c| c.kind == "crypt")
        });
        v.luks = crypt.map(|c| (c.name.clone(), c.size));
        let fs_dev = crypt.or(lvdev);
        v.fs = fs_dev.and_then(|d| {
            let f = inv
                .filesystems
                .iter()
                .find(|f| d.uuid.as_deref() == Some(f.uuid.as_str()))?;
            let mnt = f.mountpoint.clone()?;
            let used = f
                .devices
                .iter()
                .find(|x| g.find(&x.path).is_some_and(|b| b.kname == d.kname))
                .map_or(d.size, |x| x.size);
            Some((mnt, used))
        });

        // tier arrays: those of the layout, and arrays of the volume outside the group (recorded,
        // or named and labeled after it on its disks: never one of another system)
        let mut arrays: Vec<(u32, &rynas_api_types::storage::MdArray)> = Vec::new();
        for t in &layout.tiers {
            if let Some(a) = t.array.as_deref().and_then(|n| inv.arrays.iter().find(|a| a.name == n)) {
                arrays.push((t.index, a));
            }
        }
        let mut vol_disks: Vec<String> = layout.tiers.iter().flat_map(|t| t.members.clone()).collect();
        vol_disks.extend(change_disks(inv, record));
        for a in &inv.arrays {
            if arrays.iter().any(|(_, x)| x.name == a.name) {
                continue;
            }
            if let Some(n) = outside_tier(inv, label, a, &vol_disks, record) {
                arrays.push((n, a));
            }
        }
        arrays.sort_by_key(|(n, _)| *n);
        for (index, a) in arrays {
            let parts: Vec<_> = a.members.iter().filter_map(|m| g.get(&m.device)).collect();
            let (start, end) = parts
                .first()
                .and_then(|p| Some((p.start?, p.start? + p.size)))
                .or_else(|| {
                    layout
                        .tiers
                        .iter()
                        .find(|t| t.array.as_deref() == Some(a.name.as_str()))
                        .map(|t| (t.start, t.end))
                })
                .unwrap_or((0, 0));
            let pv = inv
                .lvm
                .pvs
                .iter()
                .find(|p| resolve_pv(inv, &p.name).is_some_and(|d| d.kname == a.name));
            let recorded = record.is_some_and(|r| {
                a.uuid.as_deref().is_some_and(|u| {
                    let u = mdadm_uuid(u).unwrap_or_else(|| u.to_string());
                    r.tiers.iter().any(|t| t.md_uuid.eq_ignore_ascii_case(&u))
                })
            });
            v.tiers.push(VTier {
                index,
                start,
                end,
                dev: a.path.clone(),
                kname: a.name.clone(),
                level: raid_level(&a.level),
                raid_disks: a.raid_disks,
                members: a
                    .members
                    .iter()
                    .filter(|m| !matches!(m.role, MemberRole::Journal | MemberRole::Unknown))
                    .map(|m| VMember {
                        part: m.device.clone(),
                        disk: m
                            .disk
                            .clone()
                            .or_else(|| g.disks_under(&m.device).into_iter().next())
                            .unwrap_or_default(),
                        role: m.role,
                    })
                    .collect(),
                planned: Vec::new(),
                bitmap: a.bitmap.as_deref().is_some_and(|b| b != "none"),
                sync: a.sync.as_ref().map(|s| s.action.clone()),
                interrupted: a.reshape_position.is_some()
                    && a.sync.as_ref().is_none_or(|s| s.action != "reshape"),
                // a copy under way (`--replace`): md marks the member it copies away
                replacing: a
                    .members
                    .iter()
                    .find(|m| m.role == MemberRole::Active && m.state.contains("want_replacement"))
                    .map(|m| m.device.clone()),
                size: a.size.max(pv.map_or(0, |p| p.dev_size)),
                pv: pv.map(|p| p.size),
                in_vg: pv.is_some_and(|p| &p.vg == vg),
                recorded,
            });
        }

        if v.tolerance == 0 {
            // never less than a tier has: a three-way mirror half way to RAID6 (a 2-disk
            // mirror or RAID5 for a while) does not make a 2-disk tolerance volume a 1-disk
            // one, its lowest tier (RAID6, every disk) still says 2
            v.tolerance = v
                .tiers
                .iter()
                .filter_map(|t| t.level.map(|l| level_tolerance(l, t.raid_disks)))
                .max()
                .unwrap_or(layout.tolerance)
                .clamp(1, 2);
        }

        // the disks: those of the tiers, and any disk with a slice of this volume
        let mut names: Vec<String> = v.tiers.iter().flat_map(|t| t.member_disks()).collect();
        let plabel = format!("rynas-{label}-t");
        for d in g.disks() {
            let has = d.children.iter().filter_map(|c| g.get(c)).any(|p| {
                p.partlabel.as_deref().is_some_and(|l| l.starts_with(&plabel))
            });
            if has {
                names.push(d.kname.clone());
            }
        }
        names.sort();
        names.dedup();
        natural(&mut names);
        for n in names {
            v.add_disk(inv, &n);
        }
        v
    }

    /// Tiers whose slices exist above the top tier but have no array yet (a growth stopped
    /// between the partitions and `mdadm --create`) become planned tiers. A crash while the
    /// disks in use got their partitions one by one leaves some without theirs: the tier is
    /// planned over every disk of the tier below that reaches its end, as the tier
    /// calculator made it, and the missing partitions are made again
    /// ([`growth_steps`] without a target).
    ///
    /// Only slices on disks in a tier, or `named` (the new disks of the change recorded as
    /// under way), count: a disk in no tier is never adopted silently, its slices may be left
    /// over from anything (another system's volume of the same name, a growth that never
    /// added it).
    pub fn discover_planned(&mut self, named: &[String]) {
        let mut next = self.tiers.iter().map(|t| t.index).max().unwrap_or(0) + 1;
        loop {
            let slices: Vec<(String, VPart)> = self
                .disks
                .iter()
                .filter(|d| named.contains(&d.name) || self.in_some_tier(&d.name))
                .flat_map(|d| {
                    d.parts
                        .iter()
                        .filter(|p| p.tier == next)
                        .map(move |p| (d.name.clone(), p.clone()))
                })
                .collect();
            let Some((_, first)) = slices.first() else {
                break;
            };
            if slices.iter().any(|(_, p)| p.start != first.start || p.end != first.end) {
                break;
            }
            let below = self.tiers.iter().find(|t| t.index + 1 == next);
            let in_below = |disk: &str| {
                below.is_some_and(|t| {
                    t.planned.iter().any(|d| d == disk)
                        || t.members.iter().any(|m| m.disk == disk && m.role != MemberRole::Faulty)
                })
            };
            let planned: Vec<String> = self
                .disks
                .iter()
                .filter(|d| named.contains(&d.name) || self.in_some_tier(&d.name))
                .filter(|d| {
                    slices.iter().any(|(n, _)| n == &d.name)
                        || (!d.parts.iter().any(|p| p.tier == next)
                            && in_below(&d.name)
                            && tiers::reach(d.size) >= first.end)
                })
                .map(|d| d.name.clone())
                .collect();
            if planned.len() < tiers::min_members(self.tolerance) {
                break;
            }
            self.tiers.push(VTier {
                index: next,
                start: first.start,
                end: first.end,
                level: tiers::tier_level(self.tolerance, planned.len()),
                raid_disks: planned.len() as u32,
                planned,
                ..Default::default()
            });
            next += 1;
        }
    }

    /// Adds a disk of the inventory (with its slices of this volume) if missing.
    pub fn add_disk(&mut self, inv: &Inventory, name: &str) {
        if self.disks.iter().any(|d| d.name == name) {
            return;
        }
        let g = &inv.graph;
        let size = inv
            .disks
            .iter()
            .find(|d| d.name == name)
            .map(|d| d.size)
            .or_else(|| g.get(name).map(|d| d.size))
            .unwrap_or(0);
        let plabel = format!("rynas-{}-t", self.label);
        let slice_tier = |p: &crate::lsblk::BlockDev| -> Option<u32> {
            p.partlabel.as_deref()?.strip_prefix(&plabel)?.parse().ok()
        };
        let all_parts: Vec<&crate::lsblk::BlockDev> = g.get(name).map_or_else(Vec::new, |d| {
            d.children.iter().filter_map(|c| g.get(c)).filter(|p| p.kind == "part").collect()
        });
        let others = all_parts.iter().filter(|p| slice_tier(p).is_none()).count() as u32;
        let parts = all_parts
            .iter()
            .filter_map(|p| {
                let tier = slice_tier(p)?;
                let start = p.start?;
                // an md superblock: of the tier's array (a member, or one taken out), or of
                // an array that is not running; never added or created over
                let stale = p.fstype.as_deref() == Some("linux_raid_member");
                Some(VPart {
                    tier,
                    kname: p.kname.clone(),
                    start,
                    end: start + p.size,
                    fresh: !stale,
                    partn: p.partn.unwrap_or(tier),
                })
            })
            .collect();
        self.disks.push(VDisk {
            name: name.into(),
            size,
            parts,
            others,
        });
    }

    fn disk_mut(&mut self, name: &str) -> &mut VDisk {
        if let Some(i) = self.disks.iter().position(|d| d.name == name) {
            return &mut self.disks[i];
        }
        self.disks.push(VDisk {
            name: name.into(),
            ..Default::default()
        });
        self.disks.last_mut().expect("just pushed")
    }

    fn tier_by_dev(&mut self, dev: &str) -> Option<&mut VTier> {
        self.tiers
            .iter_mut()
            .find(|t| t.dev == dev || (!t.kname.is_empty() && dev == format!("/dev/{}", t.kname)))
    }

    /// The slice partition of `disk` for tier `t` (right offsets), if any.
    fn slice_of<'a>(&'a self, disk: &str, t: &VTier) -> Option<&'a VPart> {
        self.disks
            .iter()
            .find(|d| d.name == disk)?
            .parts
            .iter()
            .find(|p| p.tier == t.index && p.start == t.start && p.end == t.end)
    }

    /// Fresh slice partitions of tier `t` that are not in its array, on the disks of the
    /// volume (a member of a tier, or planned in one) and on the goal's new disks. Never on
    /// the disk being replaced (once cleared, its partitions are fresh too), nor on a disk
    /// in no tier that the goal does not name: slices left there (a replaced disk whose
    /// partition table could not be cleared after its superblocks were, or a new disk
    /// partitioned by a growth that stopped before adding it) are never adopted silently;
    /// adding that disk again clears it ([`partition_steps`]).
    fn candidates(&self, t: &VTier, g: &Goal) -> Vec<&VPart> {
        self.disks
            .iter()
            .filter(|d| g.old.as_deref() != Some(d.name.as_str()))
            .filter(|d| g.new.contains(&d.name) || self.in_some_tier(&d.name))
            .flat_map(|d| d.parts.iter())
            .filter(|p| p.tier == t.index && p.start == t.start && p.end == t.end)
            .filter(|p| p.fresh && t.member(&p.kname).is_none())
            .collect()
    }

    /// The disk holds a member of a tier (any role) or is planned in one.
    fn in_some_tier(&self, disk: &str) -> bool {
        self.tiers
            .iter()
            .any(|t| t.planned.iter().any(|d| d == disk) || t.members.iter().any(|m| m.disk == disk))
    }

    fn part_disk(&self, kname: &str) -> String {
        self.disks
            .iter()
            .find(|d| d.parts.iter().any(|p| p.kname == kname))
            .map(|d| d.name.clone())
            .unwrap_or_default()
    }

    /// The created tiers as the tier calculator sees them.
    pub fn shapes(&self) -> Vec<Tier> {
        self.tiers
            .iter()
            .filter(|t| t.created())
            .map(|t| Tier {
                index: t.index,
                start: t.start,
                end: t.end,
                members: t.member_disks(),
                level: t.level,
            })
            .collect()
    }

    /// The disks of the created tiers with their sizes.
    pub fn layout_disks(&self) -> Vec<DiskSize> {
        let mut names: Vec<String> = self
            .tiers
            .iter()
            .filter(|t| t.created())
            .flat_map(|t| t.member_disks())
            .collect();
        names.sort();
        names.dedup();
        natural(&mut names);
        names
            .into_iter()
            .map(|n| {
                let size = self.disks.iter().find(|d| d.name == n).map_or(0, |d| d.size);
                (n, size)
            })
            .collect()
    }

    /// Bytes under the filesystem (LUKS mapping or logical volume).
    fn fs_device_size(&self) -> u64 {
        self.luks.as_ref().map_or(self.lv_size, |(_, s)| *s)
    }

    pub fn tiers_api(&self) -> Vec<FlexTier> {
        self.tiers.iter().map(VTier::to_api).collect()
    }

    /// Predicts what `step` does (the tier-state model).
    pub fn apply(&mut self, step: &PlanStep) {
        match &step.action {
            PlanAction::Config { file, entry } => {
                if file == MDADM_CONF {
                    for i in tier_indices(&self.label, entry) {
                        if let Some(t) = self.tiers.iter_mut().find(|t| t.index == i) {
                            t.recorded = true;
                        }
                    }
                }
            }
            PlanAction::Wait { condition } => {
                let kname = condition
                    .strip_prefix("/sys/block/")
                    .and_then(|r| r.split('/').next())
                    .unwrap_or("");
                if let Some(t) = self.tiers.iter_mut().find(|t| t.kname == kname) {
                    if let Some(old) = t.replacing.take()
                        && let Some(m) = t.members.iter_mut().find(|m| m.part == old)
                    {
                        m.role = MemberRole::Faulty;
                    }
                    for m in t.members.iter_mut() {
                        if m.role == MemberRole::Rebuilding {
                            m.role = MemberRole::Active;
                        }
                    }
                    t.sync = None;
                    t.size = t.formula_size();
                }
            }
            PlanAction::WriteSysfs { .. } => {}
            PlanAction::Command { argv, .. } => self.apply_argv(argv),
        }
    }

    fn apply_argv(&mut self, argv: &[String]) {
        let a: Vec<&str> = argv.iter().map(String::as_str).collect();
        let part = |p: &str| p.trim_start_matches("/dev/").to_string();
        let extent = self.extent;
        match a.as_slice() {
            ["wipefs", "--all", dev] | ["sgdisk", "--zap-all", dev] => {
                let name = part(dev);
                if let Some(d) = self.disks.iter_mut().find(|d| d.name == name) {
                    d.parts.clear();
                }
            }
            ["sgdisk", rest @ ..] => {
                let Some(dev) = rest.last() else { return };
                let disk = part(dev);
                for n in rest.iter().filter_map(|x| x.strip_prefix("--delete=")) {
                    if let (Ok(n), Some(d)) = (n.parse::<u32>(), self.disks.iter_mut().find(|d| d.name == disk)) {
                        d.parts.retain(|p| p.partn != n);
                    }
                }
                for n in rest.iter().filter_map(|x| x.strip_prefix("--new=")) {
                    let f: Vec<u64> = n.split(':').filter_map(|x| x.parse().ok()).collect();
                    if let [num, s, e] = f[..] {
                        let d = self.disk_mut(&disk);
                        d.parts.retain(|p| p.tier != num as u32);
                        d.parts.push(VPart {
                            tier: num as u32,
                            kname: part_name(&disk, num as u32),
                            start: s * 512,
                            end: (e + 1) * 512,
                            fresh: true,
                            partn: num as u32,
                        });
                    }
                }
            }
            ["mdadm", "--create", dev, rest @ ..] => {
                let level = rest.iter().find_map(|x| x.strip_prefix("--level=")).and_then(|l| match l {
                    "5" => Some(RaidLevel::Raid5),
                    "6" => Some(RaidLevel::Raid6),
                    "1" => Some(RaidLevel::Raid1),
                    _ => None,
                });
                let bitmap = rest.contains(&"--bitmap=internal");
                let parts: Vec<String> = rest
                    .iter()
                    .filter(|x| x.starts_with("/dev/"))
                    .map(|x| part(x))
                    .collect();
                let index = tier_indices(&self.label, dev).first().copied().unwrap_or(0);
                let members: Vec<VMember> = parts
                    .iter()
                    .map(|p| VMember {
                        part: p.clone(),
                        disk: self.part_disk(p),
                        role: MemberRole::Active,
                    })
                    .collect();
                let (start, end) = self
                    .disks
                    .iter()
                    .flat_map(|d| d.parts.iter())
                    .find(|p| parts.first() == Some(&p.kname))
                    .map_or((0, 0), |p| (p.start, p.end));
                let pos = self.tiers.iter().position(|t| t.index == index);
                let t = match pos {
                    Some(i) => &mut self.tiers[i],
                    None => {
                        self.tiers.push(VTier {
                            index,
                            ..Default::default()
                        });
                        self.tiers.sort_by_key(|t| t.index);
                        let i = self.tiers.iter().position(|t| t.index == index).expect("pushed");
                        &mut self.tiers[i]
                    }
                };
                t.start = start;
                t.end = end;
                t.dev = dev.to_string();
                t.kname = format!("md{}", 100 + index);
                t.level = level;
                t.raid_disks = members.len() as u32;
                t.members = members;
                t.planned.clear();
                t.bitmap = bitmap;
                t.sync = Some("resync".into());
                t.size = t.formula_size();
            }
            ["mdadm", "--grow", "--continue", dev] => {
                if let Some(t) = self.tier_by_dev(dev) {
                    t.interrupted = false;
                    t.sync = Some("reshape".into());
                }
            }
            ["mdadm", "--grow", dev, rest @ ..] => {
                let Some(t) = self.tier_by_dev(dev) else { return };
                if rest.contains(&"--bitmap=none") {
                    t.bitmap = false;
                    return;
                }
                if rest.contains(&"--bitmap=internal") {
                    t.bitmap = true;
                    return;
                }
                let level = rest.iter().find_map(|x| x.strip_prefix("--level="));
                let n = rest
                    .iter()
                    .find_map(|x| x.strip_prefix("--raid-devices="))
                    .and_then(|x| x.parse::<u32>().ok());
                match (level, n) {
                    (Some("5"), None) => t.level = Some(RaidLevel::Raid5),
                    (Some("6"), Some(n)) => {
                        // RAID5 → RAID6 with the layout kept: the spare becomes the Q disk
                        t.level = Some(RaidLevel::Raid6);
                        t.raid_disks = n;
                        for m in t.members.iter_mut().filter(|m| m.role == MemberRole::Spare) {
                            m.role = MemberRole::Rebuilding;
                        }
                        t.sync = Some("recovery".into());
                    }
                    (None, Some(n)) if t.level == Some(RaidLevel::Raid1) => {
                        t.raid_disks = n;
                        let mut slots = n as usize - t.active().min(n as usize);
                        for m in t.members.iter_mut().filter(|m| m.role == MemberRole::Spare) {
                            if slots == 0 {
                                break;
                            }
                            m.role = MemberRole::Rebuilding;
                            slots -= 1;
                        }
                        if t.count(MemberRole::Rebuilding) > 0 {
                            t.sync = Some("recovery".into());
                        }
                        t.size = t.formula_size();
                    }
                    (None, Some(n)) => {
                        let mut slots = (n as usize).saturating_sub(t.active());
                        t.raid_disks = n;
                        for m in t.members.iter_mut().filter(|m| m.role == MemberRole::Spare) {
                            if slots == 0 {
                                break;
                            }
                            m.role = MemberRole::Active;
                            slots -= 1;
                        }
                        t.sync = Some("reshape".into());
                    }
                    _ => {}
                }
            }
            ["mdadm", "--zero-superblock", p] => {
                let p = part(p);
                // the partition stays (a fresh inventory still finds its disk by its label)
                for d in self.disks.iter_mut() {
                    for x in d.parts.iter_mut().filter(|x| x.kname == p) {
                        x.fresh = true;
                    }
                }
            }
            ["mdadm", dev, op, rest @ ..] => {
                let parts: Vec<String> = rest
                    .iter()
                    .filter(|x| x.starts_with("/dev/"))
                    .map(|x| part(x))
                    .collect();
                let disks: Vec<String> = parts.iter().map(|p| self.part_disk(p)).collect();
                let Some(t) = self.tier_by_dev(dev) else { return };
                match *op {
                    "--add" | "--add-spare" => {
                        for (p, d) in parts.iter().zip(disks) {
                            let rebuild = *op == "--add"
                                && (t.degraded() as usize) > t.count(MemberRole::Rebuilding);
                            t.members.push(VMember {
                                part: p.clone(),
                                disk: d,
                                role: if rebuild {
                                    MemberRole::Rebuilding
                                } else {
                                    MemberRole::Spare
                                },
                            });
                            if rebuild {
                                t.sync = Some("recovery".into());
                            }
                        }
                    }
                    "--fail" => {
                        for p in &parts {
                            if let Some(m) = t.members.iter_mut().find(|m| &m.part == p) {
                                m.role = MemberRole::Faulty;
                            }
                        }
                    }
                    "--remove" => {
                        t.members.retain(|m| !parts.contains(&m.part));
                        let index = t.index;
                        for dk in self.disks.iter_mut() {
                            for x in dk.parts.iter_mut() {
                                if x.tier == index && parts.contains(&x.kname) {
                                    x.fresh = false;
                                }
                            }
                        }
                    }
                    "--replace" => {
                        // mdadm DEV --replace OLD --with NEW
                        if let [old, new] = parts.as_slice() {
                            t.replacing = Some(old.clone());
                            if let Some(m) = t.members.iter_mut().find(|m| &m.part == new) {
                                m.role = MemberRole::Rebuilding;
                            }
                            t.sync = Some("recovery".into());
                        }
                    }
                    _ => {}
                }
            }
            ["pvcreate", rest @ ..] => {
                for dev in rest.iter().filter(|x| x.starts_with("/dev/")) {
                    if let Some(t) = self.tier_by_dev(dev) {
                        t.pv = Some(tier_pv_bytes(t.size, extent, t.level, t.raid_disks as usize));
                        t.in_vg = false;
                    }
                }
            }
            ["vgcreate", _vg, devs @ ..] | ["vgextend", _vg, devs @ ..] => {
                self.vg_exists = true;
                for dev in devs {
                    if let Some(t) = self.tier_by_dev(dev)
                        && !t.in_vg
                    {
                        t.in_vg = true;
                        let add = t.pv.unwrap_or(0);
                        self.vg_free += add;
                    }
                }
            }
            ["pvresize", dev] => {
                if let Some(t) = self.tier_by_dev(dev) {
                    let new = tier_pv_bytes(t.size, extent, t.level, t.raid_disks as usize);
                    let old = t.pv.unwrap_or(0);
                    t.pv = Some(new);
                    if t.in_vg {
                        self.vg_free += new.saturating_sub(old);
                    }
                }
            }
            ["lvcreate", ..] | ["lvextend", ..] => {
                self.lv_exists = true;
                self.lv_size += self.vg_free;
                self.vg_free = 0;
            }
            ["cryptsetup", "luksFormat", ..] => {
                self.luks = Some((String::new(), self.lv_size.saturating_sub(LUKS_HEADER)));
            }
            ["cryptsetup", "open", .., name] => {
                if let Some(l) = &mut self.luks {
                    l.0 = name.to_string();
                }
            }
            ["cryptsetup", "resize", ..] => {
                let size = self.lv_size.saturating_sub(LUKS_HEADER);
                if let Some(l) = &mut self.luks {
                    l.1 = size;
                }
            }
            ["mkfs.btrfs", ..] => {
                self.fs = Some((format!("/{}", self.label), self.fs_device_size()));
            }
            ["btrfs", "filesystem", "resize", "max", _] => {
                let size = self.fs_device_size();
                if let Some(f) = &mut self.fs {
                    f.1 = size;
                }
            }
            _ => {}
        }
    }
}

/// Tier numbers named in `text` as `/dev/md/<label>_t<N>`.
fn tier_indices(label: &str, text: &str) -> Vec<u32> {
    let pat = format!("/dev/md/{label}_t");
    text.match_indices(&pat)
        .filter_map(|(i, _)| {
            let rest = &text[i + pat.len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .collect()
}

// ------------------------------------------------------------------------------- steps

/// What a growth is for: disks joining, a disk replaced. Empty: only finish what the state
/// shows unfinished.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Goal {
    /// The replaced disk ([`MISSING`] for one gone from the degraded tiers).
    pub old: Option<String>,
    /// New disks (one for a replace).
    pub new: Vec<String>,
}

fn run(desc: D, argv: Vec<String>) -> PlanStep {
    let r: Vec<&str> = argv.iter().map(String::as_str).collect();
    cmd(desc, &r)
}

fn on(kind: PlanStepKind, subject: &str) -> D {
    d_on(kind, subject)
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn dev(p: &str) -> String {
    format!("/dev/{p}")
}

/// Partitions for the tiers each disk is in after the change: a new blank disk gets them
/// all in one table; a disk in use (or one partly done by an interrupted plan) gets each
/// missing one alone, announced to the kernel with `partx`.
///
/// A new disk with slices of this volume left over (a member once: replaced, or gone and
/// back, its partitions in no array; or partitioned by a growth that stopped before adding
/// it) is cleared like a blank one, the md superblocks of its partitions first (the same
/// partitions made again would carry them). A disk holding a
/// live member is never cleared.
fn partition_steps(v: &mut View, target: &[Tier], new: &[String]) -> Result<Vec<PlanStep>, String> {
    let mut steps = Vec::new();
    let mut disks: Vec<String> = target.iter().flat_map(|t| t.members.clone()).collect();
    disks.sort();
    disks.dedup();
    natural(&mut disks);
    for disk in disks {
        let Some(vd) = v.disks.iter().find(|d| d.name == disk).cloned() else {
            continue;
        };
        let wanted: Vec<&Tier> = target.iter().filter(|t| t.members.contains(&disk)).collect();
        let live = v.tiers.iter().any(|t| t.members.iter().any(|m| m.disk == disk));
        let leftover = new.contains(&disk) && !live && !vd.parts.is_empty();
        // slices left over next to partitions that are not the volume's: only the slices go,
        // the disk is never wiped or its table cleared whole
        let split = leftover && vd.others > 0;
        let blank = new.contains(&disk) && (vd.parts.is_empty() || leftover) && !split;
        if !leftover {
            for t in &wanted {
                if let Some(p) = vd.parts.iter().find(|p| p.tier == t.index)
                    && (p.start != t.start || p.end != t.end)
                {
                    return Err(format!(
                        "{}: its partition for tier {} is not where the tier is",
                        p.kname, t.index
                    ));
                }
            }
        }
        let missing: Vec<&Tier> = wanted
            .iter()
            .filter(|t| blank || split || !vd.parts.iter().any(|p| p.tier == t.index))
            .copied()
            .collect();
        if missing.is_empty() {
            continue;
        }
        let devp = dev(&disk);
        let spec = |t: &Tier| {
            [
                format!("--new={}:{}:{}", t.index, t.start / 512, t.end / 512 - 1),
                format!("--typecode={}:FD00", t.index),
                format!("--change-name={}:{}", t.index, tier_partlabel(&v.label, t.index)),
            ]
        };
        let mut batch = Vec::new();
        if blank {
            if leftover {
                for p in vd.parts.iter().filter(|p| !p.fresh) {
                    batch.push(destroying(
                        run(on(PlanStepKind::WipeSignatures, &p.kname), s(&["wipefs", "--all", &dev(&p.kname)])),
                        &[&disk],
                    ));
                }
            }
            batch.push(destroying(
                run(on(PlanStepKind::WipeSignatures, &disk), s(&["wipefs", "--all", &devp])),
                &[&disk],
            ));
            batch.push(destroying(
                run(on(PlanStepKind::ClearPartitionTable, &disk), s(&["sgdisk", "--zap-all", &devp])),
                &[&disk],
            ));
            let mut argv = vec!["sgdisk".to_string()];
            for t in &missing {
                argv.extend(spec(t));
            }
            argv.push(devp.clone());
            batch.push(run(
                D(
                    PlanStepKind::CreatePartition,
                    PlanArgs {
                        subject: Some(disk.clone()),
                        count: Some(missing.len() as u64),
                        ..Default::default()
                    },
                ),
                argv,
            ));
        } else {
            if split {
                let mut old: Vec<&VPart> = vd.parts.iter().collect();
                old.sort_by_key(|p| p.partn);
                for p in old.iter().filter(|p| !p.fresh) {
                    batch.push(destroying(
                        run(on(PlanStepKind::WipeSignatures, &p.kname), s(&["wipefs", "--all", &dev(&p.kname)])),
                        &[&disk],
                    ));
                }
                let mut argv = vec!["sgdisk".to_string()];
                argv.extend(old.iter().map(|p| format!("--delete={}", p.partn)));
                argv.push(devp.clone());
                batch.push(destroying(
                    run(
                        D(
                            PlanStepKind::DeletePartitions,
                            PlanArgs {
                                subject: Some(disk.clone()),
                                items: old.iter().map(|p| p.kname.clone()).collect(),
                                ..Default::default()
                            },
                        ),
                        argv,
                    ),
                    &[&disk],
                ));
                // the kernel forgets every deleted slice before any is made again where it
                // may overlap another (`--update` drops a partition no longer in the table,
                // and does nothing when udev already did)
                for p in &old {
                    batch.push(run(
                        on(PlanStepKind::RemovePartitionLive, &p.kname),
                        s(&["partx", "--update", "--nr", &p.partn.to_string(), &devp]),
                    ));
                }
            }
            for t in &missing {
                let p = part_name(&disk, t.index);
                let mut argv = vec!["sgdisk".to_string()];
                argv.extend(spec(t));
                argv.push(devp.clone());
                batch.push(run(
                    D(
                        PlanStepKind::CreatePartition,
                        PlanArgs {
                            subject: Some(disk.clone()),
                            other: Some(p.clone()),
                            ..Default::default()
                        },
                    ),
                    argv,
                ));
                batch.push(run(
                    on(PlanStepKind::AddPartitionLive, &p),
                    // --update adds the partition, or leaves it when udev already did (a
                    // recent udev updates the partitions of a busy disk itself after sgdisk)
                    s(&["partx", "--update", "--nr", &t.index.to_string(), &devp]),
                ));
            }
        }
        for st in batch {
            v.apply(&st);
            steps.push(st);
        }
    }
    if !steps.is_empty() {
        let st = run(d(PlanStepKind::WaitUdev), s(&["udevadm", "settle"]));
        steps.push(st);
    }
    Ok(steps)
}

/// The steps of a growth: the partitions `target` asks for (without a target: those a
/// planned tier still misses), then [`next`] until done. `v` ends in the predicted state.
pub fn growth_steps(v: &mut View, goal: &Goal, target: Option<&[Tier]>) -> Result<Vec<PlanStep>, String> {
    let mut steps;
    if let Some(target) = target {
        // the tiers to create, with their disks (a tier found planned, from an interrupted
        // growth, takes the target's disks)
        v.tiers.retain(|x| x.created() || !target.iter().any(|t| t.index == x.index));
        for t in target {
            if !v.tiers.iter().any(|x| x.index == t.index) {
                v.tiers.push(VTier {
                    index: t.index,
                    start: t.start,
                    end: t.end,
                    level: t.level,
                    raid_disks: t.members.len() as u32,
                    planned: t.members.clone(),
                    ..Default::default()
                });
            }
        }
        v.tiers.sort_by_key(|t| t.index);
        steps = partition_steps(v, target, &goal.new)?;
    } else {
        // the planned tiers again, with the disks of the change under way
        v.tiers.retain(VTier::created);
        v.discover_planned(&goal.new);
        let planned: Vec<Tier> = v
            .tiers
            .iter()
            .filter(|t| !t.created())
            .map(|t| Tier {
                index: t.index,
                start: t.start,
                end: t.end,
                members: t.planned.clone(),
                level: t.level,
            })
            .collect();
        steps = partition_steps(v, &planned, &[])?;
    }
    for _ in 0..1000 {
        let Some(batch) = next(v, goal) else {
            uncreated(v)?;
            return Ok(steps);
        };
        for st in batch {
            v.apply(&st);
            steps.push(st);
        }
    }
    Err("the growth plan does not converge (a bug): plan again from the current state".into())
}

/// A planned tier that no step creates: a partition of it carries an md superblock (of an
/// array that is not running: `mdadm --create` is never run over it), or is missing.
fn uncreated(v: &View) -> Result<(), String> {
    for t in v.tiers.iter().filter(|t| !t.created()) {
        for d in &t.planned {
            match v.slice_of(d, t) {
                None => {
                    return Err(format!(
                        "tier {}: {d} has no partition for it; plan again",
                        t.index
                    ));
                }
                Some(p) if !p.fresh => {
                    return Err(format!(
                        "{}: carries an md superblock of an array that is not running; tier {} is not created over it (clear it, or assemble that array, first)",
                        p.kname, t.index
                    ));
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}

/// The next step(s) the state of `v` calls for, `None` when nothing is left.
pub fn next(v: &View, g: &Goal) -> Option<Vec<PlanStep>> {
    let one = |st: PlanStep| Some(vec![st]);
    let label = &v.label;
    let vg = vg_name(label);
    let created = || v.tiers.iter().filter(|t| t.created());
    let replace_pending = |t: &VTier| -> bool {
        let (Some(old), Some(new)) = (g.old.as_deref(), g.new.first()) else {
            return false;
        };
        t.members.iter().any(|m| m.disk == old && m.role == MemberRole::Active)
            && t.degraded() == 0
            && v.slice_of(new, t).is_some()
    };
    let idle = |t: &VTier| t.sync.is_none() && !t.interrupted && t.replacing.is_none();
    let change_pending = |t: &VTier| {
        !v.candidates(t, g).is_empty() || t.count(MemberRole::Spare) > 0 || replace_pending(t)
    };

    // a write-intent bitmap back on as soon as its tier is done (also on a tier still short
    // of a disk: the bitmap makes the rebuild of a disk that comes back short)
    for t in created() {
        if !t.bitmap && idle(t) && !change_pending(t) {
            return one(run(
                on(PlanStepKind::BitmapOn, &t.kname),
                s(&["mdadm", "--grow", &t.dev, "--bitmap=internal"]),
            ));
        }
    }

    // a three-way mirror on its way to RAID6 that lost its third mirror to the plan (a crash
    // after `--fail` or `--remove`, before `--raid-devices=2`): the conversion goes on
    // ([`third_mirror_out`]); the joins then take the partition back
    for t in created() {
        match third_mirror_out(v, t, g) {
            Some(MirrorOut::Faulty(m)) => {
                return one(run(
                    on(PlanStepKind::RemoveMember, &m.part),
                    s(&["mdadm", &t.dev, "--remove", &dev(&m.part)]),
                ));
            }
            Some(MirrorOut::Slice(out)) => {
                let mut steps = Vec::new();
                if !out.fresh {
                    steps.push(run(
                        on(PlanStepKind::WipeSignatures, &out.kname),
                        s(&["mdadm", "--zero-superblock", &dev(&out.kname)]),
                    ));
                }
                steps.push(run(
                    on(PlanStepKind::Reshape, &t.kname),
                    s(&["mdadm", "--grow", &t.dev, "--raid-devices=2"]),
                ));
                return Some(steps);
            }
            None => {}
        }
    }
    // a mirror taken out on the way to RAID6 and not cleared (a crash in between): its disk
    // still works in every lower tier, so it is cleared and joins again
    for t in created().filter(|t| v.tolerance >= 2 && t.level == Some(RaidLevel::Raid1)) {
        let lower: Vec<&VTier> = created().filter(|x| x.index < t.index).collect();
        if lower.is_empty() {
            continue;
        }
        let reclaim = v.disks.iter().find_map(|dk| {
            let p = v.slice_of(&dk.name, t)?;
            (!p.fresh && t.member(&p.kname).is_none() && works_below(&lower, &dk.name) && g.old.as_deref() != Some(&dk.name))
                .then(|| p.kname.clone())
        });
        if let Some(p) = reclaim {
            return one(run(
                on(PlanStepKind::WipeSignatures, &p),
                s(&["mdadm", "--zero-superblock", &dev(&p)]),
            ));
        }
    }
    // rebuilds first: every degraded tier gets its new member before any wait (md queues
    // them), before a new tier is made or a tier grows
    for t in created().filter(|t| t.degraded() > 0 && t.replacing.is_none()) {
        if let Some(old) = g.old.as_deref() {
            let faulty: Vec<&VMember> = t
                .members
                .iter()
                .filter(|m| m.disk == old && m.role == MemberRole::Faulty)
                .collect();
            if let Some(m) = faulty.first() {
                return one(run(
                    on(PlanStepKind::RemoveMember, &m.part),
                    s(&["mdadm", &t.dev, "--remove", &dev(&m.part)]),
                ));
            }
        }
        let slots = (t.degraded() as usize).saturating_sub(t.count(MemberRole::Rebuilding));
        let cands: Vec<String> = v
            .candidates(t, g)
            .into_iter()
            .take(slots)
            .map(|p| dev(&p.kname))
            .collect();
        if !cands.is_empty() {
            let mut argv = s(&["mdadm", &t.dev, "--add"]);
            argv.extend(cands);
            return one(run(on(PlanStepKind::AddForRebuild, &t.kname), argv));
        }
    }
    // new tiers, once no rebuild runs or waits to start: the copies of the data that lost
    // its redundancy come before anything else reads and writes the same disks
    let rebuilding = created().any(|t| {
        t.degraded() > 0
            && (t.count(MemberRole::Rebuilding) > 0
                || t.sync.as_deref() == Some("recovery")
                || !v.candidates(t, g).is_empty())
    });
    for t in v.tiers.iter().filter(|t| !t.created() && !rebuilding) {
        let parts: Option<Vec<String>> = t
            .planned
            .iter()
            .map(|d| v.slice_of(d, t).filter(|p| p.fresh).map(|p| dev(&p.kname)))
            .collect();
        let (Some(parts), Some(level)) = (parts, t.level) else {
            continue;
        };
        let tdev = tier_dev(label, t.index);
        let mut argv = s(&[
            "mdadm",
            "--create",
            &tdev,
            "--run",
            &format!("--level={}", md_level_arg(level)),
            &format!("--raid-devices={}", parts.len()),
            "--metadata=1.2",
            &format!("--name={label}_t{}", t.index),
            "--bitmap=internal",
        ]);
        argv.extend(parts.iter().cloned());
        let names: Vec<&str> = parts.iter().map(|p| p.trim_start_matches("/dev/")).collect();
        return one(destroying(
            run(
                D(
                    PlanStepKind::CreateArray,
                    PlanArgs {
                        subject: Some(tdev.clone()),
                        level: Some(level),
                        count: Some(parts.len() as u64),
                        ..Default::default()
                    },
                ),
                argv,
            ),
            &names,
        ));
    }
    // recorded before it joins the group, so the next boot assembles it
    if v.records_known
        && let Some(t) = created().find(|t| !t.recorded)
    {
        return one(config(
                on(PlanStepKind::RecordArray, &t.dev),
                MDADM_CONF,
                &format!(
                    "ARRAY {} metadata=1.2 UUID=<from mdadm --detail --export {}>",
                    tier_dev(label, t.index),
                    t.dev
                ),
            ));
    }
    if let Some(t) = created().find(|t| t.pv.is_none() && (t.recorded || !v.records_known)) {
        return one(run(
            on(PlanStepKind::CreatePv, &t.dev),
            s(&["pvcreate", "--yes", "--force", &t.dev]),
        ));
    }
    if let Some(t) = created().find(|t| t.pv.is_some() && !t.in_vg) {
        return one(run(
            D(
                PlanStepKind::ExtendVg,
                PlanArgs {
                    subject: Some(vg.clone()),
                    other: Some(t.dev.clone()),
                    ..Default::default()
                },
            ),
            s(&["vgextend", &vg, &t.dev]),
        ));
    }
    let extent = v.extent.max(MIB);
    for t in created() {
        if let Some(pv) = t.pv
            && t.in_vg
            && t.sync.as_deref() != Some("reshape")
            && !t.interrupted
            && t.size.saturating_sub(pv) > 2 * extent
        {
            return one(run(on(PlanStepKind::ResizePv, &t.dev), s(&["pvresize", &t.dev])));
        }
    }
    if v.lv_exists && v.vg_free >= extent {
        let lv = format!("{vg}/{LV_NAME}");
        return one(run(
            on(PlanStepKind::ExtendLv, &lv),
            s(&["lvextend", "-l", "+100%FREE", &lv]),
        ));
    }
    if let Some((mapper, size)) = &v.luks
        && !mapper.is_empty()
        && v.lv_size.saturating_sub(*size) > LUKS_SLACK
    {
        return one(cmd_secret(
            d(PlanStepKind::GrowLuks),
            &["cryptsetup", "resize", "--key-file", "-", mapper],
            "passphrase",
        ));
    }
    if let Some((mnt, size)) = &v.fs
        && v.fs_device_size().saturating_sub(*size) > MIB
    {
        return one(run(
            on(PlanStepKind::GrowFilesystem, mnt),
            s(&["btrfs", "filesystem", "resize", "max", mnt]),
        ));
    }

    // per tier, lowest first
    for t in created() {
        let faulty: Vec<&VMember> = t
            .members
            .iter()
            .filter(|m| m.role == MemberRole::Faulty)
            .collect();
        if let Some(m) = faulty.first()
            && t.degraded() == 0
            && t.replacing.is_none()
        {
            return one(run(
                on(PlanStepKind::RemoveReplaced, &m.part),
                s(&["mdadm", &t.dev, "--remove", &dev(&m.part)]),
            ));
        }
        if t.interrupted {
            return one(run(
                on(PlanStepKind::Reshape, &t.kname),
                s(&["mdadm", "--grow", "--continue", &t.dev]),
            ));
        }
        match t.sync.as_deref() {
            Some("reshape") => {
                return one(wait(on(PlanStepKind::WaitReshape, &t.kname), &t.wait_condition()));
            }
            Some("recovery") => {
                let kind = if t.replacing.is_some() {
                    PlanStepKind::WaitCopy
                } else {
                    PlanStepKind::WaitRebuild
                };
                return one(wait(on(kind, &t.kname), &t.wait_condition()));
            }
            _ => {}
        }
        if replace_pending(t) {
            let old = g.old.as_deref().unwrap_or_default();
            let new = g.new.first().map(String::as_str).unwrap_or_default();
            let p = v.slice_of(new, t).expect("checked by replace_pending");
            let old_part = t
                .members
                .iter()
                .find(|m| m.disk == old && m.role == MemberRole::Active)
                .map(|m| m.part.clone())
                .unwrap_or_default();
            return match t.member(&p.kname).map(|m| m.role) {
                None if p.fresh => one(run(
                    on(PlanStepKind::AddSpare, &p.kname),
                    s(&["mdadm", &t.dev, "--add-spare", &dev(&p.kname)]),
                )),
                Some(MemberRole::Spare) if t.sync.is_none() => one(run(
                    on(PlanStepKind::ReplaceCopy, &old_part),
                    s(&["mdadm", &t.dev, "--replace", &dev(&old_part), "--with", &dev(&p.kname)]),
                )),
                Some(MemberRole::Spare) => one(wait(
                    on(PlanStepKind::WaitResync, &t.kname),
                    &t.wait_condition(),
                )),
                _ => continue,
            };
        }
        // joins
        let cands: Vec<&VPart> = v.candidates(t, g);
        let spares: Vec<&VMember> = t
            .members
            .iter()
            .filter(|m| m.role == MemberRole::Spare)
            .collect();
        if (cands.is_empty() && spares.is_empty()) || t.degraded() > 0 {
            continue;
        }
        if t.sync.is_some() {
            return one(wait(on(PlanStepKind::WaitResync, &t.kname), &t.wait_condition()));
        }
        if t.bitmap {
            return one(run(
                on(PlanStepKind::BitmapOff, &t.kname),
                s(&["mdadm", "--grow", &t.dev, "--bitmap=none"]),
            ));
        }
        let level = t.level?;
        if level == RaidLevel::Raid1 && t.raid_disks == 3 && t.active() == 3 {
            // a three-way mirror on its way to RAID6: take one mirror out (cleared, so it
            // joins again as a fresh partition), back to a 2-disk mirror
            let mut ms: Vec<&VMember> = t.members.iter().filter(|m| m.role == MemberRole::Active).collect();
            ms.sort_by(|a, b| a.disk.len().cmp(&b.disk.len()).then_with(|| a.disk.cmp(&b.disk)));
            let out = ms.last()?.part.clone();
            return Some(vec![
                run(on(PlanStepKind::MarkFailed, &out), s(&["mdadm", &t.dev, "--fail", &dev(&out)])),
                run(on(PlanStepKind::RemoveMember, &out), s(&["mdadm", &t.dev, "--remove", &dev(&out)])),
                run(on(PlanStepKind::WipeSignatures, &out), s(&["mdadm", "--zero-superblock", &dev(&out)])),
                run(on(PlanStepKind::Reshape, &t.kname), s(&["mdadm", "--grow", &t.dev, "--raid-devices=2"])),
            ]);
        }
        if level == RaidLevel::Raid1 && t.raid_disks == 2 {
            return one(run(
                on(PlanStepKind::MirrorToRaid5, &t.kname),
                s(&["mdadm", "--grow", &t.dev, "--level=5"]),
            ));
        }
        if !cands.is_empty() {
            let mut argv = s(&["mdadm", &t.dev, "--add"]);
            argv.extend(cands.iter().map(|p| dev(&p.kname)));
            return one(run(on(PlanStepKind::AddSpares, &t.kname), argv));
        }
        let total = t.active() + spares.len();
        let step = match level {
            RaidLevel::Raid5 if v.tolerance >= 2 && total >= 4 && t.active() + 1 < total => run(
                on(PlanStepKind::Reshape, &t.kname),
                s(&["mdadm", "--grow", &t.dev, &format!("--raid-devices={}", total - 1)]),
            ),
            // the layout is kept (Q on the new disk, not rotated): the conversion is a
            // recovery onto that disk; normalising it now would be a second full reshape of
            // the tier, so it waits for the next reshape of the tier (below)
            RaidLevel::Raid5 if v.tolerance >= 2 && total >= 4 => run(
                on(PlanStepKind::Reshape, &t.kname),
                s(&[
                    "mdadm",
                    "--grow",
                    &t.dev,
                    "--level=6",
                    &format!("--raid-devices={total}"),
                    "--layout=preserve",
                ]),
            ),
            RaidLevel::Raid5 => run(
                on(PlanStepKind::Reshape, &t.kname),
                s(&["mdadm", "--grow", &t.dev, &format!("--raid-devices={total}")]),
            ),
            // a RAID6 made from RAID5 keeps the layout of the conversion, which mdadm does
            // not reshape without being told: this reshape restripes the tier anyway, so it
            // takes the standard layout on the way (nothing changes for one that has it)
            RaidLevel::Raid6 => run(
                on(PlanStepKind::Reshape, &t.kname),
                s(&["mdadm", "--grow", &t.dev, &format!("--raid-devices={total}"), "--layout=normalise"]),
            ),
            _ => continue,
        };
        return one(step);
    }
    // last, the replaced disk when it is still attached and out of every tier: its old tier
    // partitions lose their superblocks (otherwise assembling from mdadm.conf, as the
    // initramfs does, finds an outdated copy of a tier and gives up on the whole array),
    // then its partition table goes (a slice of the volume that is not a member would be
    // taken for a new disk's partition waiting to join): the disk is free again. A disk
    // with other partitions keeps them: only the volume's are deleted. The steps erase the
    // disk the request named (the executor checks it is still that disk and that what it
    // erases is still the volume's and unused) and are best effort: the volume no longer
    // needs the disk, which may be unreadable. Nothing else is ever erased here: for a
    // "missing" disk, a disk with slices of the volume left over is not one the request
    // named (the plan says so, [`PlanWarningCode::FlexLeftoverDisk`]).
    let clear: Option<&VDisk> = match g.old.as_deref() {
        None | Some(MISSING) => None,
        Some(old) => v
            .disks
            .iter()
            .find(|d| d.name == old && !d.parts.is_empty() && !v.in_some_tier(old)),
    };
    if let Some(dk) = clear {
        let name = dk.name.as_str();
        let mut steps: Vec<PlanStep> = dk
            .parts
            .iter()
            .filter(|p| !p.fresh)
            .map(|p| {
                run(
                    on(PlanStepKind::WipeSignatures, &p.kname),
                    s(&["mdadm", "--zero-superblock", &dev(&p.kname)]),
                )
            })
            .collect();
        if dk.others == 0 {
            steps.push(run(
                on(PlanStepKind::ClearPartitionTable, name),
                s(&["sgdisk", "--zap-all", &dev(name)]),
            ));
        } else {
            let mut parts: Vec<&VPart> = dk.parts.iter().collect();
            parts.sort_by_key(|p| p.partn);
            let mut argv = vec!["sgdisk".to_string()];
            argv.extend(parts.iter().map(|p| format!("--delete={}", p.partn)));
            argv.push(dev(name));
            steps.push(run(
                D(
                    PlanStepKind::DeletePartitions,
                    PlanArgs {
                        subject: Some(name.to_string()),
                        items: parts.iter().map(|p| p.kname.clone()).collect(),
                        ..Default::default()
                    },
                ),
                argv,
            ));
        }
        for st in &mut steps {
            st.destroys = vec![name.to_string()];
            st.best_effort = true;
        }
        return Some(steps);
    }
    None
}

/// The disk is an in-sync member of every tier in `lower`.
fn works_below(lower: &[&VTier], disk: &str) -> bool {
    lower.iter().all(|x| {
        x.members
            .iter()
            .any(|m| m.disk == disk && m.role == MemberRole::Active)
    })
}

/// The third mirror a three-way mirror lost on its way to RAID6 ([`third_mirror_out`]).
#[derive(Debug, Clone, Copy)]
enum MirrorOut<'a> {
    /// Marked faulty, still in the array (a crash between `--fail` and `--remove`).
    Faulty(&'a VMember),
    /// Out of the array (a crash after `--remove`, before `--raid-devices=2`).
    Slice(&'a VPart),
}

/// A three-way mirror of a 2-disk tolerance volume on its way to RAID6 (one mirror taken out,
/// back to two disks, RAID5, then RAID6 over the new disks: md has no direct path) stopped
/// after its third mirror went out: the mirror's partition, on a disk that still works in
/// every lower tier, is faulty in the array or out of it. With it and the new partitions
/// the tier has the four disks of RAID6, which only the conversion gives it, so the
/// conversion goes on instead of rebuilding the mirror; a third mirror lost otherwise (no
/// four disks: no new partition around) is left to a replace. The same holds when a
/// "missing" disk is replaced: its slot is taken by the conversion (the replace plan shows
/// the tier as RAID6 and warns about the window).
fn third_mirror_out<'a>(v: &'a View, t: &'a VTier, g: &Goal) -> Option<MirrorOut<'a>> {
    if v.tolerance < 2
        || t.level != Some(RaidLevel::Raid1)
        || t.raid_disks != 3
        || t.active() != 2
        || t.sync.is_some()
        || t.interrupted
        || t.replacing.is_some()
    {
        return None;
    }
    let lower: Vec<&VTier> = v.tiers.iter().filter(|x| x.created() && x.index < t.index).collect();
    let usable = |disk: &str| works_below(&lower, disk) && g.old.as_deref() != Some(disk);
    let enough = |out: &str| {
        let others = v.candidates(t, g).iter().filter(|p| p.kname != out).count();
        t.active() + 1 + others >= 4
    };
    let faulty: Vec<&VMember> = t.members.iter().filter(|m| m.role == MemberRole::Faulty).collect();
    match (t.members.len(), faulty.as_slice()) {
        (3, [m]) if usable(&m.disk) && v.slice_of(&m.disk, t).is_some() && enough(&m.part) => {
            Some(MirrorOut::Faulty(m))
        }
        (2, []) => v.disks.iter().find_map(|dk| {
            let p = v.slice_of(&dk.name, t)?;
            (t.member(&p.kname).is_none() && usable(&dk.name) && enough(&p.kname)).then_some(MirrorOut::Slice(p))
        }),
        _ => None,
    }
}

// ------------------------------------------------------------------------------- create

/// The plan of a new flexible volume (`p` holds what the classic planner checked: label,
/// disks, count).
pub fn plan_create(
    inv: &Inventory,
    req: &VolumePlanRequest,
    mut p: StoragePlan,
    chosen: &[DiskSummary],
) -> StoragePlan {
    let label = req.label.as_str();
    let tolerance = req.level.tolerance();
    let sizes: Vec<DiskSize> = chosen.iter().map(|d| (d.name.clone(), d.size)).collect();
    p.raw_bytes = sizes.iter().map(|(_, s)| s).sum();
    p.fault_tolerance = tolerance;
    let layout = match tiers::plan(&sizes, tolerance, inv.min_tier()) {
        Ok(l) => l,
        Err(e) => {
            if p.errors.is_empty() {
                fail(&mut p, PlanIssueCode::DiskCount, "disks", None, format!("{e:?}"));
            }
            return finish(p);
        }
    };
    let created: Vec<Tier> = layout.created().cloned().collect();
    if created.is_empty() {
        // too few disks reach the thinnest tier together: name the ones under it
        let min = inv.min_tier();
        let need = tiers::min_members(tolerance);
        for (name, size) in sizes.iter().filter(|(_, s)| tiers::reach(*s) - PART_START < min) {
            let m = format!(
                "{name}: too small for a flexible RAID tier ({:.1} GB; {need} disks of at least {:.1} GB are needed)",
                *size as f64 / 1e9,
                (min + PART_START + END_RESERVE) as f64 / 1e9
            );
            fail(&mut p, PlanIssueCode::DiskTooSmall, "disks", Some(name), m);
        }
        if p.errors.is_empty() {
            let m = format!("no tier can be made: {need} disks must reach the same size");
            fail(&mut p, PlanIssueCode::DiskCount, "disks", None, m);
        }
        return finish(p);
    }
    let cap = rynas_api_types::capacity::flexible(&layout, &sizes, req.encrypt);
    p.wasted_bytes = cap.wasted;
    p.usable_bytes = cap.usable;
    p.tiers_after = layout.tiers.iter().map(Tier::to_api).collect();
    mix_warnings(&mut p, chosen, true);
    unused_warning(&mut p, &layout.unused);
    if req.encrypt {
        warn(&mut p, PlanWarningCode::Encrypted, PlanArgs::default());
    }
    if !req.compress {
        warn(&mut p, PlanWarningCode::CompressionOff, PlanArgs::default());
    }
    if !p.errors.is_empty() {
        return finish(p);
    }

    let mut v = View::blank(label, tolerance, &sizes);
    let new: Vec<String> = sizes.iter().map(|(n, _)| n.clone()).collect();
    let mut steps = match partition_steps(&mut v, &created, &new) {
        Ok(s) => s,
        Err(e) => {
            fail(&mut p, PlanIssueCode::FlexLayoutUnsupported, "disks", None, e);
            return finish(p);
        }
    };
    let mut devs = Vec::new();
    for t in &created {
        let level = t.level.expect("created tiers have a level");
        let tdev = tier_dev(label, t.index);
        let parts: Vec<String> = t.members.iter().map(|m| dev(&part_name(m, t.index))).collect();
        let mut argv = s(&[
            "mdadm",
            "--create",
            &tdev,
            "--run",
            &format!("--level={}", md_level_arg(level)),
            &format!("--raid-devices={}", parts.len()),
            "--metadata=1.2",
            &format!("--name={label}_t{}", t.index),
            "--bitmap=internal",
        ]);
        argv.extend(parts.iter().cloned());
        let names: Vec<&str> = parts.iter().map(|x| x.trim_start_matches("/dev/")).collect();
        steps.push(destroying(
            run(
                D(
                    PlanStepKind::CreateArray,
                    PlanArgs {
                        subject: Some(tdev.clone()),
                        level: Some(level),
                        count: Some(parts.len() as u64),
                        ..Default::default()
                    },
                ),
                argv,
            ),
            &names,
        ));
        devs.push(tdev);
    }
    steps.push(config(
        d(PlanStepKind::RecordArray),
        MDADM_CONF,
        &devs
            .iter()
            .map(|t| format!("ARRAY {t} metadata=1.2 UUID=<from mdadm --detail --export {t}>"))
            .collect::<Vec<_>>()
            .join("; "),
    ));
    for t in &devs {
        steps.push(run(on(PlanStepKind::CreatePv, t), s(&["pvcreate", "--yes", "--force", t])));
    }
    let vg = vg_name(label);
    let mut argv = s(&["vgcreate", &vg]);
    argv.extend(devs.iter().cloned());
    steps.push(run(on(PlanStepKind::CreateVg, &vg), argv));
    let lv = format!("{vg}/{LV_NAME}");
    steps.push(run(
        on(PlanStepKind::CreateLv, &lv),
        s(&["lvcreate", "--yes", "-l", "100%FREE", "-n", LV_NAME, &vg]),
    ));
    let base = lv_path(label);
    let ssd = chosen.iter().any(|d| d.kind != rynas_api_types::storage::DiskKind::Hdd);
    let fs_dev = volume_top(&mut p, &mut steps, label, &base, req.encrypt, req.compress, ssd);
    p.summary = vec![
        ("level".into(), level_name(req.level).into()),
        ("vg".into(), vg),
        // the device under LUKS (encrypted) or the file system
        ("lv".into(), base),
        ("filesystem_device".into(), fs_dev),
        ("mountpoint".into(), format!("/{label}")),
    ];
    if req.encrypt {
        p.summary.push(("mapper".into(), format!("rynas-{label}")));
    }
    p.steps = steps;
    finish(p)
}

fn unused_warning(p: &mut StoragePlan, unused: &[(String, u64)]) {
    if unused.is_empty() {
        return;
    }
    let args = PlanArgs {
        items: unused.iter().map(|(d, _)| d.clone()).collect(),
        size_bytes: Some(unused.iter().map(|(_, b)| b).sum()),
        ..Default::default()
    };
    warn(p, PlanWarningCode::FlexUnusedSpace, args);
}

// ------------------------------------------------------------------------------- growth

/// The volume's view, or the reason it cannot be changed (recorded on `p`).
fn load(inv: &Inventory, label: &str, p: &mut StoragePlan) -> Option<View> {
    use PlanIssueCode as C;
    if !valid_label(label) {
        fail(p, C::InvalidLabel, "volume", None, format!("{label:?}: not a volume name"));
        return None;
    }
    let vg = vg_name(label);
    let Some(layout) = inv.flex.iter().find(|l| l.vg == vg) else {
        let m = format!("{label}: no flexible RAID volume of that name");
        fail(p, C::NoSuchArray, "volume", Some(label), m);
        return None;
    };
    if !layout.managed {
        let m = format!("{label}: made outside rynas, shown read-only");
        fail(p, C::FlexLayoutUnsupported, "volume", Some(label), m);
        return None;
    }
    if !layout.blockers.is_empty() {
        let m = format!("{label}: {}", layout.blockers.join("; "));
        fail(p, C::FlexLayoutUnsupported, "volume", Some(label), m);
        return None;
    }
    if layout.stale {
        let m = format!("{label}: the LVM state could not be read (a disk is asleep or LVM did not answer); try again");
        fail(p, C::FlexLayoutUnsupported, "volume", Some(label), m);
        return None;
    }
    if inv.records.is_some() && inv.record(label).is_none() {
        let m = format!("{label}: not recorded by rynas, shown read-only");
        fail(p, C::FlexLayoutUnsupported, "volume", Some(label), m);
        return None;
    }
    p.tiers_before = layout.tiers.clone();
    Some(View::from_inventory(inv, layout, label))
}

/// The pending items of the volume's layout that hold a growth or replace back. A change
/// recorded as under way is not one of them: going on with it is the request's business
/// ([`recorded_change`]), and one that never started is ignored.
fn pending_of(inv: &Inventory, label: &str) -> Vec<rynas_api_types::storage::FlexPending> {
    let vg = vg_name(label);
    inv.flex
        .iter()
        .find(|l| l.vg == vg)
        .map_or_else(Vec::new, |l| {
            l.pending
                .iter()
                .filter(|p| p.kind != rynas_api_types::storage::FlexPendingKind::ChangeInterrupted)
                .cloned()
                .collect()
        })
}

/// Days since the epoch of a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Seconds since the epoch of a btrfs-progs date ("Thu Oct 22 15:35:01 2026").
pub fn scrub_epoch(text: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let w: Vec<&str> = text.split_whitespace().collect();
    let mi = w.iter().position(|x| MONTHS.contains(x))?;
    let month = MONTHS.iter().position(|m| *m == w[mi])? as u32 + 1;
    let day: u32 = w.get(mi + 1)?.parse().ok()?;
    let hms: Vec<u64> = w.get(mi + 2)?.split(':').filter_map(|x| x.parse().ok()).collect();
    let year: i64 = w.iter().skip(mi + 3).find_map(|x| x.parse().ok())?;
    let [h, m, sec] = hms[..] else { return None };
    let days = days_from_civil(year, month, day);
    (days >= 0).then(|| days as u64 * 86_400 + h * 3600 + m * 60 + sec)
}

/// The last scrub of the volume's filesystem: refused when it found errors it could not
/// correct (`strict`), a warning when none finished cleanly in [`SCRUB_MAX_AGE_DAYS`].
fn scrub_check(p: &mut StoragePlan, inv: &Inventory, v: &View, now_s: u64, strict: bool) {
    let Some((mnt, _)) = &v.fs else { return };
    let scrub = inv
        .filesystems
        .iter()
        .find(|f| f.mountpoint.as_deref() == Some(mnt.as_str()))
        .and_then(|f| f.scrub.clone());
    if let Some(sc) = &scrub
        && strict
        && (sc.uncorrectable.unwrap_or(0) > 0 || (sc.errors > 0 && sc.corrected.is_none()))
    {
        let m = format!("{mnt}: the last scrub found errors it could not correct ({}); restore the damaged files first", sc.error_summary);
        fail(p, PlanIssueCode::ScrubErrors, "volume", Some(mnt), m);
        return;
    }
    let recent = scrub.as_ref().is_some_and(|sc| {
        sc.status == ScrubState::Finished
            && sc.uncorrectable.unwrap_or(0) == 0
            && sc
                .started_ms
                .map(|ms| ms / 1000)
                .or_else(|| sc.started.as_deref().and_then(scrub_epoch))
                .is_some_and(|t| now_s.saturating_sub(t) <= SCRUB_MAX_AGE_DAYS * 86_400)
    });
    if !recent {
        let args = PlanArgs {
            subject: Some(mnt.clone()),
            count: Some(SCRUB_MAX_AGE_DAYS),
            ..Default::default()
        };
        warn(p, PlanWarningCode::FlexNoRecentScrub, args);
    }
}

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Warnings about the reshapes of a growth: their length, and the tiers that tolerate one
/// disk less while they become RAID6.
fn reshape_warnings(p: &mut StoragePlan, g: &tiers::Growth) {
    let mut bytes = 0u64;
    let mut secs = 0u64;
    let mut joined: Vec<u32> = g.joins.iter().map(|(_, t)| *t).collect();
    joined.sort_unstable();
    joined.dedup();
    for i in &joined {
        if let Some(t) = g.tiers.iter().find(|t| t.index == *i) {
            let passes = if g.level_changes.iter().any(|c| c.index == *i && c.reduces_tolerance()) {
                2
            } else {
                1
            };
            bytes += t.usable() * passes;
            secs += t.height() / RESHAPE_SPEED * passes;
        }
    }
    if bytes > 0 {
        let args = PlanArgs {
            size_bytes: Some(bytes),
            count: Some(secs.div_ceil(3600)),
            ..Default::default()
        };
        warn(p, PlanWarningCode::FlexLongReshape, args);
    }
    for c in g.level_changes.iter().filter(|c| c.reduces_tolerance()) {
        warn_on(p, PlanWarningCode::FlexReducedToleranceWindow, &c.index.to_string());
    }
}

/// Checks shared by grow and replace: tiers healthy (`allow_degraded`: a replace of a failed
/// disk) and idle (rebuilds allowed with `allow_degraded`).
fn tiers_ready(p: &mut StoragePlan, v: &View, allow_degraded: bool) {
    for t in v.tiers.iter().filter(|t| t.created()) {
        if t.degraded() > 0 && !allow_degraded {
            let m = format!("{} (tier {}) is not healthy: repair it first", t.kname, t.index);
            fail(p, PlanIssueCode::ArrayNotHealthy, "volume", Some(&t.kname), m);
        }
        let busy = match t.sync.as_deref() {
            None => t.interrupted,
            Some("recovery") => !allow_degraded,
            Some(_) => true,
        };
        if busy {
            let m = format!(
                "{} (tier {}) is busy ({}); wait for it to finish",
                t.kname,
                t.index,
                t.sync.as_deref().unwrap_or("interrupted reshape")
            );
            fail(p, PlanIssueCode::ArrayBusy, "volume", Some(&t.kname), m);
        }
    }
}

/// A warning for each new disk that misses a tier (and so joins only the tiers below it):
/// how much it misses. Disks too small for the lowest tier are errors instead.
fn short_warnings(p: &mut StoragePlan, g: &tiers::Growth) {
    for (disk, tier, missing) in g.short.iter().filter(|(d, _, _)| !g.too_small.contains(d)) {
        let args = PlanArgs {
            subject: Some(disk.clone()),
            size_bytes: Some(*missing),
            count: Some(u64::from(*tier)),
            ..Default::default()
        };
        warn(p, PlanWarningCode::DiskShortOfTier, args);
    }
}

/// The error of a new disk too small for the lowest tier: how much it misses.
fn too_small_text(g: &tiers::Growth, name: &str) -> String {
    let missing = g.short.iter().find(|(d, _, _)| d == name).map_or(0, |(_, _, b)| *b);
    format!("{name}: too small for the lowest tier ({:.1} GB missing)", missing as f64 / 1e9)
}

/// The error of a replacement disk too small for the tiers of the disk it replaces (`need`:
/// the smallest disk that holds them).
fn replace_too_small_text(disk: &str, size: u64, need: u64) -> String {
    format!(
        "{disk}: too small ({:.1} GB missing), the tiers it replaces need a disk of {:.1} GB",
        need.saturating_sub(size) as f64 / 1e9,
        need as f64 / 1e9
    )
}

/// Fills the capacity fields and the steps of a growth plan.
fn finish_growth(
    mut p: StoragePlan,
    mut v: View,
    goal: &Goal,
    g: &tiers::Growth,
    label: &str,
) -> StoragePlan {
    p.fault_tolerance = v.tolerance;
    unused_warning(&mut p, &g.unused);
    reshape_warnings(&mut p, g);
    if v.fs.is_none() {
        warn(&mut p, PlanWarningCode::GrowFilesystemByHand, PlanArgs::default());
    }
    let mut after = g.tiers.clone();
    if p.errors.is_empty() {
        match growth_steps(&mut v, goal, Some(&g.tiers)) {
            Ok(steps) => {
                p.steps = steps;
                as_planned(&mut p, &v, &mut after);
            }
            Err(e) => fail(&mut p, PlanIssueCode::FlexLayoutUnsupported, "volume", Some(label), e),
        }
    }
    // the LUKS header stays at the start of the logical volume, as when it was created
    p.usable_bytes = lvm_usable(&after).saturating_sub(if v.luks.is_some() { LUKS_HEADER } else { 0 });
    short_warnings(&mut p, g);
    p.tiers_after = after.iter().map(Tier::to_api).collect();
    for (after, before) in p.tiers_after.iter_mut().zip(&p.tiers_before) {
        if after.index == before.index {
            after.array = before.array.clone();
        }
    }
    if !p.errors.is_empty() {
        return finish(p);
    }
    p.summary = summary(&v, label);
    finish(p)
}

/// The tiers after the plan as its steps leave them (`v`, the end state of the model), where
/// that differs from the calculator's target: a three-way mirror that lost its third mirror
/// becomes RAID6 when the disks are there ([`third_mirror_out`]), with the warning about the
/// window it tolerates one disk.
fn as_planned(p: &mut StoragePlan, v: &View, after: &mut [Tier]) {
    for a in after.iter_mut() {
        let Some(t) = v.tiers.iter().find(|t| t.index == a.index && t.created()) else {
            continue;
        };
        let mut members: Vec<String> = t
            .members
            .iter()
            .filter(|m| m.role != MemberRole::Faulty)
            .map(|m| m.disk.clone())
            .collect();
        members.sort();
        members.dedup();
        natural(&mut members);
        let mut want = a.members.clone();
        natural(&mut want);
        if (&members, t.level) == (&want, a.level) || t.level.is_none() {
            continue;
        }
        let reduces = a.level == Some(RaidLevel::Raid1) && t.level == Some(RaidLevel::Raid6);
        a.members = members;
        a.level = t.level;
        let warned = p.warning_items.iter().any(|w| {
            w.code == PlanWarningCode::FlexReducedToleranceWindow
                && w.args.subject.as_deref() == Some(a.index.to_string().as_str())
        });
        if reduces && !warned {
            warn_on(p, PlanWarningCode::FlexReducedToleranceWindow, &a.index.to_string());
        }
    }
}


fn summary(v: &View, label: &str) -> Vec<(String, String)> {
    let mut s = vec![
        ("volume".to_string(), label.to_string()),
        ("vg".to_string(), vg_name(label)),
    ];
    if let Some((m, _)) = &v.fs {
        s.push(("mountpoint".into(), m.clone()));
    }
    s
}

/// `POST /api/v1/storage/plan/grow` with `volume`: the disks join the tiers they cover.
pub fn plan_grow(inv: &Inventory, req: &GrowPlanRequest) -> StoragePlan {
    plan_grow_at(inv, req, now_s())
}

pub fn plan_grow_at(inv: &Inventory, req: &GrowPlanRequest, now: u64) -> StoragePlan {
    use PlanIssueCode as C;
    let mut p = StoragePlan {
        operation: "grow_array".into(),
        ..Default::default()
    };
    let label = req.volume.clone().unwrap_or_default();
    let Some(mut v) = load(inv, &label, &mut p) else {
        return finish(p);
    };
    if let Some(l) = req.level
        && Some(l) != RaidLevel::flexible(v.tolerance)
    {
        let m = format!("the fault tolerance of a flexible RAID cannot be changed ({} stays)", v.tolerance);
        fail(&mut p, C::ToleranceChangeUnsupported, "level", None, m);
    }
    // the same growth requested again goes on from where it stopped
    let change = recorded_change(inv, &label);
    let resume = change
        .as_ref()
        .is_some_and(|c| c.old.is_none() && same_names(&c.new, &req.disks));
    if let Some(c) = change.as_ref().filter(|_| !resume) {
        let m = format!("{label}: {} stopped half way: request it again or complete it", c.describe());
        fail(&mut p, C::FlexGrowthPending, "volume", Some(&label), m);
    } else if !resume && !pending_of(inv, &label).is_empty() {
        let m = format!("{label}: a growth was interrupted; complete it first");
        fail(&mut p, C::FlexGrowthPending, "volume", Some(&label), m);
    }
    if !resume {
        tiers_ready(&mut p, &v, false);
    }
    if req.disks.is_empty() {
        fail(&mut p, C::NoDisks, "disks", None, "select at least one disk to add".into());
    }
    let mut seen = std::collections::HashSet::new();
    let mut new: Vec<DiskSize> = Vec::new();
    for name in &req.disks {
        if !seen.insert(name) {
            fail(&mut p, C::DiskSelectedTwice, "disks", Some(name), format!("{name}: selected twice"));
            continue;
        }
        // a disk the growth already put in a tier is the volume's, not a free disk
        let d = if resume && v.in_some_tier(name) {
            inv.disks.iter().find(|d| &d.name == name).cloned()
        } else {
            check_new_disk(inv, name, "disks", &mut p)
        };
        if let Some(d) = d {
            new.push((d.name.clone(), d.size));
            v.add_disk(inv, name);
        }
    }
    p.raw_bytes = new.iter().map(|(_, s)| s).sum();
    let goal = Goal {
        old: None,
        new: new.iter().map(|(n, _)| n.clone()).collect(),
    };
    let g = if resume {
        let mut disks = v.layout_disks();
        disks.retain(|(n, _)| !goal.new.contains(n));
        tiers::grow(&resume_shapes(&v, &goal), &disks, &new, v.tolerance, inv.min_tier())
    } else {
        tiers::grow(&v.shapes(), &v.layout_disks(), &new, v.tolerance, inv.min_tier())
    };
    for name in &g.too_small {
        let m = too_small_text(&g, name);
        fail(&mut p, C::DiskTooSmall, "disks", Some(name), m);
    }
    if p.errors.is_empty() {
        scrub_check(&mut p, inv, &v, now, !resume);
    }
    finish_growth(p, v, &goal, &g, &label)
}

/// `POST /api/v1/storage/plan/replace` with `volume`: the new disk takes the old one's place
/// in every tier (a copy while the old disk works, a rebuild when it failed or vanished),
/// then joins the tiers it reaches beyond.
pub fn plan_replace(inv: &Inventory, req: &ReplacePlanRequest) -> StoragePlan {
    plan_replace_at(inv, req, now_s())
}

pub fn plan_replace_at(inv: &Inventory, req: &ReplacePlanRequest, now: u64) -> StoragePlan {
    use PlanIssueCode as C;
    let mut p = StoragePlan {
        operation: "replace_disk".into(),
        ..Default::default()
    };
    let label = req.volume.clone().unwrap_or_default();
    let Some(mut v) = load(inv, &label, &mut p) else {
        return finish(p);
    };
    let old = req.old.trim_start_matches("/dev/").to_string();
    let new_name = req.new_disk.clone();
    // the same replace requested again goes on from where it stopped: the new disk may hold
    // members already, the old one may be out of some tiers (or all: what is left is the new
    // disk's joins and clearing the old one)
    let change = recorded_change(inv, &label);
    let resume = change.as_ref().is_some_and(|c| {
        c.old.as_deref() == Some(old.as_str()) && c.new.len() == 1 && c.new[0] == new_name
    });
    if let Some(c) = change.as_ref().filter(|_| !resume) {
        let m = format!("{label}: {} stopped half way: request it again or complete it", c.describe());
        fail(&mut p, C::FlexGrowthPending, "volume", Some(&label), m);
        return finish(p);
    }
    let goal = Goal {
        old: Some(old.clone()),
        new: vec![new_name.clone()],
    };
    let mut shapes = if resume { resume_shapes(&v, &goal) } else { v.shapes() };
    let mut disks = v.layout_disks();
    if resume {
        disks.retain(|(n, _)| n != &new_name);
    }
    // the disk to replace, by name; "missing" stands for the disk gone from degraded tiers
    let in_tiers = v.tiers.iter().any(|t| t.members.iter().any(|m| m.disk == old));
    let old_name: Option<String> = if old == MISSING {
        let degraded: Vec<u32> = v
            .tiers
            .iter()
            .filter(|t| t.created() && t.degraded() > 0)
            .map(|t| t.index)
            .collect();
        if degraded.is_empty() {
            if !resume {
                fail(&mut p, C::NotAMember, "old", Some(&old), format!("{label}: no tier misses a disk"));
                return finish(p);
            }
            None
        } else {
            let ghost = "<missing>".to_string();
            let mut top = PART_START;
            for t in shapes.iter_mut().filter(|t| degraded.contains(&t.index)) {
                // a new disk rebuilding there takes the ghost's place, not one of its own
                t.members.retain(|m| m != &new_name);
                t.members.push(ghost.clone());
                top = top.max(t.end);
            }
            disks.push((ghost.clone(), top + END_RESERVE));
            Some(ghost)
        }
    } else if in_tiers {
        Some(old.clone())
    } else if resume {
        None
    } else {
        let m = format!("{old} is not a disk of {label}");
        fail(&mut p, C::NotAMember, "old", Some(&old), m);
        return finish(p);
    };
    // a working member while another disk is missing: no healthy copy to keep (a copy done,
    // the old member faulty and the new one in sync, is still a hot replace)
    let new_in_sync = |t: &VTier| {
        t.members
            .iter()
            .any(|m| m.disk == new_name && m.role == MemberRole::Active)
    };
    let rebuild = old == MISSING
        || v.tiers.iter().any(|t| {
            !new_in_sync(t)
                && t.members
                    .iter()
                    .any(|m| m.disk == old && m.role != MemberRole::Active)
        });
    for t in v.tiers.iter().filter(|t| t.created()) {
        let old_active = t.members.iter().any(|m| m.disk == old && m.role == MemberRole::Active);
        if old_active && t.degraded() > 0 {
            // md fills an empty slot before it replaces a member: the new disk would take
            // the missing disk's place, the old one would stay (and failing it first would
            // leave the tier without redundancy while it rebuilds)
            let m = format!(
                "{} (tier {}) misses another disk: the new disk would take that disk's place, not {old}'s; replace the missing disk first",
                t.kname, t.index
            );
            fail(&mut p, C::ArrayNotHealthy, "old", Some(&t.kname), m);
        }
    }
    if !resume {
        tiers_ready(&mut p, &v, rebuild);
        if !rebuild && !pending_of(inv, &label).is_empty() {
            let m = format!("{label}: a growth was interrupted; complete it first");
            fail(&mut p, C::FlexGrowthPending, "volume", Some(&label), m);
        }
    }
    // a new disk the replace already put in a tier is the volume's, not a free disk
    let nd = if resume && v.in_some_tier(&new_name) {
        inv.disks.iter().find(|d| d.name == new_name).cloned()
    } else {
        check_new_disk(inv, &new_name, "new_disk", &mut p)
    };
    let Some(nd) = nd else {
        return finish(p);
    };
    if nd.smr {
        warn_on(&mut p, PlanWarningCode::SmrSlowRebuild, &nd.name);
    }
    v.add_disk(inv, &nd.name);
    p.raw_bytes = nd.size;
    let new_disk = (nd.name.clone(), nd.size);
    let g = match &old_name {
        Some(o) => match tiers::replace(&shapes, &disks, o, &new_disk, v.tolerance, inv.min_tier()) {
            Ok(g) => g,
            Err(tiers::TierError::TooSmall { disk, need }) => {
                let m = replace_too_small_text(&disk, nd.size, need);
                fail(&mut p, C::DiskTooSmall, "new_disk", Some(&disk), m);
                return finish(p);
            }
            Err(e) => {
                fail(&mut p, C::NotAMember, "old", Some(&old), format!("{e:?}"));
                return finish(p);
            }
        },
        None => tiers::grow(&shapes, &disks, std::slice::from_ref(&new_disk), v.tolerance, inv.min_tier()),
    };
    if p.errors.is_empty() && !g.joins.is_empty() {
        scrub_check(&mut p, inv, &v, now, !rebuild && !resume);
    }
    if old == MISSING {
        // slices left on a disk in no tier are never erased by a replace of "missing": the
        // request names no such disk
        for dk in v.disks.iter().filter(|d| !d.parts.is_empty() && d.name != nd.name && !v.in_some_tier(&d.name)) {
            warn_on(&mut p, PlanWarningCode::FlexLeftoverDisk, &dk.name);
        }
    }
    finish_growth(p, v, &goal, &g, &label)
}

/// `POST /api/v1/storage/plan/complete`: what is left of an interrupted growth.
pub fn plan_complete(inv: &Inventory, req: &CompletePlanRequest) -> StoragePlan {
    plan_complete_at(inv, req, now_s())
}

/// [`plan_complete`]: a growth or replace recorded as under way ([`recorded_change`]) goes on
/// as if requested again, whatever step it stopped at; otherwise only what the state shows
/// unfinished is done (and no disk outside the tiers is taken).
pub fn plan_complete_at(inv: &Inventory, req: &CompletePlanRequest, now: u64) -> StoragePlan {
    let label = req.volume.as_str();
    if let Some(c) = recorded_change(inv, label) {
        let mut p = match &c.old {
            Some(old) => plan_replace_at(
                inv,
                &ReplacePlanRequest {
                    array: String::new(),
                    old: old.clone(),
                    new_disk: c.new.first().cloned().unwrap_or_default(),
                    volume: Some(label.to_string()),
                },
                now,
            ),
            None => plan_grow_at(
                inv,
                &GrowPlanRequest {
                    array: String::new(),
                    disks: c.new.clone(),
                    level: None,
                    volume: Some(label.to_string()),
                },
                now,
            ),
        };
        p.operation = "complete_growth".into();
        return p;
    }
    let mut p = StoragePlan {
        operation: "complete_growth".into(),
        ..Default::default()
    };
    let Some(mut v) = load(inv, label, &mut p) else {
        return finish(p);
    };
    p.fault_tolerance = v.tolerance;
    match growth_steps(&mut v, &Goal::default(), None) {
        Ok(steps) => p.steps = steps,
        Err(e) => fail(&mut p, PlanIssueCode::FlexLayoutUnsupported, "volume", Some(label), e),
    }
    p.tiers_after = v.tiers_api();
    p.usable_bytes = v.lv_size;
    p.summary = summary(&v, label);
    if v.fs.is_none() {
        warn(&mut p, PlanWarningCode::GrowFilesystemByHand, PlanArgs::default());
    }
    finish(p)
}

/// md tolerance of a tier array.
fn level_tolerance(l: RaidLevel, raid_disks: u32) -> u32 {
    match l {
        RaidLevel::Raid1 => raid_disks.saturating_sub(1),
        RaidLevel::Raid5 => 1,
        RaidLevel::Raid6 => 2,
        _ => 0,
    }
}

/// The same disks, in any order.
fn same_names(a: &[String], b: &[String]) -> bool {
    let (mut a, mut b) = (a.to_vec(), b.to_vec());
    a.sort();
    b.sort();
    a == b
}

/// A growth or replace recorded as under way ([`FlexChange`]), its disks under today's
/// kernel names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The disk a replace takes out: today's name, or [`MISSING`] (recorded so, or gone
    /// since); `None` for a growth.
    pub old: Option<String>,
    pub new: Vec<String>,
}

impl Change {
    fn describe(&self) -> String {
        match &self.old {
            Some(o) => format!("the replace of {o} by {}", self.new.join(", ")),
            None => format!("the growth by {}", self.new.join(", ")),
        }
    }
}

/// A partition of `disk` is a slice of volume `label` (labeled `rynas-<label>-t<N>`); with
/// `member`, one with an md array on it.
pub fn holds_slice(inv: &Inventory, label: &str, disk: &str, member: bool) -> bool {
    let g = &inv.graph;
    let plabel = format!("rynas-{label}-t");
    g.get(disk).is_some_and(|d| {
        d.children.iter().filter_map(|c| g.get(c)).any(|p| {
            p.kind == "part"
                && p.partlabel.as_deref().is_some_and(|l| l.starts_with(&plabel))
                && (!member || p.children.iter().filter_map(|c| g.get(c)).any(|x| x.is_md()))
        })
    })
}

/// The change recorded for volume `label` that is going on: its new disks found again by
/// identity, one of them at least holding a slice of the volume. A change that stopped
/// before its first step, or was rolled back (its new disks erased again), is not one to go
/// on with: planning ignores it, and the next change or complete clears it.
pub fn recorded_change(inv: &Inventory, label: &str) -> Option<Change> {
    let c = inv.record(label)?.change.as_ref()?;
    let new: Vec<String> = c.new.iter().map(|r| find_disk(inv, r)).collect::<Option<_>>()?;
    if new.is_empty() || !new.iter().any(|d| holds_slice(inv, label, d, false)) {
        return None;
    }
    let old = c.old.as_ref().map(|o| {
        if o.name == MISSING {
            MISSING.to_string()
        } else {
            find_disk(inv, o).unwrap_or_else(|| MISSING.to_string())
        }
    });
    Some(Change { old, new })
}

/// The record of a change before its first step ([`VolumeRecord::change`]): the disks with
/// their names and identities in `inv`.
///
/// [`VolumeRecord::change`]: rynas_api_types::storage::VolumeRecord::change
pub fn change_record(inv: &Inventory, old: Option<&str>, new: &[String]) -> FlexChange {
    let r = |name: &str| DiskRef {
        name: name.to_string(),
        id: inv
            .disks
            .iter()
            .find(|d| d.name == name)
            .map(|d| crate::exec::identity(d).to_string())
            .unwrap_or_default(),
    };
    FlexChange {
        old: old.map(|o| {
            if o == MISSING {
                DiskRef {
                    name: MISSING.into(),
                    id: String::new(),
                }
            } else {
                r(o)
            }
        }),
        new: new.iter().map(|n| r(n)).collect(),
    }
}

/// The created tiers as the tier calculator sees them while a change goes on (`g`): fresh
/// slices of a tier not in its array yet count as members (the new disks partitioned, or
/// half way through the joins), and so does a third mirror out on its way to RAID6
/// ([`third_mirror_out`]); where the disk a replace takes out is still a member, the new
/// disk does not count (a spare, or a copy under way). The calculator then plans the target
/// of the change, less what is done.
fn resume_shapes(v: &View, g: &Goal) -> Vec<Tier> {
    let mut shapes = v.shapes();
    for s in shapes.iter_mut() {
        let Some(t) = v.tiers.iter().find(|t| t.index == s.index) else {
            continue;
        };
        let mut extra: Vec<String> = v.candidates(t, g).iter().map(|p| v.part_disk(&p.kname)).collect();
        if let Some(MirrorOut::Slice(p)) = third_mirror_out(v, t, g) {
            extra.push(v.part_disk(&p.kname));
        }
        for d in extra {
            if !s.members.contains(&d) {
                s.members.push(d);
            }
        }
        if let Some(old) = g.old.as_deref()
            && s.members.iter().any(|m| m == old)
        {
            s.members.retain(|m| !g.new.contains(m));
        }
        natural(&mut s.members);
    }
    shapes
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
