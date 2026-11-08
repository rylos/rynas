//! Snapshots for rynas: btrbk names and the btrfs side of browsing and restoring them.
//!
//! - [`name`]: btrbk snapshot names (`<prefix>.YYYYMMDDTHHMM[_N]`), parsed and ordered;
//! - [`ioctl`]: subvolume facts, snapshot create/delete, reflinks, on open descriptors;
//! - [`scan`]: the snapshots of a job and the chain-safe delete (under btrbk's lock);
//! - [`browse`]: descriptor-based, read-only walk inside a snapshot;
//! - [`restore`]: file/folder restore (copy next to it, or replace after saving the current
//!   version) and rollback of a whole subvolume by rename;
//! - [`system`]: system restore points before apt (`rynasctl snapshot system`);
//! - [`rootfs`]: rolling the system back to a restore point (`@rootfs` and, through a hardware
//!   module's boot layer, the boot entry of its kernel);
//! - [`btrbk`]: the configuration rynas generates per job and btrbk's raw output;
//! - [`replication`]: replica targets of a job (btrbk `target` sections), transfers, status,
//!   replicas browse and promote; [`btrbk_ssh`]: reusable SSH key and filter helpers;
//! - [`receive`], [`receive_filter`]: PC backup, receive side (btrbk pushes from PCs): folder
//!   summary, NAS-side delete rules, account texts, and the forced command's allowlist;
//! - [`sysvol`]: the system volume's folder (D-ROOTVOL), the top-level subvolume
//!   `@rynas-system` mounted on `/srv/rynas/system` on a btrfs root;
//! - [`domain`]: the `snapshots` configuration domain.
//!
//! Ported in part from btrbk-nas (MIT, same author): `snapshot.rs`, `btrfs.rs`, `browse.rs`.
//! No qgroups: nothing here enables or reads quotas.

pub mod browse;
pub mod btrbk;
pub mod btrbk_ssh;
pub mod domain;
pub mod ioctl;
pub mod name;
pub mod receive;
pub mod receive_filter;
pub mod replication;
pub mod restore;
pub mod rootfs;
pub mod scan;
pub mod system;
pub mod sysvol;

pub use domain::{HostSnapshots, MemorySnapshots, SNAPSHOTS_KEY, SnapshotHost, SnapshotsDomain};

#[cfg(test)]
mod tests;
