//! rynas safe networking (P4, decision D-NET; design in `docs/research/network.md`).
//!
//! A headless NAS must never lock its administrator out, so every change of the network is a
//! **transaction** ([`txn`]):
//!
//! 1. validate ([`validate`]): addresses, gateway inside the subnet, duplicate addresses
//!    (`arping -D`), MTU within the driver maximum, at least one management path left;
//! 2. snapshot the files that are about to change as the last known good revision
//!    ([`store`]); write the trial marker (fsync);
//! 3. arm the rollback timer with `systemd-run --on-active=<N>s rynasctl net rollback`, so a
//!    crash or restart of rynasd cannot keep an unconfirmed change;
//! 4. write the networkd files atomically ([`networkd`]), `networkctl reload` and reconfigure
//!    the touched links;
//! 5. verify the actual state against the expected one, repairing what networkd leaves
//!    behind (an MTU it does not reset, stale addresses: the lesson of netplan LP#1768802);
//! 6. wait for the confirmation, which must come over the new path; without it the timer
//!    restores the last known good revision and verifies it again.
//!
//! Around it: the boot guard (`rynas-net-guard.service`, before networkd) restores the last
//! known good files when a trial was interrupted by a reboot; the post-boot watchdog falls back
//! to the last known good revision and then to the rescue configuration (DHCP and link-local on
//! every port) when no connected port has a routable address; the rescue paths ([`rescue`]):
//! deterministic IPv4 link-local addresses, the Avahi service file, the Synology reset button
//! double press; the read-only parser of ifupdown and its guided migration ([`ifupdown`]);
//! diagnostics ([`diag`]).
//!
//! Everything that touches the system goes through [`os::NetOs`], so the engine runs the
//! same in production, under `--dry-system` and against the scripted kernel of [`fake`].

pub mod diag;
pub mod fake;
pub mod ifupdown;
pub mod inventory;
pub mod networkd;
pub mod os;
pub mod rescue;
pub mod store;
pub mod txn;
pub mod validate;

use std::path::{Path, PathBuf};

pub use os::{HostOs, NetOs};
pub use txn::{Engine, NetError};

/// Where the networkd files rynas writes live.
pub const NETWORKD_DIR: &str = "/etc/systemd/network";
/// ifupdown's main file.
pub const INTERFACES: &str = "/etc/network/interfaces";
/// Where ifupdown's file goes when rynas takes over (kept for the rollback and by hand).
pub const INTERFACES_BACKUP: &str = "/etc/network/interfaces.rynas-bak";
/// The root helper the timers and units call.
pub const RYNASCTL: &str = "/usr/libexec/rynas/rynasctl";
/// Transient unit of the rollback timer (`systemd-run --unit=`).
pub const ROLLBACK_UNIT: &str = "rynas-net-rollback";

/// Default, smallest and largest confirmation timeout (seconds).
pub const TIMEOUT_DEFAULT_S: u32 = 120;
pub const TIMEOUT_MIN_S: u32 = 60;
pub const TIMEOUT_MAX_S: u32 = 600;
/// Seconds the boot guard gives a migration done at boot before restoring ifupdown.
pub const BOOT_CHECK_S: u32 = 180;

/// Why a revision was restored (`NetRollbackReport::reason`, `rynasctl net rollback --reason`).
pub mod reason {
    /// The rollback timer: nobody confirmed in time.
    pub const TIMEOUT: &str = "timeout";
    /// The timer of a migration done at boot.
    pub const BOOT_CHECK: &str = "boot_check";
    /// An administrator (API, `rynasctl`).
    pub const MANUAL: &str = "manual";
    /// The new settings did not come up.
    pub const VERIFY_FAILED: &str = "verify_failed";
    /// The boot guard found an interrupted trial.
    pub const BOOT_GUARD: &str = "boot_guard";
}

/// Locations of the engine.
#[derive(Debug, Clone)]
pub struct NetPaths {
    /// rynas's network state: revisions, trial marker, staged changes (`/var/lib/rynas/net`).
    pub state: PathBuf,
    /// Volatile state (`/run/rynas`): the reset-button press.
    pub run: PathBuf,
}

impl NetPaths {
    pub fn new(state: impl Into<PathBuf>, run: impl Into<PathBuf>) -> Self {
        Self {
            state: state.into(),
            run: run.into(),
        }
    }

    /// The defaults of a package install: `/var/lib/rynas/net`, `/run/rynas`.
    pub fn system() -> Self {
        Self::new("/var/lib/rynas/net", "/run/rynas")
    }

    pub fn from_state_dir(state_dir: &Path) -> Self {
        Self::new(state_dir.join("net"), "/run/rynas")
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    rynas_tasks::now_ms()
}
