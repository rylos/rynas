//! Capacity of a new volume: the disk layout arithmetic of the classic md levels and the tier
//! calculator of flexible RAID. Pure arithmetic on disk sizes, no system access, so the
//! planner (`rynas-storage`) builds volumes with it and the web interface's capacity
//! calculator shows the same numbers ([`volume`]).
//!
//! Classic level: one partition per disk from [`PART_START`] to [`END_RESERVE`] before the
//! end, cut to the smallest disk, less the md overhead; the level keeps
//! [`capacity_factor`] data members.
//!
//! Flexible RAID: each disk is cut into slices at the sizes of the disks (the "boundaries");
//! slice j of every disk that reaches it forms tier j, one md array. Tier levels follow the
//! tolerance:
//!
//! | members | 1-disk tolerance | 2-disk tolerance |
//! |---|---|---|
//! | 1 | not created | not created |
//! | 2 | RAID1 | not created |
//! | 3 | RAID5 | three-way RAID1 |
//! | 4+ | RAID5 | RAID6 |
//!
//! So the usable space is the sum of the disks minus the largest (1-disk tolerance) or the two
//! largest (2-disk tolerance), less the md overhead of each tier and the LVM metadata of each
//! tier. Boundaries closer than [`MIN_TIER`] merge into the lower one, so a disk "of the same
//! size" a few MB short still counts as equal.

use crate::storage::{FlexTier, RaidLevel};

pub const MIB: u64 = 1 << 20;
/// Partition starts at 1 MiB (sgdisk default alignment).
pub const PART_START: u64 = MIB;
/// Left free at the end of each disk so that a replacement "of the same size" from another
/// vendor (a few MB smaller) still fits.
pub const END_RESERVE: u64 = 128 * MIB;
/// md 1.2 superblock, bitmap and data offset (mdadm reserves room for reshapes). Estimate.
pub const MD_OVERHEAD: u64 = 128 * MIB;
/// LUKS2 header with the default 16 MiB metadata area.
pub const LUKS_HEADER: u64 = 16 * MIB;
/// Thinnest flexible RAID tier: closer boundaries merge into the lower one (and smaller rests
/// are not reported as unused). Tests on loop files pass a smaller value.
pub const MIN_TIER: u64 = 16 << 30;
/// LVM extent of a flexible RAID volume group.
pub const EXTENT: u64 = 4 * MIB;
/// LVM metadata at the start of each physical volume.
pub const PV_META: u64 = MIB;

/// Bytes of a disk a partition can use: from [`PART_START`] to [`END_RESERVE`] before the
/// end, in whole MiB.
pub fn usable_partition(disk_size: u64) -> u64 {
    disk_size.saturating_sub(PART_START + END_RESERVE) / MIB * MIB
}

/// The level accepts `n` disks.
pub fn disk_count_ok(l: RaidLevel, n: usize) -> bool {
    match l {
        RaidLevel::Single => n == 1,
        RaidLevel::Raid0 | RaidLevel::Raid1 | RaidLevel::Flex1 => n >= 2,
        RaidLevel::Raid5 => n >= 3,
        RaidLevel::Raid6 | RaidLevel::Flex2 => n >= 4,
        RaidLevel::Raid10 => n >= 4 && n.is_multiple_of(2),
    }
}

/// Fewest disks a level takes.
pub fn level_min_disks(l: RaidLevel) -> usize {
    match l {
        RaidLevel::Single => 1,
        RaidLevel::Raid0 | RaidLevel::Raid1 | RaidLevel::Flex1 => 2,
        RaidLevel::Raid5 => 3,
        RaidLevel::Raid6 | RaidLevel::Raid10 | RaidLevel::Flex2 => 4,
    }
}

/// Data-bearing members out of `n` and guaranteed fault tolerance. For a flexible RAID this
/// is the case of disks of one size (one tier); mixed sizes are counted by [`plan`].
pub fn capacity_factor(l: RaidLevel, n: u64) -> (u64, u32) {
    match l {
        RaidLevel::Single => (1, 0),
        RaidLevel::Raid0 => (n, 0),
        RaidLevel::Raid1 => (1, n.saturating_sub(1) as u32),
        RaidLevel::Raid5 | RaidLevel::Flex1 => (n.saturating_sub(1), 1),
        RaidLevel::Raid6 | RaidLevel::Flex2 => (n.saturating_sub(2), 2),
        RaidLevel::Raid10 => (n / 2, 1),
    }
}

