//! The `qnap` hardware module (`docs/plugins/qnap.md`).
//!
//! QNAP boards carry no usable identification in DMI (the HS-251 reads "To be filled by
//! O.E.M." / "Aptio CRB"), so [`model`] only matches a machine on a full signature: DMI
//! strings, CPU, the PCI devices and the NICs' vendor prefix. The Super I/O chip is then
//! confirmed from its own driver (`control::confirm`). **Everything that touches a GPIO line
//! needs the result of that check**: on anything else the module does not exist.
//!
//! - [`api`]: the API documents (no feature, linked by the web interface);
//! - [`model`] (feature `identify`): the model table, identification, the web provider;
//! - feature `domain` (rynasd): [`gpio`] (the gpio character device), [`beep`] (the PC
//!   speaker), [`control`] (LEDs, buttons, alarm), [`disks`] (bays and their state),
//!   `domain` (the `qnap.hw` key), `http` (the routes), `module` (the `rynas_module::Module`).
//!
//! **Not verified on hardware**: the line mapping comes from the vendor's configuration and
//! the driver's source. See `verified` in the model table.

pub mod api;
#[cfg(feature = "domain")]
pub mod beep;
#[cfg(feature = "domain")]
pub mod control;
#[cfg(feature = "domain")]
pub mod disks;
#[cfg(feature = "domain")]
pub mod domain;
#[cfg(feature = "domain")]
pub mod gpio;
#[cfg(feature = "domain")]
pub mod http;
#[cfg(feature = "identify")]
pub mod model;
#[cfg(feature = "domain")]
pub mod module;
#[cfg(feature = "identify")]
pub mod provider;
