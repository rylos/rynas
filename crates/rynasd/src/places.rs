//! The volumes something can be put on, the system volume included (D-ROOTVOL).
//!
//! Every mounted rynas volume (`/<label>`, a btrfs top level) and the root filesystem as the
//! volume [`SYSTEM_VOLUME`]: what rynas puts there lives in [`SYSTEM_VOLUME_DIR`]. The folder
//! is made ready on first use by [`ensure_system_dir`] (`rynas_btrfs::sysvol`, the one
//! function every user calls): on a btrfs root the mount of the top-level subvolume
//! `@rynas-system` (unit `srv-rynas-system.mount`), so restore points and rollbacks leave
//! its data alone; on another root a plain folder. Anything else there (a plain folder with
//! files, a symbolic link, a foreign mount) is refused and the place is listed as
//! unavailable with the reason.
//!
//! `GET /v1/storage/places` lists them with their free space ([`VolumePlace`]).

use std::path::{Path, PathBuf};

use axum::{Json, Router, routing::get};
use rynas_api_types::storage::{
    SYSTEM_VOLUME, SYSTEM_VOLUME_DIR, VolumePlace, low_space, volume_dir,
};

/// Root filesystems that are no disk: the system volume is not offered on them (a live
/// system, a container, a network root).
const NOT_A_DISK: &[&str] = &[
    "tmpfs", "ramfs", "rootfs", "overlay", "squashfs", "nfs", "nfs4", "cifs", "smb3", "9p",
    "virtiofs", "fuse",
];

/// The system's mount table as the host sees it ([`rynas_storage::mounts::host_mountinfo`]):
/// rynasd's own namespace adds the bind mounts of its sandbox.
pub fn mountinfo() -> String {
    rynas_storage::mounts::host_mountinfo()
}

pub fn root_fstype(mountinfo: &str) -> Option<String> {
    rynas_btrfs::sysvol::root_fstype(mountinfo)
}

/// The root filesystem is a disk the system volume can be offered on.
pub fn root_is_disk(mountinfo: &str) -> bool {
    root_fstype(mountinfo).is_some_and(|t| !NOT_A_DISK.contains(&t.as_str()) && !t.starts_with("fuse"))
}

/// `(label, folder)` of every place: the mounted rynas volumes, then the system volume
/// when the root filesystem is a disk.
pub fn places_in(mountinfo: &str) -> Vec<(String, String)> {
    let mut v = crate::storage_ops::volumes_in(mountinfo);
    if root_is_disk(mountinfo) {
        v.push((SYSTEM_VOLUME.into(), SYSTEM_VOLUME_DIR.into()));
    }
    v
}

/// [`places_in`] of the running system.
pub fn host_places() -> Vec<(String, String)> {
    places_in(&mountinfo())
}

/// Is `label` a place now (a mounted rynas volume, or the system volume on a disk root)?
pub fn is_place(label: &str, mountinfo: &str) -> bool {
    places_in(mountinfo).iter().any(|(l, _)| l == label)
}

/// The places with their sizes. `root` is the dry or test root the folders are below; the
/// system volume's size is the root filesystem's (its folder may not exist yet).
pub fn describe(places: &[(String, String)], mountinfo: &str, root: &Path) -> Vec<VolumePlace> {
    let mounts = rynas_storage::mounts::parse_mountinfo(mountinfo);
    places
        .iter()
        .map(|(label, path)| {
            let system = label == SYSTEM_VOLUME;
            let mp = if system { "/" } else { path.as_str() };
            let fs_type = mounts
                .iter()
                .rfind(|m| m.mountpoint == mp)
                .map(|m| m.fstype.clone())
                .unwrap_or_default();
            let probe = if root == Path::new("/") {
                PathBuf::from(mp)
            } else {
                rynas_docker::under(root, path)
                    .ancestors()
                    .find(|p| p.is_dir())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.to_path_buf())
            };
            let st = rynas_storage::overview::statvfs(&probe.to_string_lossy());
            VolumePlace {
                label: label.clone(),
                path: path.clone(),
                avail_bytes: st.as_ref().map(|s| s.avail),
                system,
                fs_type,
                size_bytes: st.as_ref().map(|s| s.size),
                low_space: st.as_ref().is_some_and(|s| low_space(s.size, s.avail)),
                unavailable: if system {
                    rynas_btrfs::sysvol::unavailable(root, root == Path::new("/"))
                } else {
                    None
                },
            }
        })
        .collect()
}

