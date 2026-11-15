//! Capacity of a new volume: the disk layout arithmetic of the classic md levels and the tier
//! calculator of flexible RAID. Pure arithmetic on disk sizes, no system access, so the
//! planner (`rynas-storage`) builds volumes with it and the web interface's capacity
//! calculator shows the same numbers ([`volume`]).
//!
//! Classic level: one partition per disk from [`PART_START`] to [`END_RESERVE`] before the
//! end; md uses each member as large as the smallest one, less its data offset
//! ([`md_component`]); the level keeps [`capacity_factor`] data members.
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
//! largest (2-disk tolerance), less the md data offset of each tier and the LVM metadata of
//! each tier. Boundaries closer than [`MIN_TIER`] merge into the lower one, so a disk "of the
//! same size" a few MB short still counts as equal.
//!
//! [`END_RESERVE`] is left free only when a disk is partitioned for the first slices it gets:
//! a disk that joins existing arrays (a replacement, a growth) only has to hold their slices
//! before its backup GPT ([`fits`]), so a disk "of the same size" a few MB smaller still
//! joins.

use crate::storage::{FlexTier, RaidLevel};

pub const MIB: u64 = 1 << 20;
/// Partition starts at 1 MiB (sgdisk default alignment).
pub const PART_START: u64 = MIB;
/// Left free at the end of a disk partitioned for a new array, so that the slices end before
/// the end of a disk "of the same size" a few MB smaller: such a disk can replace it or join
/// the arrays later ([`fits`]).
pub const END_RESERVE: u64 = 128 * MIB;
/// Room a partition must leave at the end of a disk: the backup GPT (33 sectors of 512 bytes,
/// 6 of 4096), rounded up to a whole MiB.
pub const GPT_TAIL: u64 = MIB;
/// Largest md 1.2 data offset mdadm gives a member (superblock, internal bitmap and the
/// 128 MiB reshape headroom, rounded up to a MiB): the offset of members over 128 GiB. Smaller
/// members get less ([`md_data_offset`]).
pub const MD_OVERHEAD: u64 = 129 * MIB;
/// md chunk of the striped levels (mdadm's default).
pub const MD_CHUNK: u64 = 512 * 1024;
/// LUKS2 header with the default 16 MiB metadata area.
pub const LUKS_HEADER: u64 = 16 * MIB;
/// Thinnest flexible RAID tier: closer boundaries merge into the lower one (and smaller rests
/// are not reported as unused). Tests on loop files pass a smaller value.
pub const MIN_TIER: u64 = 16 << 30;
/// LVM extent of a flexible RAID volume group.
pub const EXTENT: u64 = 4 * MIB;
/// LVM metadata at the start of a physical volume: its data starts 1 MiB in (LVM's default
/// alignment), or at the md stripe width when that is larger ([`pv_start`]).
pub const PV_META: u64 = MIB;
/// Smallest disk the planner takes.
pub const MIN_DISK: u64 = 2 * (PART_START + END_RESERVE + MD_OVERHEAD);

/// Bytes of a disk a partition can use: from [`PART_START`] to [`END_RESERVE`] before the
/// end, in whole MiB.
pub fn usable_partition(disk_size: u64) -> u64 {
    disk_size.saturating_sub(PART_START + END_RESERVE) / MIB * MIB
}

/// Where a partition can end at the latest on a disk of `size` bytes: before the backup GPT,
/// in whole MiB.
pub fn limit(size: u64) -> u64 {
    size.saturating_sub(GPT_TAIL) / MIB * MIB
}

/// A disk of `size` bytes holds a slice ending at `end` (absolute byte offset).
pub fn fits(size: u64, end: u64) -> bool {
    limit(size) >= end
}

/// Bytes a disk of `size` bytes misses to hold a slice ending at `end` (0 when it fits): the
/// smallest disk that holds it has the slice's end, in whole MiB, plus [`GPT_TAIL`].
pub fn shortfall(size: u64, end: u64) -> u64 {
    (end.div_ceil(MIB) * MIB + GPT_TAIL).saturating_sub(size)
}

