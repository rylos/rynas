//! The `synology` hardware provider (D-SYNOPLUG, `docs/plugins/synology.md`): every feature
//! of synodeb's synohwd, with synohwd as the executor (fans, LEDs, buttons, the
//! microcontroller, the RTC handshake at shutdown).
//!
//! - [`status`]: synohwd's `status.json` → `HardwareStatus` / `SynologyStatus`;
//! - [`conf`]: `/etc/synohwd.toml` keys, validation (synohwd's ranges and choices), renderer,
//!   and a port of synohwd's lenient reader;
//! - [`cmd`]: microcontroller commands through synohwd's FIFO, validated like `synohwd send`;
//! - [`leds`]: bay identify on the LED class devices;
//! - [`model`]: DSM fan curves and bay counts per model, platform detection;
//! - [`provider`]: [`provider::Synology`], implementing `rynas_hw::{HardwareProvider,
//!   HardwareControl}` (built in today, a plugin process later);
//! - feature `domain` (rynasd only), the daemon side of the module: `domain` (the
//!   `synology.hw` configuration domain), `boot` (synodeb's DOM through `synodeb-dom`: the
//!   boot entries on the updates page, the boot half of a root rollback), `http` (the module's routes under
//!   `/v1/modules/synology`), `ups` (synodeb's UPS layer), `notify` (synodeb's `notify`
//!   forward and `notify.conf` import) and `module` ([`module::SynologyModule`], the
//!   `rynas_module::Module` registered only on a Synology or where synodeb runs).
//!
//! Module boundary (D-MODULE): everything Synology- or synodeb-specific lives here or in the
//! web interface's `modules/synology` folder; the core only knows the `rynas-hw` and
//! `rynas-module` traits.

pub mod cmd;
pub mod conf;
#[cfg(feature = "domain")]
pub mod boot;
#[cfg(feature = "domain")]
pub mod domain;
#[cfg(feature = "domain")]
pub mod http;
pub mod leds;
pub mod model;
#[cfg(feature = "domain")]
pub mod module;
#[cfg(feature = "domain")]
pub mod notify;
pub mod provider;
pub mod status;
#[cfg(feature = "domain")]
pub mod ups;

pub use provider::Synology;