/// Bytes LVM can allocate on a physical volume of `size` bytes (whole extents after its
/// metadata).
pub fn pv_bytes(size: u64, extent: u64) -> u64 {
    let e = extent.max(MIB);
    size.saturating_sub(PV_META) / e * e
}

// ------------------------------------------------------------------ flexible RAID tiers

/// A disk by kernel name and size in bytes.
pub type DiskSize = (String, u64);

/// One tier: the slice `[start, end)` (absolute byte offsets, identical on every member).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    /// 1-based, lowest first: also the partition number on each member.
    pub index: u32,
    pub start: u64,
    pub end: u64,
    pub members: Vec<String>,
    /// `None`: too few members for the tolerance, the slice stays unused.
    pub level: Option<RaidLevel>,
}

impl Tier {
    pub fn height(&self) -> u64 {
        self.end - self.start
    }

    /// Usable bytes the tier adds (0 when it is not created).
    pub fn usable(&self) -> u64 {
        self.level.map_or(0, |l| {
            data_members(l, self.members.len()) * self.height().saturating_sub(MD_OVERHEAD)
        })
    }

    pub fn created(&self) -> bool {
        self.level.is_some()
    }

    /// The API shape of a planned tier (no array, health or progress yet).
    pub fn to_api(&self) -> FlexTier {
        FlexTier {
            index: self.index,
            start: self.start,
            end: self.end,
            level: self.level,
            members: self.members.clone(),
            usable: self.usable(),
            ..Default::default()
        }
    }
}

/// Why no layout can be computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TierError {
    /// The tolerance is not 1 or 2.
    BadTolerance(u32),
    /// Fewer disks than the tolerance needs (2 for 1-disk, 4 for 2-disk tolerance).
    TooFewDisks { need: usize },
    /// The disk cannot hold the slices it has to (`need` bytes from the start of the disk).
    TooSmall { disk: String, need: u64 },
    /// The disk to replace is not in any tier.
    NotAMember(String),
}

/// md level of a tier with `members` disks, `None` when it is not created.
pub fn tier_level(tolerance: u32, members: usize) -> Option<RaidLevel> {
    match (tolerance, members) {
        (1, 2) | (2, 3) => Some(RaidLevel::Raid1),
        (1, n) if n >= 3 => Some(RaidLevel::Raid5),
        (2, n) if n >= 4 => Some(RaidLevel::Raid6),
        _ => None,
    }
}

/// Fewest members a tier needs at a tolerance.
pub fn min_members(tolerance: u32) -> usize {
    if tolerance >= 2 { 3 } else { 2 }
}

/// Fewest disks a layout needs at a tolerance.
pub fn min_disks(tolerance: u32) -> usize {
    if tolerance >= 2 { 4 } else { 2 }
}

/// Members that carry data in a tier: the rest is redundancy.
pub fn data_members(level: RaidLevel, n: usize) -> u64 {
    let n = n as u64;
    match level {
        RaidLevel::Raid5 => n.saturating_sub(1),
        RaidLevel::Raid6 => n.saturating_sub(2),
        _ => 1,
    }
}

/// Where the last slice a disk of `size` bytes can hold ends (absolute byte offset).
pub fn reach(size: u64) -> u64 {
    PART_START + usable_partition(size)
}

/// The tier boundaries (absolute ends) of disks reaching `reaches`: sorted, in whole MiB,
/// and a boundary closer than `min_tier` to the one below merges into it.
pub fn boundaries(reaches: &[u64], min_tier: u64) -> Vec<u64> {
    let mut v: Vec<u64> = reaches.iter().map(|r| r / MIB * MIB).collect();
    v.sort_unstable();
    v.dedup();
    let mut out: Vec<u64> = Vec::new();
    for r in v {
        let low = out.last().copied().unwrap_or(PART_START);
        if r > low && r - low >= min_tier.max(MIB) {
            out.push(r);
        }
    }
    out
}

/// A computed layout: tiers (created ones first; uncreated ones show the space they would
/// have taken) and the space left on each disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub tolerance: u32,
    pub tiers: Vec<Tier>,
    /// (disk, bytes) no created tier uses, at least `min_tier`.
    pub unused: Vec<(String, u64)>,
}

