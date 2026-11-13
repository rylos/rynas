//! The shared folders, and the virtual paths the interface uses: the first component names
//! a share ("/Documents/2026/invoice.pdf"), the rest is a path inside it. "home" is the
//! user's own home folder.
//!
//! The list itself comes from rynasd (the applied `shares` configuration and the user's home,
//! see [`crate::Context`]); this module only checks paths against it. A path never leaves
//! its share through `..` or `.`: every component must be a plain name. Symbolic links are
//! followed only while they stay inside their share, as Samba does with `wide links = no`
//! ([`crate::confine`]): [`Resolved::target`] and [`Resolved::entry`] give the real path.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::confine;
use crate::wire::ApiError;

pub const HOME: &str = "home";
pub const RECYCLE: &str = "#recycle";
/// The folder of the apps' data (administrators, when the Docker setting is on): one
/// folder per stack below it, each a share named `@apps/<folder>`. A share name never
/// starts with `@` (SMB names start with a letter or digit), so nothing clashes.
pub const APPS: &str = "@apps";
/// The folder of the PCs' backups (administrators): one folder per PC whose received btrfs
/// snapshots rynas keeps, each a read-only share named `@backups/<pc>`.
pub const BACKUPS: &str = "@backups";
/// The staging folder rynas receives a PC's snapshot into (rynasd's
/// `pc_backup::STAGING_DIR`): its own and transient, not there for the file manager by any
/// path (`confine::set_guard`).
pub const BACKUP_STAGING: &str = ".incoming";

/// What an app data share belongs to.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AppFolder {
    /// The stack (compose project) whose folder this is.
    pub stack: String,
    /// The catalogue app it was installed from, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
}

/// The virtual folders whose shares are named by two components (`@apps/<folder>`,
/// `@backups/<pc>`): the folder itself only lists them.
pub const GROUPS: [&str; 2] = [APPS, BACKUPS];

/// The group folder (one of [`GROUPS`]) a virtual path starts with, and whether the path
/// goes on below it.
fn group_of(vpath: &str) -> Option<(&'static str, bool)> {
    let mut parts = vpath.split('/').filter(|p| !p.is_empty());
    let first = parts.next()?;
    let group = GROUPS.into_iter().find(|g| *g == first)?;
    Some((group, parts.next().is_some()))
}

/// The group folder a virtual path names itself (`/@apps`, `/@backups`), if it does.
pub fn group_root(vpath: &str) -> Option<&'static str> {
    group_of(vpath).filter(|(_, below)| !below).map(|(g, _)| g)
}

/// Whether a virtual path is the app data folder itself (`/@apps`).
pub fn is_apps_root(vpath: &str) -> bool {
    group_of(vpath) == Some((APPS, false))
}

/// Whether a virtual path lies in an app's data folder (`/@apps/<folder>/…`).
pub fn in_apps(vpath: &str) -> bool {
    group_of(vpath) == Some((APPS, true))
}

/// Whether a virtual path is the backups folder itself (`/@backups`).
pub fn is_backups_root(vpath: &str) -> bool {
    group_of(vpath) == Some((BACKUPS, false))
}

/// The name of a group's share below its group folder: `immich` for `@apps/immich`.
pub fn member_name<'a>(share: &'a Share, group: &str) -> Option<&'a str> {
    share.name.strip_prefix(group)?.strip_prefix('/')
}

/// A shared folder as the worker sees it. `path` is the real directory; it never goes to
/// the interface (see [`Share::public`]).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Share {
    pub name: String,
    pub path: PathBuf,
    /// The volume it lives on, as rynas storage names it (mount point), "" when unknown.
    #[serde(default)]
    pub volume: String,
    #[serde(default)]
    pub comment: String,
    /// Deleting moves items into `#recycle` (the share's recycle bin is on).
    #[serde(default)]
    pub recycle: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub encrypted: bool,
    /// The user's own home folder ("home").
    #[serde(default)]
    pub home: bool,
    /// On a removable device (USB, SD, eSATA): it can be ejected.
    #[serde(default)]
    pub removable: bool,
    /// The disk a removable share is on (`sdc`): shares of one disk are ejected together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// The disk's vendor and model, as the interface names it ("SanDisk Ultra").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    /// An app's data folder ([`APPS`]): the stack it belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppFolder>,
    /// Nothing may be written here, whoever asks: every writing route and every job that
    /// would write or remove something in it is refused ([`Share::refuse_writes`]), and
    /// listings say so (`writable: false`, `read_only: true`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// A PC's backup folder ([`BACKUPS`]): the PC's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    /// A shared folder that holds a PC's backup folder, or lies inside one (an install made
    /// before rynas refused it): the PC's name. The share is then read-only in the file
    /// manager for administrators, until the backups move to a folder of their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holds_backup: Option<String>,
}

