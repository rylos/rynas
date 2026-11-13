//! Where a bare-metal restore takes its data from. Both PC backup methods are first-class
//! sources behind one trait:
//! - [`super::restic::ResticSource`]: the file method; it **fills mounted filesystems** with
//!   the files of a version;
//! - [`super::btrfs::BtrfsSource`]: the btrfs method (btrfs snapshots received by the NAS,
//!   `docs/research/pc-backup.md` §11): it **fills a btrfs pool** with `btrfs receive` of send
//!   streams streamed over HTTPS, then writable snapshots at the subvolume paths
//!   ([`RestoreSource::fill_pool`], [`Fill::BtrfsPool`]).
//!
//! The rest is shared and lives in [`super::exec`]: GPT, LUKS, mkfs with the original IDs,
//! mounts, ID fixes, the chroot that writes `/etc/kernel/cmdline` and reinstalls the boot
//! loader, the report.

use std::path::{Path, PathBuf};

use rynas_api_types::pcagent::{MANIFEST_FORMAT, MAX_MANIFEST, Manifest, Subvolume};

use super::exec::Log;
use super::paths::safe_rel;

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

/// The version `want` (`latest`: the newest) of `versions` (newest first); `prefix_ok`: a
/// restic id may be shortened.
pub fn pick_version(versions: &[Version], want: &str, prefix_ok: bool) -> Option<Version> {
    if want == "latest" {
        return versions.first().cloned();
    }
    versions.iter().find(|v| v.id == want || (prefix_ok && v.id.starts_with(want))).cloned()
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
    /// The manifest's UUID of the filesystem (the PC's, also when the new one differs).
    pub uuid: String,
    pub top_level: PathBuf,
    /// The manifest's subvolumes of this filesystem: the source receives the chosen ones,
    /// then the engine makes the others empty ([`super::exec::finish_pool`]).
    pub subvolumes: Vec<Subvolume>,
}

/// Progress of the data step: (done, total) bytes.
pub type Progress<'a> = &'a (dyn Fn(u64, u64) + Sync);

/// A manifest as read from a backup: at most [`MAX_MANIFEST`] bytes, of the format this
/// agent reads, every path in it plain ([`check_paths`]). `what` names it in errors.
pub fn parse_manifest(b: &[u8], what: &str) -> Result<Manifest, String> {
    if b.len() > MAX_MANIFEST {
        return Err(format!("{what} is too large"));
    }
    let m: Manifest = serde_json::from_slice(b).map_err(|e| format!("{what} does not parse: {e}"))?;
    if m.format != MANIFEST_FORMAT {
        return Err(format!("{what} has format {}, this agent reads {MANIFEST_FORMAT}", m.format));
    }
    check_paths(&m)?;
    Ok(m)
}

/// Every path of the manifest the restore turns into a place on the target is a plain path
/// (no `..`, no NUL): subvolumes and their mount points, the snapshot folder, the boot
/// copies, the ESP, swap files. A manifest comes from the PC: nothing in it may lead the
/// restore out of the target.
pub fn check_paths(m: &Manifest) -> Result<(), String> {
    let plain = |what: &str, p: &str| safe_rel(p).map(|_| ()).ok_or_else(|| format!("the manifest's {what} {p:?} is not a plain path"));
    for f in &m.filesystems {
        for s in &f.subvolumes {
            plain("subvolume", &s.path)?;
        }
        for me in &f.mounts {
            plain("mount point", &me.path)?;
            if let Some(s) = &me.subvol {
                plain("subvolume", s)?;
            }
        }
        if let Some(d) = &f.default_subvol {
            plain("default subvolume", d)?;
        }
    }
    if let Some(b) = &m.btrbk {
        plain("snapshot folder", &b.snapshot_dir)?;
        for s in &b.subvolumes {
            plain("subvolume", &s.path)?;
            plain("mount point", &s.mount)?;
            safe_rel(&s.name).filter(|n| !n.is_empty() && !n.contains('/')).ok_or_else(|| format!("the manifest's subvolume name {:?} is not a name", s.name))?;
        }
        for c in &b.boot_copies {
            plain("boot copy", &c.dir)?;
            plain("boot mount point", &c.mount)?;
        }
    }
    if let Some(e) = &m.boot.esp {
        plain("ESP", e)?;
    }
    for s in m.swap.iter().filter(|s| s.file) {
        plain("swap file", &s.path)?;
    }
    Ok(())
}

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
    /// [`Fill::BtrfsPool`]: the subvolumes of `v` into these filesystems; what it does goes
    /// to `log`.
    fn fill_pool(&mut self, _v: &Version, _pools: &[PoolTarget], _p: Progress, _log: &mut Log) -> Result<(), String> {
        Err(format!("the {} source does not fill btrfs filesystems", self.kind()))
    }
    /// The outcome, for the PC's history on the NAS (best effort).
    fn report(&mut self, v: &Version, ok: bool, message: &str, log: &str);
    /// Subvolumes (paths from the top level) this restore leaves out on purpose: not created,
    /// not mounted, their fstab lines commented ([`super::plan::RESTORE_MARK`]).
    fn skipped_subvolumes(&self) -> Vec<String> {
        Vec::new()
    }
    /// Boot folder copies inside the restored tree to lay on their mount points before the
    /// boot step (a [`Fill::BtrfsPool`] source has no ESP files otherwise): (folder in the
    /// target, mount point).
    fn boot_copies(&self, _m: &Manifest) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Lines for the plan: what this source restores and from which snapshots.
    fn describe(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::pcagent::*;

    #[test]
    fn manifest_paths_must_be_plain() {
        let mut m = super::super::plan::tests::sample();
        assert!(check_paths(&m).is_ok());
        m.btrbk = Some(BtrbkLayout { snapshot_dir: "/.btrbk_snapshots".into(), subvolumes: vec![BtrbkSubvolume { name: "@".into(), path: "@".into(), mount: "/".into(), ..Default::default() }], ..Default::default() });
        assert!(check_paths(&m).is_ok());
        let mut bad = m.clone();
        bad.btrbk.as_mut().unwrap().snapshot_dir = "/../../root".into();
        assert!(check_paths(&bad).is_err());
        let mut bad = m.clone();
        bad.btrbk.as_mut().unwrap().subvolumes[0].path = "@/../..".into();
        assert!(check_paths(&bad).is_err());
        let mut bad = m.clone();
        bad.btrbk.as_mut().unwrap().subvolumes[0].name = "a/b".into();
        assert!(check_paths(&bad).is_err());
        let mut bad = m.clone();
        bad.filesystems[0].mounts[0].subvol = Some("../x".into());
        assert!(check_paths(&bad).is_err());
        let json = serde_json::to_vec(&m).unwrap();
        assert!(parse_manifest(&json, "m").is_ok());
        let mut old = m.clone();
        old.format = 99;
        assert!(parse_manifest(&serde_json::to_vec(&old).unwrap(), "m").unwrap_err().contains("format 99"));
    }

    #[test]
    fn versions_by_id() {
        let v = |id: &str| Version { id: id.into(), time_ms: 0, hostname: String::new(), bytes: 0, files: 0 };
        let all = vec![v("abcdef12"), v("20261109T0300")];
        assert_eq!(pick_version(&all, "latest", false).unwrap().id, "abcdef12");
        assert_eq!(pick_version(&all, "abcd", true).unwrap().id, "abcdef12");
        assert!(pick_version(&all, "abcd", false).is_none());
        assert_eq!(pick_version(&all, "20261109T0300", false).unwrap().id, "20261109T0300");
    }
}
