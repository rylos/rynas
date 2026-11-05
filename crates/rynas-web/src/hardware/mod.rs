//! Platform hardware (fans, bays, front LED) through a provider.
//!
//! [`HardwareProvider`] is the `rynas-hw` interface: the dashboard and the storage page only
//! see `HardwareStatus`, never Synology specifics. Today the only provider is
//! [`synohwd::Synohwd`], which reads the status file of the synodeb daemon through
//! `rynas-hw-synology`; the Synology settings and actions are served by rynasd
//! (`/api/v1/hardware/synology/...`). Later a plugin (`rynas-plugin-synology`, `-ugreen`,
//! `-hwmon`) will implement the same contract.

pub mod synohwd;

pub use rynas_hw::{HardwareProvider, NoProvider};

/// "Vendor Product" from DMI, e.g. "Synology Inc. DS923+"; empty if unknown.
pub fn dmi_model() -> String {
    rynas_hw::dmi_model(std::path::Path::new("/"))
}