impl Share {
    /// Refused with `read_only` when nothing may be written in this share.
    pub fn refuse_writes(&self) -> Result<(), ApiError> {
        if !self.read_only {
            return Ok(());
        }
        Err(ApiError::read_only(if self.backup.is_some() {
            t!("PC backups are read-only: copy the files to a shared folder to change them")
        } else if let Some(pc) = &self.holds_backup {
            t!("this shared folder overlaps the backup folder of {pc}: it is read-only here until the PC's backups move to a folder of their own")
        } else {
            t!("this folder is read-only")
        }))
    }

    /// What the interface gets: everything but the real path.
    pub fn public(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.remove("path");
        }
        v
    }
}

/// A path inside a share, checked: no "..", no ".", nothing absolute
/// below the share name.
pub struct Resolved<'a> {
    pub share: &'a Share,
    pub rel: PathBuf,
    pub real: PathBuf,
}

impl Resolved<'_> {
    pub fn is_root(&self) -> bool {
        self.rel.as_os_str().is_empty()
    }

    /// The share root, opened and admitted to the worker's [`confine::Jail`].
    pub fn root(&self) -> Result<Arc<confine::Root>, ApiError> {
        Ok(confine::admit(Arc::new(confine::Root::open(&self.share.path)?)))
    }

    /// The real path of what the path names, every link followed: refused with
    /// `outside_share` when a link leads out of the share.
    pub fn target(&self) -> Result<PathBuf, ApiError> {
        Ok(self.root()?.resolve(&self.rel, true)?)
    }

    /// The real path of the item itself: the folders on the way followed (inside the share),
    /// the last component not (a link stays the link: renamed, deleted, copied as a link).
    pub fn entry(&self) -> Result<PathBuf, ApiError> {
        Ok(self.root()?.resolve(&self.rel, false)?)
    }
}

pub fn resolve<'a>(shares: &'a [Share], vpath: &str) -> Result<Resolved<'a>, ApiError> {
    if vpath.contains('\0') {
        return Err(ApiError::invalid("path", "invalid path"));
    }
    let mut parts = vpath.split('/').filter(|p| !p.is_empty());
    let first = parts.next().ok_or_else(|| ApiError::invalid("path", "a path starts with a shared folder"))?;
    // an app's folder and a PC's backup are named by two components: `@apps/<folder>`
    let name = match GROUPS.into_iter().find(|g| *g == first) {
        Some(group) => {
            let member = parts.next().ok_or_else(|| {
                ApiError::read_only(if group == APPS { t!("the app data folder only holds the apps' folders") } else { t!("the backups folder only holds the PCs' folders") })
            })?;
            format!("{group}/{member}")
        }
        None => first.to_string(),
    };
    let share = shares
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| ApiError::not_found(t!("no shared folder {}", name)))?;
    let mut rel = PathBuf::new();
    for part in parts {
        match Path::new(part).components().next() {
            Some(Component::Normal(_)) if Path::new(part).components().count() == 1 => rel.push(part),
            _ => return Err(ApiError::invalid("path", "invalid path")),
        }
    }
    let real = share.path.join(&rel);
    Ok(Resolved { share, rel, real })
}

