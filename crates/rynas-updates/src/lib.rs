//! rynas system updates (D-UPDATES, P6).
//!
//! - [`apt`]: pure parsers of `apt-get -s` output (`Inst`/`Remv` lines, kept-back sections),
//!   package name checks, `/var/run/reboot-required.pkgs`;
//! - [`dom`]: `synodeb-dom status --json` (schema 1) → [`DomStatus`](rynas_api_types::updates::DomStatus);
//! - [`domain`]: the `updates.auto` configuration domain (unattended-upgrades and the apt
//!   timers).
//!
//! Running apt, caching and the tasks live in rynasd (`crates/rynasd/src/updates.rs`).

pub mod apt;
pub mod dom;
pub mod domain;

pub use domain::{AUTO_KEY, AutoDomain};

/// Fixtures used by `rynasd --dry-system` (no apt is run there) and the tests.
pub mod fixtures {
    pub const FULL_UPGRADE: &str = include_str!("fixtures/full-upgrade.txt");
    pub const AUTOREMOVE: &str = include_str!("fixtures/autoremove.txt");
    pub const SYNODEB_DOM: &str = include_str!("fixtures/synodeb-dom.json");
}
