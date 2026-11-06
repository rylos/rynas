//! The Synology module (`rynas_module::Module`, D-MODULE): what rynasd gets on a Synology
//! running synodeb, and only there.
//!
//! Two independent signals, read at the start of rynasd:
//! - the hardware ([`Synology::detect`]: platform driver bound, DMI): the `synology.hw`
//!   configuration domain and the boot flash's USB ids (the DOM, `f400:f400`, never offered
//!   as a removable drive; synodeb's udev rule also marks it `UDISKS_IGNORE=1`, which the core
//!   honours on any machine, so the ids only matter without that rule);
//! - synodeb (its own files `/etc/synodeb/ups.conf` or `/etc/synodeb/notify.conf` below the
//!   configuration root): the UPS layer ([`crate::ups`], when `ups.conf` exists), the `notify`
//!   forward and the one-time import of `notify.conf` ([`crate::notify`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rynas_config::Applier;
use rynas_module::{EventForward, Module, UpsLayer};

use crate::Synology;

/// USB ids of the Synology boot flash (DOM).
pub const BOOT_FLASH_IDS: &[(&str, &str)] = &[("f400", "f400")];
/// The pre-Safe Mode hook the package ships (`packaging/modules/synology/ups-pre-safemode`).
pub const UPS_HOOK: &str = "/usr/lib/rynas/modules/synology/ups-pre-safemode";
/// Its link in synodeb's hook folder.
pub const UPS_HOOK_LINK: &str = "/etc/synodeb/ups-pre-safemode.d/50-rynas";

/// Links the pre-Safe Mode hook into synodeb's folder below `root` (synodeb's UPS layer only).
/// A file of the admin's with that name is left alone; the hook of rynas 0.1.0 (a regular file
/// calling `rynasctl ups safe-mode`) becomes the link. Returns what it did.
pub fn link_ups_hook(root: &Path) -> Result<Option<&'static str>, String> {
    let link = rynas_config::files::under_root(root, Path::new(UPS_HOOK_LINK));
    match std::fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => {
            if std::fs::read_link(&link).is_ok_and(|t| t == Path::new(UPS_HOOK)) {
                return Ok(None);
            }
            return Ok(Some("a link of another target is in the way: left as it is"));
        }
        Ok(_) => {
            let ours = std::fs::read_to_string(&link)
                .is_ok_and(|t| t.contains("rynasctl") && t.contains("ups safe-mode"));
            if !ours {
                return Ok(Some("a hook of the admin's has the same name: left as it is"));
            }
            std::fs::remove_file(&link).map_err(|e| format!("{UPS_HOOK_LINK}: {e}"))?;
        }
        Err(_) => {}
    }
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::os::unix::fs::symlink(UPS_HOOK, &link).map_err(|e| format!("{UPS_HOOK_LINK}: {e}"))?;
    Ok(Some("linked the pre-Safe Mode hook"))
}

/// Whether synodeb is installed below the configuration root (its files, not the directory:
/// older rynas packages created `/etc/synodeb/ups-pre-safemode.d/` on any machine).
pub fn synodeb_present(root: &Path) -> bool {
    [crate::ups::UPS_CONF, crate::notify::NOTIFY_CONF]
        .iter()
        .any(|f| rynas_config::files::under_root(root, Path::new(f)).is_file())
}

pub struct SynologyModule {
    /// The hardware provider, on Synology hardware.
    pub hardware: Option<Arc<Synology>>,
    /// synodeb is installed.
    pub synodeb: bool,
    /// Configuration root (synodeb's files are below it).
    pub root: PathBuf,
}

impl SynologyModule {
    /// The module when this machine is a Synology or runs synodeb; `None` elsewhere.
    pub fn detect(hw_root: &Path, root: &Path) -> Option<Self> {
        let hardware = Synology::detect(hw_root).map(Arc::new);
        let synodeb = synodeb_present(root);
        (hardware.is_some() || synodeb).then(|| Self {
            hardware,
            synodeb,
            root: root.to_path_buf(),
        })
    }
}

impl Module for SynologyModule {
    fn id(&self) -> &'static str {
        "synology"
    }

    fn ups(&self) -> Option<Arc<dyn UpsLayer>> {
        crate::ups::present(&self.root)
            .then(|| Arc::new(crate::ups::SynodebUps::default()) as Arc<dyn UpsLayer>)
    }

    fn event_forward(&self) -> Option<Arc<dyn EventForward>> {
        self.synodeb
            .then(|| Arc::new(crate::notify::Forward) as Arc<dyn EventForward>)
    }

    fn notify_import(&self) -> Option<rynas_notify::ImportedSettings> {
        crate::notify::imported(&self.root)
    }

    fn system_disk_usb_ids(&self) -> &'static [(&'static str, &'static str)] {
        if self.hardware.is_some() { BOOT_FLASH_IDS } else { &[] }
    }

    fn domains(&self) -> Vec<Arc<dyn Applier>> {
        if self.hardware.is_some() {
            vec![Arc::new(crate::domain::SynologyDomain)]
        } else {
            Vec::new()
        }
    }

    fn start(&self, log: &dyn Fn(&str)) {
        if !crate::ups::present(&self.root) {
            return;
        }
        match link_ups_hook(&self.root) {
            Ok(None) => {}
            Ok(Some(what)) => log(&format!("synodeb UPS hook {UPS_HOOK_LINK}: {what}")),
            Err(e) => log(&format!("synodeb UPS hook: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detected_by_synodeb_files_only() {
        let hw = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        // a plain Debian, even with the directory an older rynas package created
        std::fs::create_dir_all(root.path().join("etc/synodeb/ups-pre-safemode.d")).unwrap();
        assert!(SynologyModule::detect(hw.path(), root.path()).is_none());
        std::fs::write(root.path().join("etc/synodeb/notify.conf"), "MIN_PRIORITY=high\n").unwrap();
        let m = SynologyModule::detect(hw.path(), root.path()).unwrap();
        assert!(m.event_forward().is_some() && m.notify_import().is_some());
        // no UPS layer without synodeb's ups.conf, no hardware parts on other hardware
        assert!(m.ups().is_none() && m.domains().is_empty());
        assert!(m.system_disk_usb_ids().is_empty());
        std::fs::write(root.path().join("etc/synodeb/ups.conf"), "").unwrap();
        assert!(m.ups().is_some());
        assert_eq!(m.notify_import().unwrap().key, "synodeb_import");
    }

    #[test]
    fn ups_hook_linked_once_and_old_hook_replaced() {
        let root = tempfile::tempdir().unwrap();
        let link = root.path().join("etc/synodeb/ups-pre-safemode.d/50-rynas");
        assert_eq!(link_ups_hook(root.path()).unwrap(), Some("linked the pre-Safe Mode hook"));
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new(UPS_HOOK));
        assert_eq!(link_ups_hook(root.path()).unwrap(), None);
        // the conffile of rynas 0.1.0
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "#!/bin/sh\ntimeout 40 \"$CTL\" ups safe-mode \"$reason\"\n# rynasctl\n").unwrap();
        assert!(link_ups_hook(root.path()).unwrap().is_some());
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        // an admin's own hook
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "#!/bin/sh\nmine\n").unwrap();
        assert!(link_ups_hook(root.path()).unwrap().unwrap().contains("admin"));
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "#!/bin/sh\nmine\n");
    }
}