/// Checks that a file name is one component, not empty, not "." or "..".
pub fn valid_name(name: &str) -> Result<(), ApiError> {
    let name_trim = name.trim();
    if name_trim.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(ApiError::invalid("name", t!("invalid name")));
    }
    if name.len() > 255 {
        return Err(ApiError::invalid("name", t!("name too long (255 bytes at most)")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::error_codes;

    fn shares() -> Vec<Share> {
        vec![
            Share { name: "Documents".into(), path: "/srv/vol1/Documents".into(), recycle: true, ..Default::default() },
            Share { name: "docker".into(), path: "/srv/vol2/docker".into(), hidden: true, ..Default::default() },
        ]
    }

    #[test]
    fn resolves_inside_shares() {
        let shares = shares();
        let r = resolve(&shares, "/Documents/2026/a b.pdf").unwrap();
        assert_eq!(r.real, PathBuf::from("/srv/vol1/Documents/2026/a b.pdf"));
        assert_eq!(r.rel, PathBuf::from("2026/a b.pdf"));
        assert!(resolve(&shares, "/Documents").unwrap().is_root());
        assert!(resolve(&shares, "//Documents//x/").is_ok());
        let bad = resolve(&shares, "/Documents/../etc").err().unwrap();
        assert_eq!((bad.status, bad.code), (400, error_codes::INVALID_VALUE));
        assert_eq!(resolve(&shares, "/Documents/./x").err().unwrap().status, 400);
        assert_eq!(resolve(&shares, "/Documents/a/../../x").err().unwrap().status, 400);
        assert_eq!(resolve(&shares, "/Documents/a\0b").err().unwrap().status, 400);
        assert_eq!(resolve(&shares, "/Nope/x").err().unwrap().status, 404);
        // a share name is matched exactly, never as a path
        assert_eq!(resolve(&shares, "/../srv").err().unwrap().status, 404);
        assert_eq!(resolve(&shares, "/").err().unwrap().status, 400);
    }

    #[test]
    fn app_folders_are_named_by_two_components() {
        let mut list = shares();
        list.push(Share { name: format!("{APPS}/immich"), path: "/srv/vol1/docker/stacks/immich".into(), app: Some(AppFolder { stack: "immich".into(), app: None }), ..Default::default() });
        let r = resolve(&list, "/@apps/immich/library/a.jpg").unwrap();
        assert_eq!(r.real, PathBuf::from("/srv/vol1/docker/stacks/immich/library/a.jpg"));
        assert!(resolve(&list, "/@apps/immich").unwrap().is_root());
        // the folder of the apps itself is no share: nothing is made or read there
        assert_eq!(resolve(&list, "/@apps").err().unwrap().code, error_codes::READ_ONLY);
        assert_eq!(resolve(&list, "/@apps/nope/x").err().unwrap().status, 404);
        assert_eq!(resolve(&list, "/@apps/immich/../../etc").err().unwrap().status, 400);
        // the full name is never a first component
        assert_eq!(resolve(&list, "/@apps%2Fimmich").err().unwrap().status, 404);
        assert!(is_apps_root("/@apps/") && !is_apps_root("/@apps/immich"));
        assert!(in_apps("/@apps/immich/x") && in_apps("//@apps/immich") && !in_apps("/@apps") && !in_apps("/Documents/@apps/x"));
    }

    #[test]
    fn backup_folders_are_named_by_two_components_and_read_only() {
        let mut list = shares();
        list.push(Share { name: format!("{BACKUPS}/laptop"), path: "/srv/vol1/pc-backups/laptop".into(), read_only: true, backup: Some("laptop".into()), ..Default::default() });
        let r = resolve(&list, "/@backups/laptop/@home.20261113T1543/a.txt").unwrap();
        assert_eq!(r.real, PathBuf::from("/srv/vol1/pc-backups/laptop/@home.20261113T1543/a.txt"));
        assert!(resolve(&list, "/@backups/laptop").unwrap().is_root());
        assert_eq!(resolve(&list, "/@backups").err().unwrap().code, error_codes::READ_ONLY);
        assert_eq!(resolve(&list, "/@backups/nope/x").err().unwrap().status, 404);
        assert_eq!(resolve(&list, "/@backups/laptop/../x").err().unwrap().status, 400);
        assert!(is_backups_root("/@backups/") && !is_backups_root("/@backups/laptop"));
        assert!(!in_apps("/@backups/laptop/x") && !is_backups_root("/Documents/@backups"));
        assert_eq!(member_name(r.share, BACKUPS), Some("laptop"));
        // writes refused in the backup, allowed in a share
        assert_eq!(r.share.refuse_writes().unwrap_err().code, error_codes::READ_ONLY);
        assert!(list[0].refuse_writes().is_ok());
        // the interface learns it is read-only, never where it is
        let v = r.share.public();
        assert_eq!((v["read_only"].as_bool(), v["backup"].as_str()), (Some(true), Some("laptop")));
        assert!(v.get("path").is_none());
        assert!(list[0].public().get("read_only").is_none());
    }


    #[test]
    fn checks_names() {
        assert!(valid_name("invoice 2026.pdf").is_ok());
        assert!(valid_name("..").is_err());
        assert!(valid_name(".").is_err());
        assert!(valid_name("a/b").is_err());
        assert!(valid_name("  ").is_err());
        assert_eq!(valid_name(&"x".repeat(256)).unwrap_err().field.as_deref(), Some("name"));
    }

    #[test]
    fn public_view_has_no_path() {
        let v = shares()[0].public();
        assert_eq!(v["name"], "Documents");
        assert!(v.get("path").is_none());
    }
}
