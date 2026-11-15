//! Flexible RAID: disks of different sizes cut into tiers, one md array per tier, the tiers
//! joined by one LVM volume group with one logical volume, the filesystem on top (optionally
//! through LUKS).
//!
//! - [`tiers`]: the pure tier calculator (layout of a new volume, growth, replacement);

pub mod tiers;