impl Layout {
    pub fn usable(&self) -> u64 {
        self.tiers.iter().map(Tier::usable).sum()
    }

    /// The tiers to create (always the lowest ones: members only shrink going up).
    pub fn created(&self) -> impl Iterator<Item = &Tier> {
        self.tiers.iter().filter(|t| t.created())
    }
}

/// Space on each disk past the highest created tier it is in, when at least `min_tier`.
pub fn unused_space(tiers: &[Tier], disks: &[DiskSize], min_tier: u64) -> Vec<(String, u64)> {
    disks
        .iter()
        .filter_map(|(name, size)| {
            let top = tiers
                .iter()
                .filter(|t| t.created() && t.members.contains(name))
                .map(|t| t.end)
                .max()
                .unwrap_or(PART_START);
            let rest = reach(*size).saturating_sub(top);
            (rest >= min_tier.max(MIB)).then(|| (name.clone(), rest))
        })
        .collect()
}

/// The layout of a new flexible RAID over `disks` at `tolerance` (1 or 2).
pub fn plan(disks: &[DiskSize], tolerance: u32, min_tier: u64) -> Result<Layout, TierError> {
    if !(1..=2).contains(&tolerance) {
        return Err(TierError::BadTolerance(tolerance));
    }
    if disks.len() < min_disks(tolerance) {
        return Err(TierError::TooFewDisks {
            need: min_disks(tolerance),
        });
    }
    let reaches: Vec<u64> = disks.iter().map(|(_, s)| reach(*s)).collect();
    let bounds = boundaries(&reaches, min_tier);
    let mut tiers = Vec::new();
    let mut start = PART_START;
    for (i, &end) in bounds.iter().enumerate() {
        let members: Vec<String> = disks
            .iter()
            .filter(|(_, s)| reach(*s) >= end)
            .map(|(n, _)| n.clone())
            .collect();
        tiers.push(Tier {
            index: i as u32 + 1,
            start,
            end,
            level: tier_level(tolerance, members.len()),
            members,
        });
        start = end;
    }
    let unused = unused_space(&tiers, disks, min_tier);
    Ok(Layout {
        tolerance,
        tiers,
        unused,
    })
}

/// Usable bytes of tiers once joined by LVM (whole extents of each physical volume).
pub fn lvm_usable<'a>(tiers: impl IntoIterator<Item = &'a Tier>) -> u64 {
    tiers.into_iter().map(|t| pv_bytes(t.usable(), EXTENT)).sum()
}

// ------------------------------------------------------------------ a new volume

/// One disk of a [`Capacity`], in the order given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskUse {
    pub size: u64,
    /// Bytes the volume's arrays take on the disk (redundancy included).
    pub used: u64,
    /// Bytes no array takes ([`Capacity::wasted`] is their sum).
    pub unused: u64,
}

/// What a new volume over some disks gives: the planner's `usable_bytes`, `raw_bytes`,
/// `wasted_bytes` and `fault_tolerance` come from here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Capacity {
    pub raw: u64,
    pub usable: u64,
    pub wasted: u64,
    pub tolerance: u32,
    pub disks: Vec<DiskUse>,
}

/// A classic md volume (one array) over disks of `sizes`. A disk count the level does not
/// take gives no usable space (the plan refuses it).
pub fn classic(level: RaidLevel, sizes: &[u64], encrypt: bool) -> Capacity {
    let n = sizes.len() as u64;
    let min = sizes.iter().copied().min().unwrap_or(0);
    let member = usable_partition(min).saturating_sub(MD_OVERHEAD);
    let (factor, tolerance) = if disk_count_ok(level, sizes.len()) {
        capacity_factor(level, n)
    } else {
        (0, 0)
    };
    let disks: Vec<DiskUse> = sizes
        .iter()
        .map(|&size| DiskUse {
            size,
            used: min,
            unused: size - min,
        })
        .collect();
    Capacity {
        raw: sizes.iter().sum(),
        usable: (factor * member).saturating_sub(if encrypt { LUKS_HEADER } else { 0 }),
        wasted: disks.iter().map(|d| d.unused).sum(),
        tolerance,
        disks,
    }
}

