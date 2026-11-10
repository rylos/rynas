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
pub mod health_ack;
pub mod host;
pub mod inventory;
pub mod lsblk;
pub mod luks;
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

use rynas_api_types::storage::{BtrfsFilesystem, DiskSummary, MdArray};

/// A consistent snapshot of the storage state, the input of the planner and the tree.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub graph: lsblk::BlockGraph,
    pub disks: Vec<DiskSummary>,
    pub arrays: Vec<MdArray>,
    pub filesystems: Vec<BtrfsFilesystem>,
    pub mounts: Vec<mounts::Mount>,
}
