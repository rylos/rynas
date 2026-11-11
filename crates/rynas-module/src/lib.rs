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
//! - [`Module::boot`]: a [`boot::BootLayer`] when the machine boots from a device of its own
//!   (its entries on the updates page, the boot half of a root rollback). Without one the
//!   updates page shows the kernels in `/boot` and a root rollback is a console procedure.
//! - [`Module::rescue_feedback`]: how the machine signals the network rescue paths (beeps,
//!   LEDs; `rynas_net::rescue::Feedback`). Without one they are silent.
//! - [`Module::reset_button`]: the machine's reset button and whether a double press restores
//!   the factory network (a setting of the module's own), shown read-only on Network › Rescue.
//! - [`Module::wake_clock`]: the real-time clock that powers the machine on and the unit that
//!   reads its alarm at shutdown (the power schedule). Without one `rtc0` (or the first RTC).
//!
//! The web interface has the matching rule: a module's pages, tabs and texts live in the UI's
//! module folder and are shown only where the module answers.

use std::sync::Arc;

use rynas_api_types::network::ResetButton;
use rynas_api_types::storage::HealthVerdict;
use rynas_config::{Applier, Config};
use rynas_hw::HardwareControl;
pub use rynas_net::rescue::{Feedback, Signal};

pub mod boot;
pub mod http;
pub mod notify;
pub mod ups;

pub use boot::{BootLayer, BootSync};
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

    /// Folder or file names deduplication never looks at (D-DEDUP), matched by name at any
    /// depth: what data migrated from this machine's own system carries (thumbnails,
    /// metadata folders). gitignore patterns without `/` inside (a trailing one: folders only).
    fn dedup_excludes(&self) -> &'static [&'static str] {
        &[]
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

    /// The boot layer, when the machine boots from a device of its own.
    fn boot(&self) -> Option<Arc<dyn BootLayer>> {
        None
    }

    /// How the machine signals the network rescue paths.
    fn rescue_feedback(&self) -> Option<Arc<dyn Feedback>> {
        None
    }

    /// The reset button as `owner` sees it (staged value included); `Ok(None)` without one.
    fn reset_button(&self, _cfg: &Config, _owner: &str) -> Result<Option<ResetButton>, String> {
        Ok(None)
    }

    /// The real-time clock that powers the machine on.
    fn wake_clock(&self) -> Option<WakeClock> {
        None
    }

    /// The core's notification engine, once it is running.
    fn attach(&self, _notifier: Arc<dyn rynas_notify::Notifier>) {}

    /// The verdicts of the core's periodic storage check, `(kernel name, verdict)`.
    fn storage_update(&self, _health: &[(String, HealthVerdict)]) {}

    /// rynasd is stopping.
    fn stop(&self) {}
}

/// The wake clock of a machine (the power schedule's startups).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeClock {
    /// End of the name of the RTC's `device` (`-0030`: an I²C clock at address 0x30); that
    /// RTC is armed instead of `rtc0`.
    pub rtc_device_suffix: &'static str,
    /// The unit whose stop reads the alarm (`rynas-power-wake.service` is ordered after it,
    /// so it writes the alarm first at shutdown).
    pub read_by_unit: &'static str,
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

    /// The deduplication excludes of every module, without repeats.
    pub fn dedup_excludes(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for p in self.0.iter().flat_map(|m| m.dedup_excludes().iter()) {
            if !v.iter().any(|x| x == p) {
                v.push(p.to_string());
            }
        }
        v
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

    /// The first module's rescue feedback.
    pub fn rescue_feedback(&self) -> Option<Arc<dyn Feedback>> {
        self.0.iter().find_map(|m| m.rescue_feedback())
    }

    /// The first module's reset button.
    pub fn reset_button(&self, cfg: &Config, owner: &str) -> Result<Option<ResetButton>, String> {
        for m in &self.0 {
            if let Some(b) = m.reset_button(cfg, owner)? {
                return Ok(Some(b));
            }
        }
        Ok(None)
    }

    /// The first module's wake clock.
    pub fn wake_clock(&self) -> Option<WakeClock> {
        self.0.iter().find_map(|m| m.wake_clock())
    }

    /// The first module's boot layer (one machine boots one way).
    pub fn boot(&self) -> Option<Arc<dyn BootLayer>> {
        self.0.iter().find_map(|m| m.boot())
    }

    /// The first module's hardware control.
    pub fn hardware_control(&self) -> Option<Arc<dyn HardwareControl>> {
        self.0.iter().find_map(|m| m.hardware_control())
    }

    /// Gives every module the notification engine.
    pub fn attach(&self, notifier: Arc<dyn rynas_notify::Notifier>) {
        for m in &self.0 {
            m.attach(notifier.clone());
        }
    }

    /// Hands the storage check's verdicts to every module.
    pub fn storage_update(&self, health: &[(String, HealthVerdict)]) {
        for m in &self.0 {
            m.storage_update(health);
        }
    }

    /// rynasd is stopping.
    pub fn stop(&self) {
        for m in &self.0 {
            m.stop();
        }
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
        assert!(m.dedup_excludes().is_empty());
        assert!(m.hardware_control().is_none() && m.ids() == ["bare"]);
        assert!(m.boot().is_none() && m.rescue_feedback().is_none() && m.wake_clock().is_none());
        assert_eq!(m.iter().map(|m| m.id()).collect::<Vec<_>>(), ["bare"]);
    }
}