/// A new flexible RAID with the tiers of `layout` over `disks`: usable space after LVM,
/// the space of each disk that no created tier takes.
pub fn flexible(layout: &Layout, disks: &[DiskSize], encrypt: bool) -> Capacity {
    let disks: Vec<DiskUse> = disks
        .iter()
        .map(|(name, size)| {
            let used: u64 = layout
                .created()
                .filter(|t| t.members.contains(name))
                .map(Tier::height)
                .sum();
            DiskUse {
                size: *size,
                used,
                unused: (reach(*size) - PART_START).saturating_sub(used),
            }
        })
        .collect();
    Capacity {
        raw: disks.iter().map(|d| d.size).sum(),
        usable: lvm_usable(layout.created()).saturating_sub(if encrypt { LUKS_HEADER } else { 0 }),
        wasted: disks.iter().map(|d| d.unused).sum(),
        tolerance: layout.tolerance,
        disks,
    }
}

/// A new volume at `level` over disks of `sizes` (unencrypted when `encrypt` is false), as
/// the planner would build it; `None` when the level does not take that many disks.
pub fn volume(level: RaidLevel, sizes: &[u64], encrypt: bool, min_tier: u64) -> Option<Capacity> {
    if !disk_count_ok(level, sizes.len()) {
        return None;
    }
    if level.is_flexible() {
        let disks: Vec<DiskSize> = sizes
            .iter()
            .enumerate()
            .map(|(i, s)| (i.to_string(), *s))
            .collect();
        let layout = plan(&disks, level.tolerance(), min_tier).ok()?;
        Some(flexible(&layout, &disks, encrypt))
    } else {
        Some(classic(level, sizes, encrypt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TB: u64 = 1_000_000_000_000;

    fn tb(bytes: u64) -> f64 {
        (bytes as f64 / TB as f64 * 100.0).round() / 100.0
    }

    #[test]
    fn classic_levels() {
        let c = volume(RaidLevel::Raid5, &[4 * TB, 4 * TB, 8 * TB], false, MIN_TIER).unwrap();
        assert_eq!(c.tolerance, 1);
        assert_eq!(tb(c.usable), 8.0);
        assert_eq!(c.wasted, 4 * TB);
        assert_eq!(c.disks[2], DiskUse { size: 8 * TB, used: 4 * TB, unused: 4 * TB });
        let c = volume(RaidLevel::Raid1, &[4 * TB; 3], false, MIN_TIER).unwrap();
        assert_eq!((tb(c.usable), c.tolerance), (4.0, 2));
        let enc = volume(RaidLevel::Raid1, &[4 * TB; 3], true, MIN_TIER).unwrap();
        assert_eq!(c.usable - enc.usable, LUKS_HEADER);
        assert!(volume(RaidLevel::Raid10, &[4 * TB; 5], false, MIN_TIER).is_none());
        assert!(volume(RaidLevel::Single, &[4 * TB; 2], false, MIN_TIER).is_none());
        assert_eq!(volume(RaidLevel::Raid0, &[4 * TB; 2], false, MIN_TIER).unwrap().tolerance, 0);
    }

    #[test]
    fn flexible_levels() {
        // 4 + 8 + 8 at 1-disk tolerance: RAID5 of 3 × 4 TB, RAID1 of 2 × 4 TB = 12 TB
        let c = volume(RaidLevel::Flex1, &[4 * TB, 8 * TB, 8 * TB], false, MIN_TIER).unwrap();
        assert_eq!((tb(c.usable), c.tolerance), (12.0, 1));
        assert!(c.disks.iter().all(|d| d.unused < MIN_TIER), "{:?}", c.disks);
        // 4 + 4 + 12: the 8 TB the second largest does not reach stay unused
        let c = volume(RaidLevel::Flex1, &[4 * TB, 4 * TB, 12 * TB], false, MIN_TIER).unwrap();
        assert_eq!(tb(c.usable), 8.0);
        assert_eq!(tb(c.disks[2].unused), 8.0);
        assert_eq!(c.wasted, c.disks.iter().map(|d| d.unused).sum::<u64>());
        // 2-disk tolerance needs four disks
        assert!(volume(RaidLevel::Flex2, &[8 * TB; 3], false, MIN_TIER).is_none());
        let c = volume(RaidLevel::Flex2, &[4 * TB, 8 * TB, 8 * TB, 8 * TB], false, MIN_TIER).unwrap();
        assert_eq!((tb(c.usable), c.tolerance), (12.0, 2));
    }
}
