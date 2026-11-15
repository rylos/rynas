//! The tier calculator of flexible RAID: pure arithmetic on disk sizes, no system access.
//! The layout of a new volume lives in [`rynas_api_types::capacity`] (the web interface's
//! capacity calculator runs the same code); growths and replacements are here.
//!
//! Each disk is cut into slices at the sizes of the disks (the "boundaries"); slice j of
//! every disk that reaches it forms tier j, one md array. Tier levels follow the tolerance:
//!
//! | members | 1-disk tolerance | 2-disk tolerance |
//! |---|---|---|
//! | 1 | not created | not created |
//! | 2 | RAID1 | not created |
//! | 3 | RAID5 | three-way RAID1 |
//! | 4+ | RAID5 | RAID6 |
//!
//! So the usable space is the sum of the disks minus the largest (1-disk tolerance) or the two
//! largest (2-disk tolerance), less the md overhead of each tier. Boundaries closer than
//! [`MIN_TIER`] merge into the lower one, so a disk "of the same size" a few MB short still
//! counts as equal. Once created, tiers are fixed: a new disk joins every tier it covers, and
//! a new top tier is made only when enough disks reach past the top one.

use rynas_api_types::storage::RaidLevel;

use crate::plan::{MIB, PART_START};
// the arithmetic of a new layout is shared with the web interface's capacity calculator
pub use rynas_api_types::capacity::{
    DiskSize, Layout, MIN_TIER, Tier, TierError, boundaries, data_members, min_disks, min_members, plan, reach,
    tier_level,
};
use rynas_api_types::capacity::unused_space as unused;

/// A tier whose level changes in a growth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelChange {
    pub index: u32,
    pub from: RaidLevel,
    pub to: RaidLevel,
}

impl LevelChange {
    /// A three-way RAID1 becoming RAID6 goes through RAID5: the tier tolerates one disk
    /// until the reshape ends.
    pub fn reduces_tolerance(&self) -> bool {
        self.from == RaidLevel::Raid1 && self.to == RaidLevel::Raid6
    }
}

/// What adding or replacing disks does to existing tiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Growth {
    /// Every tier after the change (existing ones keep their slice and index).
    pub tiers: Vec<Tier>,
    /// (disk, tier index): a new partition joining an existing tier, lowest tier first per
    /// disk.
    pub joins: Vec<(String, u32)>,
    /// Tiers the replacement disk takes over from the old one (hot replace).
    pub replaced: Vec<u32>,
    /// Indices of the new top tiers.
    pub created: Vec<u32>,
    pub level_changes: Vec<LevelChange>,
    /// New disks that cannot hold the lowest tier.
    pub too_small: Vec<String>,
    pub unused: Vec<(String, u64)>,
}

impl Growth {
    pub fn usable(&self) -> u64 {
        self.tiers.iter().map(Tier::usable).sum()
    }
}

fn empty_growth(existing: &[Tier]) -> Growth {
    Growth {
        tiers: existing.to_vec(),
        joins: Vec::new(),
        replaced: Vec::new(),
        created: Vec::new(),
        level_changes: Vec::new(),
        too_small: Vec::new(),
        unused: Vec::new(),
    }
}

/// `disk` joins every existing tier it fully covers.
fn join_covered(g: &mut Growth, disk: &str, reach: u64) {
    for t in g.tiers.iter_mut() {
        if reach >= t.end && !t.members.iter().any(|m| m == disk) {
            t.members.push(disk.to_string());
            g.joins.push((disk.to_string(), t.index));
        }
    }
}

/// New levels of the grown tiers, then new top tiers while enough disks reach past the top,
/// then the unused space.
fn finish_growth(
    mut g: Growth,
    before: &[Tier],
    disks: &[DiskSize],
    tolerance: u32,
    min_tier: u64,
) -> Growth {
    for t in g.tiers.iter_mut() {
        let to = tier_level(tolerance, t.members.len());
        if let (Some(from), Some(to)) = (before.iter().find(|b| b.index == t.index).and_then(|b| b.level), to)
            && from != to
        {
            g.level_changes.push(LevelChange {
                index: t.index,
                from,
                to,
            });
        }
        t.level = to.or(t.level);
    }
    loop {
        let top = g.tiers.last().map_or(PART_START, |t| t.end);
        let over: Vec<(&String, u64)> = disks
            .iter()
            .map(|(n, s)| (n, reach(*s).saturating_sub(top) / MIB * MIB))
            .filter(|(_, x)| *x >= min_tier.max(MIB))
            .collect();
        if over.len() < min_members(tolerance) {
            break;
        }
        let height = over.iter().map(|(_, x)| *x).min().unwrap_or(0);
        let members: Vec<String> = over.iter().map(|(n, _)| (*n).clone()).collect();
        let index = g.tiers.last().map_or(1, |t| t.index + 1);
        g.tiers.push(Tier {
            index,
            start: top,
            end: top + height,
            level: tier_level(tolerance, members.len()),
            members,
        });
        g.created.push(index);
    }
    g.unused = unused(&g.tiers, disks, min_tier);
    g
}

