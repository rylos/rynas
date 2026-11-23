//! rynas system updates (D-UPDATES, P6).
//!
//! - [`apt`]: pure parsers of `apt-get -s` output (`Inst`/`Remv` lines, kept-back sections),
//!   package name checks, `/var/run/reboot-required.pkgs`;
//! - [`kernels`]: the kernels installed in `/boot`;
//! - [`domain`]: the `updates.auto` configuration domain (unattended-upgrades and the apt
//!   timers);
//! - [`kernel_policy`]: the `updates.kernel` domain (held kernel metapackages), Debian version
//!   order, the kernel feed and the timing of notifications and automatic installs.
//!
//! Running apt, caching and the tasks live in rynasd (`crates/rynasd/src/updates.rs`).

pub mod apt;
pub mod kernels;
pub mod domain;
pub mod kernel_policy;

pub use domain::{AUTO_KEY, AutoDomain};
pub use kernel_policy::{KERNEL_KEY, KernelDomain};

/// Fixtures used by `rynasd --dry-system` (no apt is run there) and the tests.
pub mod fixtures {
    pub const FULL_UPGRADE: &str = include_str!("fixtures/full-upgrade.txt");
    pub const AUTOREMOVE: &str = include_str!("fixtures/autoremove.txt");
}
