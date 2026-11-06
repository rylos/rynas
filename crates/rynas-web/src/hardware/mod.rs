//! Platform hardware (fans, bays, front LED) through a provider.
//!
//! [`HardwareProvider`] is the `rynas-hw` interface: the dashboard and the storage page only
//! see `HardwareStatus`, never a platform's specifics. [`detect`] picks the provider of the
//! hardware module that knows this machine (one line per module, like rynasd's module
//! detection); a plain Debian has none ([`NoProvider`]: nothing shown). A module's settings
//! and actions are served by rynasd (`/api/v1/modules/<id>/...`).

use std::path::Path;
use std::sync::Arc;

pub use rynas_hw::{HardwareProvider, NoProvider};

/// The hardware provider of the machine below `hw_root` (`/` in production).
pub fn detect(hw_root: &Path) -> Arc<dyn HardwareProvider> {
    if let Some(p) = rynas_hw_synology::Synology::web_provider(hw_root) {
        return Arc::new(p);
    }
    Arc::new(NoProvider)
}

/// "Vendor Product" from DMI, e.g. "Synology Inc. DS923+"; empty if unknown.
pub fn dmi_model() -> String {
    rynas_hw::dmi_model(Path::new("/"))
}