/// Adding `new` disks to the existing (created) tiers of a layout over `disks`.
pub fn grow(
    existing: &[Tier],
    disks: &[DiskSize],
    new: &[DiskSize],
    tolerance: u32,
    min_tier: u64,
) -> Growth {
    let mut g = empty_growth(existing);
    let mut all: Vec<DiskSize> = disks.to_vec();
    let lowest = existing.first().map_or(PART_START, |t| t.end);
    for (name, size) in new {
        if reach(*size) < lowest {
            g.too_small.push(name.clone());
            continue;
        }
        join_covered(&mut g, name, reach(*size));
        all.push((name.clone(), *size));
    }
    finish_growth(g, existing, &all, tolerance, min_tier)
}

/// Replacing `old` with `new` (at least as large as the top of the tiers `old` is in): the
/// new disk takes the old one's place in those tiers, then joins every other tier it covers.
pub fn replace(
    existing: &[Tier],
    disks: &[DiskSize],
    old: &str,
    new: &DiskSize,
    tolerance: u32,
    min_tier: u64,
) -> Result<Growth, TierError> {
    let old_top = existing
        .iter()
        .filter(|t| t.members.iter().any(|m| m == old))
        .map(|t| t.end)
        .max()
        .ok_or_else(|| TierError::NotAMember(old.to_string()))?;
    let new_reach = reach(new.1);
    if new_reach < old_top {
        return Err(TierError::TooSmall {
            disk: new.0.clone(),
            need: old_top,
        });
    }
    let mut g = empty_growth(existing);
    for t in g.tiers.iter_mut() {
        if let Some(m) = t.members.iter_mut().find(|m| *m == old) {
            *m = new.0.clone();
            g.replaced.push(t.index);
        }
    }
    join_covered(&mut g, &new.0, new_reach);
    let all: Vec<DiskSize> = disks
        .iter()
        .filter(|(n, _)| n != old)
        .cloned()
        .chain(std::iter::once(new.clone()))
        .collect();
    Ok(finish_growth(g, existing, &all, tolerance, min_tier))
}

