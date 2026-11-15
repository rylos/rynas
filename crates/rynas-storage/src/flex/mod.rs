//! Flexible RAID: disks of different sizes cut into tiers, one md array per tier, the tiers
//! joined by one LVM volume group with one logical volume, the filesystem on top (optionally
//! through LUKS).
//!
//! - [`tiers`]: the pure tier calculator (layout of a new volume, growth, replacement);
//! - [`detect`]: recognizes a tiered layout in the inventory (read side);
//! - [`plan`]: the plans that create, grow, repair and complete a flexible volume.

pub mod detect;
pub mod plan;
pub mod tiers;
