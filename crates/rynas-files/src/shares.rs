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
}

impl Share {
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
    let name = parts.next().ok_or_else(|| ApiError::invalid("path", "a path starts with a shared folder"))?;
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