/// [`describe`] of the running system's places.
pub fn host_described() -> Vec<VolumePlace> {
    let mi = mountinfo();
    describe(&places_in(&mi), &mi, Path::new("/"))
}

/// Makes the system volume's folder ready below `root` and returns its path there:
/// `rynas_btrfs::sysvol::ensure_system_dir` (`real`: the machine, not a dry run), the one
/// function every user of the system volume calls.
pub use rynas_btrfs::sysvol::ensure_system_dir;

/// The folder of a place label below `root`, made ready when it is the system volume.
/// Refuses a label that is no place now. `real`: the machine, not a dry run.
pub fn ready_dir(label: &str, root: &Path, real: bool, log: &dyn Fn(&str)) -> Result<String, String> {
    if label == SYSTEM_VOLUME {
        if root == Path::new("/") && !root_is_disk(&mountinfo()) {
            return Err("the root filesystem is not a disk: it cannot hold the system volume".into());
        }
        ensure_system_dir(root, real, log)?;
        return Ok(SYSTEM_VOLUME_DIR.into());
    }
    let want = volume_dir(label);
    let ok = if root == Path::new("/") {
        is_place(label, &mountinfo())
    } else {
        rynas_docker::under(root, &want).is_dir()
    };
    if ok {
        Ok(want)
    } else {
        Err(format!("volume {label} is not mounted"))
    }
}

/// `path` is strictly inside the system volume folder.
pub fn in_system_dir(path: &str) -> bool {
    path.strip_prefix(SYSTEM_VOLUME_DIR)
        .is_some_and(|r| r.len() > 1 && r.starts_with('/'))
}

async fn places() -> Json<Vec<VolumePlace>> {
    Json(
        tokio::task::spawn_blocking(host_described)
            .await
            .unwrap_or_default(),
    )
}

/// `GET /v1/storage/places`.
pub fn router() -> Router {
    Router::new().route("/v1/storage/places", get(places))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MI: &str = "22 1 0:21 / / rw - btrfs /dev/md0 rw\n\
                      30 22 0:40 / /volume1 rw - btrfs /dev/sdb1 rw\n\
                      31 22 8:33 / /media/usb1 rw - ext4 /dev/sdc1 rw\n";

    #[test]
    fn the_system_volume_comes_last_and_only_on_a_disk() {
        assert_eq!(
            places_in(MI),
            vec![
                ("volume1".to_string(), "/volume1".to_string()),
                (SYSTEM_VOLUME.into(), SYSTEM_VOLUME_DIR.into())
            ]
        );
        assert!(is_place("system", MI) && is_place("volume1", MI) && !is_place("usb1", MI));
        let overlay = "1 0 0:1 / / rw - overlay overlay rw\n";
        assert!(places_in(overlay).is_empty());
        assert!(!root_is_disk(overlay));
        assert_eq!(root_fstype("1 0 0:1 / / rw - ext4 /dev/sda2 rw\n").as_deref(), Some("ext4"));
    }

    #[test]
    fn the_folder_is_made_below_a_dry_root() {
        let t = tempfile::tempdir().unwrap();
        let d = ensure_system_dir(t.path(), false, &|_| {}).unwrap();
        assert!(d.is_dir() && d.ends_with("srv/rynas/system"));
        // made once
        assert_eq!(ensure_system_dir(t.path(), false, &|_| {}).unwrap(), d);
        assert_eq!(ready_dir("system", t.path(), false, &|_| {}).unwrap(), SYSTEM_VOLUME_DIR);
        assert!(ready_dir("vol9", t.path(), false, &|_| {}).is_err());
        let ds = describe(&[("system".into(), SYSTEM_VOLUME_DIR.into())], MI, t.path());
        assert!(ds[0].system && ds[0].fs_type == "btrfs" && ds[0].size_bytes.is_some());
        assert!(in_system_dir("/srv/rynas/system/backup") && !in_system_dir("/srv/rynas/system"));
        assert!(!in_system_dir("/srv/rynas/systemx/a"));
        assert_eq!(ds[0].unavailable, None);
        // a symbolic link in its place: listed as unavailable with the reason, refused
        std::fs::remove_dir(&d).unwrap();
        std::os::unix::fs::symlink("/", &d).unwrap();
        let ds = describe(&[("system".into(), SYSTEM_VOLUME_DIR.into())], MI, t.path());
        assert!(ds[0].unavailable.as_deref().is_some_and(|w| w.contains("symbolic link")));
        assert!(ready_dir("system", t.path(), false, &|_| {}).is_err());
    }
}
