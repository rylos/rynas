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
//! - `domain` (feature `domain`): the `synology.hw` configuration domain.

pub mod cmd;
pub mod conf;
#[cfg(feature = "domain")]
pub mod domain;
pub mod leds;
pub mod model;
pub mod provider;
pub mod status;

pub use provider::Synology;
