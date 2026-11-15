//! rynas storage, read side and planning: disk inventory and SMART health, md arrays, btrfs
//! filesystems, LUKS layers, the per-volume tree, and a planner that returns the exact
//! commands a change would run (without running them).
//!
//! Used by `rynasd` (root: smartctl, mdadm and btrfs need it); rynas-web uses the unprivileged
//! [`overview`] (sysfs and procfs only) for the dashboard. Rules:
//! - **never wake a sleeping disk**: power state comes from ATA CHECK POWER MODE; smartctl
//!   always runs with `-n standby`; `mdadm --detail` and the btrfs tree walks run only when
//!   no disk under them is in standby, otherwise the last answer is served as `stale`;
//! - **external commands through [`host::Host`]**, which runs them with the one command
//!   runner of rynas (`rynas_tasks::cmd::Cmd`): argv only, never a shell, timeout with the
//!   process group killed, clean environment, output capped;
//! - **parsers are pure** and tested on fixture outputs of the Debian 13 tools
//!   (smartmontools 7.4, mdadm 4.4, btrfs-progs 6.14, util-linux 2.41).

pub mod btrfs;
pub mod domain;
pub mod exec;
pub mod flex;
pub mod health_ack;
pub mod host;
pub mod inventory;
pub mod lsblk;
pub mod luks;
pub mod lvm;
pub mod md;
pub mod mounts;
pub mod overview;
pub mod plan;
pub mod service;
pub mod smart;
pub mod subvol;
pub mod tuning;

#[cfg(test)]
pub(crate) mod tests_support;

#[cfg(test)]
mod exec_tests;

pub use service::Storage;

use rynas_api_types::storage::{BtrfsFilesystem, DiskSummary, FlexLayout, MdArray, VolumeRecord};

/// A consistent snapshot of the storage state, the input of the planner and the tree.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub graph: lsblk::BlockGraph,
    pub disks: Vec<DiskSummary>,
    pub arrays: Vec<MdArray>,
    pub filesystems: Vec<BtrfsFilesystem>,
    pub mounts: Vec<mounts::Mount>,
    /// LVM facts of the logical volumes built on md arrays (flexible RAID candidates).
    pub lvm: lvm::LvmReport,
    /// Physical volumes (kernel names) whose LVM facts are old or missing: a disk under them
    /// sleeps, or LVM could not be read.
    pub lvm_stale: Vec<String>,
    /// The flexible RAID layouts found ([`flex::detect`]).
    pub flex: Vec<FlexLayout>,
    /// The volumes rynas recorded (configuration key `storage.volumes`); `None` when the
    /// caller did not say (the checks against the records are skipped).
    pub records: Option<Vec<VolumeRecord>>,
    /// Thinnest flexible RAID tier; 0 means [`flex::tiers::MIN_TIER`] (see
    /// [`Inventory::min_tier`]).
    pub min_tier: u64,
    /// `lvm` is not installed: no new flexible RAID volume can be made.
    pub lvm_missing: bool,
}

impl Inventory {
    /// Thinnest flexible RAID tier for this inventory (smaller on loop files).
    pub fn min_tier(&self) -> u64 {
        if self.min_tier == 0 {
            flex::tiers::MIN_TIER
        } else {
            self.min_tier
        }
    }

    /// The record of a volume, when the records are known.
    pub fn record(&self, label: &str) -> Option<&VolumeRecord> {
        self.records.as_ref()?.iter().find(|r| r.label == label)
    }
}