/// Space left over on a disk that is worth reporting as unused: rests smaller than
/// `min_tier` (disks "of the same size" from different makes differ by a few MB, a tier
/// thinner than that is never made) count as nothing.
pub fn reportable_unused(rest: u64, min_tier: u64) -> u64 {
    if rest >= min_tier.max(MIB) { rest } else { 0 }
}

/// Two disk sizes differ by less than [`MIN_TIER`] ("the same size" from different makes).
pub fn negligible_difference(a: u64, b: u64) -> bool {
    a.abs_diff(b) < MIN_TIER
}

/// The level accepts `n` disks.
pub fn disk_count_ok(l: RaidLevel, n: usize) -> bool {
    match l {
        RaidLevel::Single => n == 1,
        RaidLevel::Raid0 | RaidLevel::Raid1 | RaidLevel::Flex1 => n >= 2,
        RaidLevel::Raid5 => n >= 3,
        // md's RAID10 (near=2) takes an odd count too: every block is on two disks
        RaidLevel::Raid6 | RaidLevel::Raid10 | RaidLevel::Flex2 => n >= 4,
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
/// is the case of disks of one size (one tier); mixed sizes are counted by [`plan`]. RAID10
/// keeps half of the members' space: `n / 2` (an odd count keeps half a member more, see
/// [`md_array_size`]).
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

/// Data offset mdadm gives a new metadata 1.2 member of `dev` bytes: 4 KiB before the
/// superblock, the superblock and bad block log, the internal bitmap space and the reshape
/// headroom (128 MiB, halved on small members until it is at most 1/1024 of the member, at
/// least 1 MiB), rounded up to a MiB. 129 MiB for members over 128 GiB.
pub fn md_data_offset(dev: u64) -> u64 {
    let sectors = dev / 512;
    let mut headroom: u64 = 128 * 2048;
    while (headroom << 10) > sectors && headroom > 2048 {
        headroom >>= 1;
    }
    let bitmap = if sectors < 128 {
        0
    } else if sectors - 128 >= 200 * 1024 * 1024 * 2 {
        256
    } else if sectors - 8 > 8 * 1024 * 1024 * 2 {
        128
    } else {
        8
    };
    (24 + bitmap + headroom).div_ceil(2048) * 2048 * 512
}

/// Bytes md uses of a member partition of `dev` bytes: after its data offset, rounded down
/// to the chunk (striped levels) or to 64 KiB (mirrors).
pub fn md_component(level: RaidLevel, dev: u64) -> u64 {
    let unit = match level {
        RaidLevel::Single | RaidLevel::Raid1 => 64 * 1024,
        _ => MD_CHUNK,
    };
    dev.saturating_sub(md_data_offset(dev)) / unit * unit
}

/// Size md gives an array of `n` members at `level` over components of `component` bytes
/// (what [`md_component`] gives, or md's `component_size`).
pub fn md_array_size_of_components(level: RaidLevel, n: usize, component: u64) -> u64 {
    let n = n as u64;
    match level {
        RaidLevel::Single | RaidLevel::Raid1 => component,
        RaidLevel::Raid0 => n * component,
        RaidLevel::Raid5 | RaidLevel::Flex1 => n.saturating_sub(1) * component,
        RaidLevel::Raid6 | RaidLevel::Flex2 => n.saturating_sub(2) * component,
        // near=2: two copies of each chunk over all the members, odd counts included
        RaidLevel::Raid10 => component / MD_CHUNK * n / 2 * MD_CHUNK,
    }
}

/// Size md gives an array of `n` members at `level` over partitions of `dev` bytes.
pub fn md_array_size(level: RaidLevel, n: usize, dev: u64) -> u64 {
    md_array_size_of_components(level, n, md_component(level, dev))
}

/// Where LVM puts the first extent of a physical volume on an md array of `members` disks at
/// `level`: 1 MiB, or the stripe width (chunk × data members) when larger (LVM aligns the
/// data to md's stripe).
pub fn pv_start(level: Option<RaidLevel>, members: usize) -> u64 {
    let stripe = match level {
        Some(l @ (RaidLevel::Raid5 | RaidLevel::Raid6)) => data_members(l, members) * MD_CHUNK,
        _ => 0,
    };
    stripe.max(PV_META)
}

/// Bytes LVM can allocate on a physical volume of `size` bytes (whole extents after its
/// metadata, data starting at 1 MiB).
pub fn pv_bytes(size: u64, extent: u64) -> u64 {
    pv_bytes_from(size, extent, PV_META)
}

/// Bytes LVM can allocate on a physical volume of `size` bytes whose data starts at `start`.
pub fn pv_bytes_from(size: u64, extent: u64, start: u64) -> u64 {
    let e = extent.max(MIB);
    size.saturating_sub(start) / e * e
}

/// Bytes LVM can allocate on a tier array of `size` bytes ([`pv_start`]).
pub fn tier_pv_bytes(size: u64, extent: u64, level: Option<RaidLevel>, members: usize) -> u64 {
    pv_bytes_from(size, extent, pv_start(level, members))
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
        self.level
            .map_or(0, |l| md_array_size(l, self.members.len(), self.height()))
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

/// Space on each disk past the highest created tier it is in, when at least `min_tier`
/// ([`reportable_unused`]).
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
            let rest = reportable_unused(reach(*size).saturating_sub(top), min_tier);
            (rest > 0).then(|| (name.clone(), rest))
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

/// Usable bytes of tiers once joined by LVM (whole extents of each physical volume, data
/// aligned as LVM aligns it on md: [`pv_start`]).
pub fn lvm_usable<'a>(tiers: impl IntoIterator<Item = &'a Tier>) -> u64 {
    tiers
        .into_iter()
        .map(|t| tier_pv_bytes(t.usable(), EXTENT, t.level, t.members.len()))
        .sum()
}

// ------------------------------------------------------------------ a new volume

/// One disk of a [`Capacity`], in the order given. `used + unused` never exceeds what a
/// partition can take ([`usable_partition`]): the rest is the partition alignment, the
/// end reserve and leftovers too small to count ([`reportable_unused`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskUse {
    pub size: u64,
    /// Bytes the volume's arrays take on the disk: their partitions, md data offsets and
    /// redundancy included.
    pub used: u64,
    /// Bytes partitions could take but no array uses ([`Capacity::wasted`] is their sum);
    /// rests thinner than the thinnest tier count as nothing.
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

/// A classic md volume (one array) over disks of `sizes`: md uses every member as large as
/// the smallest; what larger disks have beyond counts as unused from `min_tier` on (smaller
/// differences are disks "of the same size" from different makes). A disk count the level
/// does not take gives no usable space (the plan refuses it).
pub fn classic(level: RaidLevel, sizes: &[u64], encrypt: bool, min_tier: u64) -> Capacity {
    let min = sizes.iter().copied().min().unwrap_or(0);
    let part = usable_partition(min);
    let (usable, tolerance) = if disk_count_ok(level, sizes.len()) {
        let array = md_array_size(level, sizes.len(), part);
        (array, capacity_factor(level, sizes.len() as u64).1)
    } else {
        (0, 0)
    };
    let disks: Vec<DiskUse> = sizes
        .iter()
        .map(|&size| DiskUse {
            size,
            used: part,
            unused: reportable_unused(usable_partition(size) - part, min_tier),
        })
        .collect();
    Capacity {
        raw: sizes.iter().sum(),
        usable: usable.saturating_sub(if encrypt { LUKS_HEADER } else { 0 }),
        wasted: disks.iter().map(|d| d.unused).sum(),
        tolerance,
        disks,
    }
}

/// A new flexible RAID with the tiers of `layout` over `disks`: usable space after LVM,
/// the space of each disk that no created tier takes (as [`Layout::unused`] lists it).
pub fn flexible(layout: &Layout, disks: &[DiskSize], encrypt: bool) -> Capacity {
    let disks: Vec<DiskUse> = disks
        .iter()
        .map(|(name, size)| DiskUse {
            size: *size,
            used: layout
                .created()
                .filter(|t| t.members.contains(name))
                .map(Tier::height)
                .sum(),
            unused: layout
                .unused
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, b)| *b)
                .sum(),
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
/// the planner would build it; `None` when the planner refuses it: the level does not take
/// that many disks, a disk is under [`MIN_DISK`], or no flexible RAID tier can be made (too
/// few disks reach [`MIN_TIER`] together).
pub fn volume(level: RaidLevel, sizes: &[u64], encrypt: bool, min_tier: u64) -> Option<Capacity> {
    if !disk_count_ok(level, sizes.len()) || sizes.iter().any(|s| *s < MIN_DISK) {
        return None;
    }
    if level.is_flexible() {
        let disks: Vec<DiskSize> = sizes
            .iter()
            .enumerate()
            .map(|(i, s)| (i.to_string(), *s))
            .collect();
        let layout = plan(&disks, level.tolerance(), min_tier).ok()?;
        layout.created().next()?;
        Some(flexible(&layout, &disks, encrypt))
    } else {
        Some(classic(level, sizes, encrypt, min_tier))
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
        assert_eq!(tb(c.wasted), 4.0);
        let part = usable_partition(4 * TB);
        assert_eq!(c.disks[2], DiskUse { size: 8 * TB, used: part, unused: usable_partition(8 * TB) - part });
        assert_eq!(c.disks[0], DiskUse { size: 4 * TB, used: part, unused: 0 });
        let c = volume(RaidLevel::Raid1, &[4 * TB; 3], false, MIN_TIER).unwrap();
        assert_eq!((tb(c.usable), c.tolerance), (4.0, 2));
        let enc = volume(RaidLevel::Raid1, &[4 * TB; 3], true, MIN_TIER).unwrap();
        assert_eq!(c.usable - enc.usable, LUKS_HEADER);
        // md's RAID10 takes five disks: half of 5 × 4 TB
        let c = volume(RaidLevel::Raid10, &[4 * TB; 5], false, MIN_TIER).unwrap();
        assert_eq!((tb(c.usable), c.tolerance), (10.0, 1));
        assert!(volume(RaidLevel::Raid10, &[4 * TB; 3], false, MIN_TIER).is_none());
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

    /// No tier can be made: two disks under the thinnest tier, or one disk alone above it.
    #[test]
    fn flexible_without_a_tier_is_refused() {
        let g = 1u64 << 30;
        assert!(volume(RaidLevel::Flex1, &[10 * g, 10 * g], false, MIN_TIER).is_none());
        assert!(volume(RaidLevel::Flex1, &[10 * g, 4 * TB], false, MIN_TIER).is_none());
        assert!(volume(RaidLevel::Flex2, &[10 * g, 10 * g, 4 * TB, 4 * TB], false, MIN_TIER).is_none());
        // a disk under the planner's minimum
        assert!(volume(RaidLevel::Raid1, &[100 * MIB, 4 * TB], false, MIN_TIER).is_none());
        assert!(volume(RaidLevel::Flex1, &[10 * g, 4 * TB, 4 * TB], false, MIN_TIER).is_some());
    }

    /// The bar and the warning count the same unused space: rests thinner than a tier are
    /// nothing, on every disk.
    #[test]
    fn unused_space_is_counted_once() {
        let g = 1u64 << 30;
        // 4, 4, 4 + 15 GiB, 4 + 16.2 GiB: the 15 GiB rest is too thin, the 16.2 GiB one counts
        let sizes = [4 * TB, 4 * TB, 4 * TB + 15 * g, 4 * TB + 16 * g + 200 * MIB];
        let c = volume(RaidLevel::Flex1, &sizes, false, MIN_TIER).unwrap();
        let disks: Vec<DiskSize> = sizes.iter().enumerate().map(|(i, s)| (i.to_string(), *s)).collect();
        let l = plan(&disks, 1, MIN_TIER).unwrap();
        assert_eq!(c.wasted, l.unused.iter().map(|(_, b)| b).sum::<u64>());
        assert_eq!(c.disks[2].unused, 0);
        assert_eq!(l.unused.len(), 1);
        assert!(c.disks[3].unused >= 16 * g, "{:?}", c.disks);
        for d in &c.disks {
            assert!(d.used + d.unused <= usable_partition(d.size), "{d:?}");
        }
    }

    /// "8 TB" disks of two makes (5 MB apart) are the same size: nothing unused, classic or
    /// flexible.
    #[test]
    fn near_equal_disks_waste_nothing() {
        let sizes = [8 * TB, 8 * TB + 5_000_000, 8 * TB];
        for level in [RaidLevel::Raid5, RaidLevel::Flex1] {
            let c = volume(level, &sizes, false, MIN_TIER).unwrap();
            assert_eq!(c.wasted, 0, "{level:?}");
            assert!(c.disks.iter().all(|d| d.unused == 0), "{level:?} {:?}", c.disks);
        }
        assert!(negligible_difference(8 * TB, 8 * TB + 5_000_000));
        assert!(!negligible_difference(8 * TB, 8 * TB + 20 * (1 << 30)));
        // classic and flexible count a used disk the same way: the partition md uses
        let classic = volume(RaidLevel::Raid5, &[8 * TB; 3], false, MIN_TIER).unwrap();
        let flex = volume(RaidLevel::Flex1, &[8 * TB; 3], false, MIN_TIER).unwrap();
        assert_eq!(classic.disks[0].used, flex.disks[0].used);
    }

    /// md's data offset as mdadm sets it: 264192 sectors on large members, less on small ones;
    /// a 64 MiB tier (tests on loop files) keeps most of its space.
    #[test]
    fn md_offsets_and_components() {
        assert_eq!(md_data_offset(4 * TB), 264_192 * 512);
        assert_eq!(md_data_offset(4 * TB), MD_OVERHEAD);
        assert_eq!(md_data_offset(64 * MIB), 2 * MIB);
        assert!(md_data_offset(1 << 30) < 4 * MIB);
        let c = md_component(RaidLevel::Raid5, 4 * TB);
        assert_eq!(c % MD_CHUNK, 0);
        assert!(4 * TB - c - MD_OVERHEAD < MD_CHUNK);
        let t = Tier { index: 1, start: MIB, end: 65 * MIB, members: vec!["a".into(), "b".into(), "c".into()], level: Some(RaidLevel::Raid5) };
        assert_eq!(t.usable(), 2 * 62 * MIB);
        // RAID10 near=2 over 5 members: whole chunks, half of them
        assert_eq!(md_array_size_of_components(RaidLevel::Raid10, 5, 3 * MD_CHUNK), 7 * MD_CHUNK);
    }

    /// LVM puts the first extent at the stripe width of a RAID5/6 tier when it is over 1 MiB.
    #[test]
    fn pv_alignment() {
        assert_eq!(pv_start(Some(RaidLevel::Raid1), 2), MIB);
        assert_eq!(pv_start(Some(RaidLevel::Raid5), 3), MIB);
        assert_eq!(pv_start(Some(RaidLevel::Raid5), 4), 3 * MD_CHUNK);
        assert_eq!(pv_start(Some(RaidLevel::Raid6), 6), 4 * MD_CHUNK);
        assert_eq!(tier_pv_bytes(100 * MIB, EXTENT, Some(RaidLevel::Raid5), 4), 96 * MIB);
        assert_eq!(pv_bytes(100 * MIB, EXTENT), 96 * MIB);
        assert_eq!(pv_bytes_from(100 * MIB, EXTENT, 5 * MIB), 92 * MIB);
    }

    /// A disk a few MB smaller still holds slices that end before another disk's reserve.
    #[test]
    fn fitting_before_the_backup_gpt() {
        let end = reach(8 * TB);
        assert!(fits(8 * TB - 5_000_000, end));
        assert_eq!(shortfall(8 * TB - 5_000_000, end), 0);
        assert!(!fits(end, end));
        assert_eq!(shortfall(end, end), MIB);
        assert_eq!(limit(8 * TB) % MIB, 0);
        assert!(limit(8 * TB) + GPT_TAIL <= 8 * TB);
    }
}
