//! The hardware module interface (D-MODULE, `docs/phase0/05-decisions.md`).
//!
//! The core works on any Debian with standard tools only: NUT for the UPS, the standard udev
//! properties for system disks, its own notification engine. A hardware module (Synology
//! today: `rynas-hw-synology`) is detected at start and fills in the extension points below.
//! Everything specific to that hardware or to the system layer that ships with it (synodeb on
//! a Synology) lives in the module; the core never names it. Without a module every extension
//! point is empty and every feature works with the standard tools.
//!
//! Extension points ([`Module`]):
//! - [`Module::ups`]: a [`ups::UpsLayer`] that owns NUT on this machine (its own tool runs the
//!   driver, the shutdown and the pre-shutdown hooks). Without one rynasd's generic NUT backend
//!   writes rynas blocks in `/etc/nut` and follows upsd itself.
//! - [`Module::event_forward`]: the module's own tools post notifications to rynasd on the
//!   local socket; the module parses their body ([`notify::EventForward`]). Without one the
//!   socket is not opened.
//! - [`Module::notify_import`]: the module layer's notification settings, imported once into
//!   targets and a rule.
//! - [`Module::system_disk_usb_ids`]: USB ids of the machine's own boot flash, never offered as
//!   a removable drive (the core always honours `UDISKS_IGNORE` / `UDISKS_SYSTEM`).
//! - [`Module::domains`]: the module's configuration domains.
//! - [`Module::start`]: once at the start of rynasd, the module puts its hooks into its system
//!   layer (files the package ships below `/usr/lib/rynas/modules/<module>/`, never into the
//!   layer's `/etc` from the package: nothing of a module exists on other systems) and adopts
//!   its layer's settings into its configuration keys.
//! - [`Module::router`]: the module's own HTTP routes, nested by rynasd under
//!   `/v1/modules/<id>` ([`http`]); rynas-web forwards `/api/v1/modules/<id>/…` by prefix.
//! - [`Module::hardware_control`]: the actions of the machine's hardware provider behind the
//!   provider-neutral endpoints of the core (bay identify).
//!
//! The web interface has the matching rule: a module's pages, tabs and texts live in the UI's
//! module folder and are shown only where the module answers.

use std::sync::Arc;

use rynas_config::{Applier, Config};
use rynas_hw::HardwareControl;

pub mod http;
pub mod notify;
pub mod ups;

pub use notify::EventForward;
pub use ups::UpsLayer;

/// A hardware module, built in today (a plugin process later, D-PLUG): every method has an
/// empty default, so a module implements only what its hardware has.
pub trait Module: Send + Sync {
    /// Short stable name (`synology`).
    fn id(&self) -> &'static str;

    /// One line for rynasd's start log (what was detected).
    fn summary(&self) -> String {
        self.id().to_string()
    }

    /// The UPS layer of this machine, when the module's system layer runs NUT.
    fn ups(&self) -> Option<Arc<dyn UpsLayer>> {
        None
    }

    /// The parser of notifications the module's tools forward to rynasd.
    fn event_forward(&self) -> Option<Arc<dyn EventForward>> {
        None
    }

    /// Notification settings to import once (read at every start; the engine imports each
    /// [`rynas_notify::ImportedSettings::key`] only the first time).
    fn notify_import(&self) -> Option<rynas_notify::ImportedSettings> {
        None
    }

    /// USB `(vendor, product)` ids of the machine's own boot flash.
    fn system_disk_usb_ids(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// Configuration domains of the module.
    fn domains(&self) -> Vec<Arc<dyn Applier>> {
        Vec::new()
    }

    /// Once at the start of rynasd (blocking, as root), after the configuration store is open:
    /// hooks into the module's system layer, its layer's settings adopted into its keys.
    /// Errors are logged, never fatal.
    fn start(&self, _cfg: &Config, _log: &dyn Fn(&str)) {}

    /// The module's HTTP routes (paths relative to `/v1/modules/<id>`).
    fn router(&self, _ctx: &http::Ctx) -> Option<axum::Router> {
        None
    }

    /// Actions on the machine's hardware (the provider of the module), behind the core's
    /// provider-neutral endpoints.
    fn hardware_control(&self) -> Option<Arc<dyn HardwareControl>> {
        None
    }
}

/// The modules detected on this machine (none on a plain Debian).
#[derive(Clone, Default)]
pub struct Modules(Vec<Arc<dyn Module>>);

impl Modules {
    pub fn new(list: Vec<Arc<dyn Module>>) -> Self {
        Self(list)
    }

    pub fn none() -> Self {
        Self::default()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Module>> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The first module's UPS layer (one machine has one UPS layer).
    pub fn ups(&self) -> Option<Arc<dyn UpsLayer>> {
        self.0.iter().find_map(|m| m.ups())
    }

    /// The first module's event forward.
    pub fn event_forward(&self) -> Option<Arc<dyn EventForward>> {
        self.0.iter().find_map(|m| m.event_forward())
    }

    pub fn notify_imports(&self) -> Vec<rynas_notify::ImportedSettings> {
        self.0.iter().filter_map(|m| m.notify_import()).collect()
    }

    pub fn system_disk_usb_ids(&self) -> Vec<(String, String)> {
        self.0
            .iter()
            .flat_map(|m| m.system_disk_usb_ids().iter())
            .map(|(v, p)| (v.to_string(), p.to_string()))
            .collect()
    }

    pub fn domains(&self) -> Vec<Arc<dyn Applier>> {
        self.0.iter().flat_map(|m| m.domains()).collect()
    }

    pub fn start(&self, cfg: &Config, log: &dyn Fn(&str)) {
        for m in &self.0 {
            m.start(cfg, log);
        }
    }

    /// Every module's routes, each nested under `/v1/modules/<id>`.
    pub fn router(&self, ctx: &http::Ctx) -> axum::Router {
        self.0.iter().fold(axum::Router::new(), |r, m| match m.router(ctx) {
            Some(mr) => r.nest(&format!("/v1/modules/{}", m.id()), mr),
            None => r,
        })
    }

    /// The first module's hardware control.
    pub fn hardware_control(&self) -> Option<Arc<dyn HardwareControl>> {
        self.0.iter().find_map(|m| m.hardware_control())
    }

    /// The ids of the detected modules.
    pub fn ids(&self) -> Vec<&'static str> {
        self.0.iter().map(|m| m.id()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Bare;
    impl Module for Bare {
        fn id(&self) -> &'static str {
            "bare"
        }
    }

    #[test]
    fn empty_extension_points_by_default() {
        let m = Modules::new(vec![Arc::new(Bare)]);
        assert!(m.ups().is_none() && m.event_forward().is_none());
        assert!(m.notify_imports().is_empty() && m.system_disk_usb_ids().is_empty());
        assert!(m.domains().is_empty() && Modules::none().is_empty());
        assert!(m.hardware_control().is_none() && m.ids() == ["bare"]);
        assert_eq!(m.iter().map(|m| m.id()).collect::<Vec<_>>(), ["bare"]);
    }
}