/// The disk sizes the tiers need: for each member, the end of the highest tier it is in.
pub fn needed_reach(tiers: &[Tier], disk: &str) -> u64 {
    tiers
        .iter()
        .filter(|t| t.members.iter().any(|m| m == disk))
        .map(|t| t.end)
        .max()
        .unwrap_or(PART_START)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::MD_OVERHEAD;
    use RaidLevel::{Raid1, Raid5, Raid6};

    const TB: u64 = 1_000_000_000_000;

    fn disks(sizes: &[(&str, u64)]) -> Vec<DiskSize> {
        sizes.iter().map(|(n, s)| (n.to_string(), s * TB)).collect()
    }

    /// Usable TB, to the design table's precision (md overhead is a few hundred MB).
    fn tb(bytes: u64) -> f64 {
        (bytes as f64 / TB as f64 * 100.0).round() / 100.0
    }

    fn levels(tiers: &[Tier]) -> Vec<(Option<RaidLevel>, usize)> {
        tiers.iter().map(|t| (t.level, t.members.len())).collect()
    }

    /// Every case of the design table for a new volume: tier levels, member counts and the
    /// usable capacity.
    #[test]
    fn new_layouts() {
        struct Case {
            disks: &'static [(&'static str, u64)],
            tolerance: u32,
            tiers: &'static [(Option<RaidLevel>, usize)],
            usable_tb: f64,
        }
        let cases = [
            Case {
                disks: &[("sda", 4), ("sdb", 8), ("sdc", 8)],
                tolerance: 1,
                tiers: &[(Some(Raid5), 3), (Some(Raid1), 2)],
                usable_tb: 12.0,
            },
            Case {
                disks: &[("sda", 4), ("sdb", 4), ("sdc", 8), ("sdd", 12)],
                tolerance: 1,
                tiers: &[(Some(Raid5), 4), (Some(Raid1), 2), (None, 1)],
                usable_tb: 16.0,
            },
            Case {
                disks: &[("sda", 4), ("sdb", 4), ("sdc", 8), ("sdd", 12)],
                tolerance: 2,
                tiers: &[(Some(Raid6), 4), (None, 2), (None, 1)],
                usable_tb: 8.0,
            },
            Case {
                disks: &[("sda", 4), ("sdb", 4), ("sdc", 8), ("sdd", 8)],
                tolerance: 2,
                tiers: &[(Some(Raid6), 4), (None, 2)],
                usable_tb: 8.0,
            },
            Case {
                disks: &[("sda", 4), ("sdb", 8), ("sdc", 8), ("sdd", 8)],
                tolerance: 2,
                tiers: &[(Some(Raid6), 4), (Some(Raid1), 3)],
                usable_tb: 12.0,
            },
            // equal disks: one tier, as a classic RAID5 / RAID1
            Case {
                disks: &[("sda", 8), ("sdb", 8), ("sdc", 8)],
                tolerance: 1,
                tiers: &[(Some(Raid5), 3)],
                usable_tb: 16.0,
            },
            Case {
                disks: &[("sda", 4), ("sdb", 8)],
                tolerance: 1,
                tiers: &[(Some(Raid1), 2), (None, 1)],
                usable_tb: 4.0,
            },
        ];
        for c in cases {
            let l = plan(&disks(c.disks), c.tolerance, MIN_TIER).unwrap();
            assert_eq!(levels(&l.tiers), c.tiers, "{:?}", c.disks);
            assert_eq!(tb(l.usable()), c.usable_tb, "{:?} tol {}", c.disks, c.tolerance);
            // sum minus the largest (two largest) disks, in the space partitions can use
            let mut reaches: Vec<u64> = disks(c.disks).iter().map(|(_, s)| reach(*s) - PART_START).collect();
            reaches.sort_unstable();
            let biggest: u64 = reaches.iter().rev().take(c.tolerance as usize).sum();
            let unused_by_rule: u64 = l.tiers.iter().filter(|t| !t.created()).map(|t| t.height() * t.members.len() as u64).sum();
            let unused_redundancy: u64 = l.tiers.iter().filter(|t| !t.created()).map(|t| t.height() * c.tolerance.min(t.members.len() as u32) as u64).sum();
            let overhead: u64 = l.created().map(|t| data_members(t.level.unwrap(), t.members.len()) * MD_OVERHEAD).sum();
            assert_eq!(
                l.usable(),
                reaches.iter().sum::<u64>() - biggest - (unused_by_rule - unused_redundancy) - overhead,
                "{:?}",
                c.disks
            );
        }
    }

    #[test]
    fn slices_are_contiguous_nested_and_aligned() {
        let l = plan(&disks(&[("sda", 4), ("sdb", 4), ("sdc", 8), ("sdd", 12)]), 1, MIN_TIER).unwrap();
        assert_eq!(l.tiers[0].start, PART_START);
        for w in l.tiers.windows(2) {
            assert_eq!(w[0].end, w[1].start);
            assert!(w[1].members.iter().all(|m| w[0].members.contains(m)), "nested");
        }
        assert!(l.tiers.iter().all(|t| t.start % MIB == 0 && t.end % MIB == 0));
        assert_eq!(l.tiers[0].end, reach(4 * TB));
        assert_eq!(l.tiers[1].members, ["sdc", "sdd"]);
        // 4 TB of the 12 TB disk stay unused
        assert_eq!(l.unused.len(), 1);
        assert_eq!(l.unused[0].0, "sdd");
        assert_eq!(tb(l.unused[0].1), 4.0);
        assert_eq!(l.tiers.iter().map(|t| t.index).collect::<Vec<_>>(), [1, 2, 3]);
        let api = l.tiers[1].to_api();
        assert_eq!((api.index, api.level, api.usable), (2, Some(Raid1), l.tiers[1].usable()));
    }

    #[test]
    fn two_disk_tolerance_needs_four_disks() {
        assert_eq!(
            plan(&disks(&[("sda", 4), ("sdb", 8)]), 2, MIN_TIER),
            Err(TierError::TooFewDisks { need: 4 })
        );
        assert_eq!(
            plan(&disks(&[("sda", 4), ("sdb", 8), ("sdc", 8)]), 2, MIN_TIER),
            Err(TierError::TooFewDisks { need: 4 })
        );
        assert_eq!(plan(&disks(&[("sda", 4)]), 1, MIN_TIER), Err(TierError::TooFewDisks { need: 2 }));
        assert_eq!(plan(&disks(&[("sda", 4), ("sdb", 4)]), 3, MIN_TIER), Err(TierError::BadTolerance(3)));
    }

    /// "8 TB" disks of different makes differ by a few MB: they make one tier, and the larger
    /// ones' rest is not reported as unused.
    #[test]
    fn near_equal_sizes_merge_into_the_lower_tier() {
        let ds = vec![
            ("sda".to_string(), 8 * TB),
            ("sdb".to_string(), 8 * TB - 5_000_000),
            ("sdc".to_string(), 8 * TB + 10 * (1 << 30)),
        ];
        let l = plan(&ds, 1, MIN_TIER).unwrap();
        assert_eq!(levels(&l.tiers), [(Some(Raid5), 3)]);
        assert_eq!(l.tiers[0].end, reach(8 * TB - 5_000_000));
        assert!(l.unused.is_empty(), "{:?}", l.unused);
        // past MIN_TIER they are separate tiers
        let ds = vec![
            ("sda".to_string(), 8 * TB),
            ("sdb".to_string(), 8 * TB),
            ("sdc".to_string(), 8 * TB + 20 * (1 << 30)),
            ("sdd".to_string(), 8 * TB + 20 * (1 << 30)),
        ];
        let l = plan(&ds, 1, MIN_TIER).unwrap();
        assert_eq!(levels(&l.tiers), [(Some(Raid5), 4), (Some(Raid1), 2)]);
        // a chain of small steps: each merges into the boundary below it until the gap is
        // MIN_TIER wide
        let g = 1u64 << 30;
        let b = boundaries(&[100 * g, 110 * g, 120 * g, 125 * g], 16 * g);
        assert_eq!(b, [100 * g, 120 * g]);
        // loop-test mode: a small MIN_TIER
        let l = plan(&[("loop0".into(), g), ("loop1".into(), 2 * g), ("loop2".into(), 2 * g)], 1, 64 * MIB).unwrap();
        assert_eq!(levels(&l.tiers), [(Some(Raid5), 3), (Some(Raid1), 2)]);
    }

    fn existing(ds: &[(&str, u64)], tolerance: u32) -> (Vec<Tier>, Vec<DiskSize>) {
        let d = disks(ds);
        let l = plan(&d, tolerance, MIN_TIER).unwrap();
        (l.created().cloned().collect(), d)
    }

    #[test]
    fn add_a_bigger_disk() {
        // 4+8+8 + 12: t1 RAID5 3→4, t2 RAID1→RAID5(3), 8–12 left unpartitioned
        let (tiers, d) = existing(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1);
        assert_eq!(tb(tiers.iter().map(Tier::usable).sum()), 12.0);
        let g = grow(&tiers, &d, &disks(&[("sdd", 12)]), 1, MIN_TIER);
        assert_eq!(g.joins, [("sdd".to_string(), 1), ("sdd".to_string(), 2)]);
        assert_eq!(levels(&g.tiers), [(Some(Raid5), 4), (Some(Raid5), 3)]);
        assert_eq!(
            g.level_changes,
            [LevelChange { index: 2, from: Raid1, to: Raid5 }]
        );
        assert!(g.created.is_empty() && g.too_small.is_empty() && g.replaced.is_empty());
        assert_eq!(tb(g.usable()), 20.0);
        assert_eq!(g.unused.len(), 1);
        assert_eq!((g.unused[0].0.as_str(), tb(g.unused[0].1)), ("sdd", 4.0));
        // existing slices never move
        assert_eq!(g.tiers[0].end, tiers[0].end);
        assert_eq!(g.tiers[1].end, tiers[1].end);
    }

    #[test]
    fn a_new_top_tier_needs_enough_disks() {
        // two 12s added to 4+8+8: they reach past the top (8) together → t3 RAID1 of 4 TB
        let (tiers, d) = existing(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1);
        let g = grow(&tiers, &d, &disks(&[("sdd", 12), ("sde", 16)]), 1, MIN_TIER);
        assert_eq!(g.created, [3]);
        let t3 = &g.tiers[2];
        assert_eq!((t3.level, t3.members.clone()), (Some(Raid1), vec!["sdd".to_string(), "sde".to_string()]));
        assert_eq!(t3.start, tiers[1].end);
        assert_eq!(t3.end, reach(12 * TB));
        // then the 16 alone is past t3: not enough disks for another tier
        assert_eq!(g.unused.iter().map(|(n, b)| (n.as_str(), tb(*b))).collect::<Vec<_>>(), [("sde", 4.0)]);
        // 2-disk tolerance needs three disks past the top: 4+4+8+8 (t1 RAID6 only) + 12
        let (tiers, d) = existing(&[("sda", 4), ("sdb", 4), ("sdc", 8), ("sdd", 8)], 2);
        assert_eq!(tiers.len(), 1);
        let g = grow(&tiers, &d, &disks(&[("sde", 12)]), 2, MIN_TIER);
        assert_eq!(g.created, [2]);
        assert_eq!(levels(&g.tiers), [(Some(Raid6), 5), (Some(Raid1), 3)]);
        assert_eq!(g.tiers[1].height(), reach(8 * TB) - reach(4 * TB));
        assert_eq!(tb(g.usable()), 16.0);
    }

    #[test]
    fn three_way_mirror_to_raid6_reduces_tolerance_for_a_while() {
        // 4+8+8+8 at 2-disk tolerance: t2 is a three-way RAID1; a fourth 8 makes it RAID6
        let (tiers, d) = existing(&[("sda", 4), ("sdb", 8), ("sdc", 8), ("sdd", 8)], 2);
        let g = grow(&tiers, &d, &disks(&[("sde", 8)]), 2, MIN_TIER);
        assert_eq!(levels(&g.tiers), [(Some(Raid6), 5), (Some(Raid6), 4)]);
        assert_eq!(g.level_changes.len(), 1);
        assert!(g.level_changes[0].reduces_tolerance());
        assert_eq!(tb(g.usable()), 20.0);
    }

    #[test]
    fn a_disk_smaller_than_the_lowest_tier_cannot_join() {
        let (tiers, d) = existing(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1);
        let g = grow(&tiers, &d, &disks(&[("sdd", 2)]), 1, MIN_TIER);
        assert_eq!(g.too_small, ["sdd"]);
        assert!(g.joins.is_empty());
        assert_eq!(g.tiers, tiers);
        // a 6 joins t1 only: its last 2 TB stay unused
        let g = grow(&tiers, &d, &disks(&[("sdd", 6)]), 1, MIN_TIER);
        assert_eq!(g.joins, [("sdd".to_string(), 1)]);
        assert_eq!(g.unused.iter().map(|(n, b)| (n.as_str(), tb(*b))).collect::<Vec<_>>(), [("sdd", 2.0)]);
    }

    #[test]
    fn replace_with_a_bigger_disk() {
        // the 4 of 4+8+8 replaced by a 12: hot replace in t1, the new disk joins t2
        let (tiers, d) = existing(&[("sda", 4), ("sdb", 8), ("sdc", 8)], 1);
        let g = replace(&tiers, &d, "sda", &("sdd".into(), 12 * TB), 1, MIN_TIER).unwrap();
        assert_eq!(g.replaced, [1]);
        assert_eq!(g.joins, [("sdd".to_string(), 2)]);
        assert_eq!(levels(&g.tiers), [(Some(Raid5), 3), (Some(Raid5), 3)]);
        assert_eq!(g.level_changes, [LevelChange { index: 2, from: Raid1, to: Raid5 }]);
        assert!(g.created.is_empty());
        assert_eq!(tb(g.usable()), 16.0);
        assert!(g.tiers.iter().all(|t| !t.members.contains(&"sda".to_string())));
        assert_eq!(g.unused.iter().map(|(n, b)| (n.as_str(), tb(*b))).collect::<Vec<_>>(), [("sdd", 4.0)]);
        // a failed 8 replaced by a 6 is refused: it must reach the top of t2
        let e = replace(&tiers, &d, "sdb", &("sdd".into(), 6 * TB), 1, MIN_TIER);
        assert_eq!(e, Err(TierError::TooSmall { disk: "sdd".into(), need: tiers[1].end }));
        assert_eq!(needed_reach(&tiers, "sdb"), tiers[1].end);
        // the same size: a plain replacement, nothing else changes
        let g = replace(&tiers, &d, "sdb", &("sdd".into(), 8 * TB), 1, MIN_TIER).unwrap();
        assert_eq!(g.replaced, [1, 2]);
        assert!(g.joins.is_empty() && g.level_changes.is_empty());
        assert_eq!(tb(g.usable()), 12.0);
        assert_eq!(
            replace(&tiers, &d, "sdx", &("sdd".into(), 8 * TB), 1, MIN_TIER),
            Err(TierError::NotAMember("sdx".into()))
        );
    }
}
