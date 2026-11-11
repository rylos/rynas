//! Where a bare-metal restore takes its data from. Both PC backup methods are first-class
//! sources behind one trait:
//! - [`super::restic::ResticSource`]: the file method; it **fills mounted filesystems** with
//!   the files of a version;
//! - the btrfs method (btrfs snapshots received by the NAS, `docs/research/pc-backup.md` §11):
//!   it **fills a btrfs pool** with `btrfs receive` of send streams streamed over HTTPS, then
//!   writable snapshots at the subvolume paths. Not in this build; the trait has its step
//!   ([`RestoreSource::fill_pool`], [`Fill::BtrfsPool`]) so the engine's order of work already
//!   fits it.
//!
//! The rest is shared and lives in [`super::exec`]: GPT, LUKS, mkfs with the original IDs,
//! mounts, ID fixes, the chroot that writes `/etc/kernel/cmdline` and reinstalls the boot
//! loader, the report.

use std::path::{Path, PathBuf};

use rynas_api_types::pcagent::{Manifest, Subvolume};

/// One restorable point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// The source's own id (a restic snapshot id; a btrbk run timestamp).
    pub id: String,
    pub time_ms: u64,
    pub hostname: String,
    /// Bytes of data the restore writes (to check that the target is large enough).
    pub bytes: u64,
    pub files: u64,
}

/// How a source puts the data on the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    /// Files into the filesystems, mounted as fstab says (the engine mounts them first).
    MountedTree,
    /// Subvolumes received into btrfs filesystems mounted at their top level; the engine
    /// mounts the whole tree afterwards.
    BtrfsPool,
}

/// A btrfs filesystem made for a [`Fill::BtrfsPool`] source, mounted at its top level.
#[derive(Debug, Clone)]
pub struct PoolTarget {
    pub uuid: String,
    pub top_level: PathBuf,
    /// The manifest's subvolumes of this filesystem (captured ones are filled, the others
    /// were created empty by the engine); read by the btrfs method's source.
    #[allow(dead_code)]
    pub subvolumes: Vec<Subvolume>,
}

/// Progress of the data step: (done, total) bytes.
pub type Progress<'a> = &'a (dyn Fn(u64, u64) + Sync);

pub trait RestoreSource {
    /// "file backup", "btrfs snapshots".
    fn kind(&self) -> &'static str;
    /// The PC being restored, as the NAS names it.
    fn device(&self) -> &str;
    fn versions(&mut self) -> Result<Vec<Version>, String>;
    /// The layout manifest of a version.
    fn manifest(&mut self, v: &Version) -> Result<Manifest, String>;
    fn fill(&self) -> Fill;
    /// [`Fill::MountedTree`]: the files of `v` below `root`.
    fn fill_tree(&mut self, _v: &Version, _root: &Path, _p: Progress) -> Result<(), String> {
        Err(format!("the {} source does not restore files into a tree", self.kind()))
    }
    /// [`Fill::BtrfsPool`]: the subvolumes of `v` into these filesystems.
    fn fill_pool(&mut self, _v: &Version, _pools: &[PoolTarget], _p: Progress) -> Result<(), String> {
        Err(format!("the {} source does not fill btrfs pools", self.kind()))
    }
    /// The outcome, for the PC's history on the NAS (best effort).
    fn report(&mut self, v: &Version, ok: bool, message: &str, log: &str);
}
