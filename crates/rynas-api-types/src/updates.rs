//! System updates (`/api/v1/updates/...`): Debian packages through apt, automatic updates
//! with unattended-upgrades, and the kernel and boot state (the boot device of a hardware
//! module's boot layer, where there is one).
//!
//! Configuration key `updates.auto` ([`AutoUpdates`]). Checking and installing are rynasd
//! tasks (`updates.check`, `updates.install`, `updates.autoremove`, `updates.changelog`).

use serde::{Deserialize, Serialize};

/// One package that `apt-get full-upgrade` would upgrade or newly install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpdatePackage {
    pub name: String,
    /// `amd64`, `all`...
    #[serde(default)]
    pub arch: String,
    /// Installed version; `None` for a package the upgrade would newly install.
    #[serde(default)]
    pub current: Option<String>,
    /// Version after the upgrade.
    pub candidate: String,
    /// Where the new version comes from, as apt prints it (`Debian-Security:13/stable-security`,
    /// several separated by `, `).
    #[serde(default)]
    pub origin: String,
    /// The new version comes from a security suite.
    #[serde(default)]
    pub security: bool,
}

/// What the page shows: upgradable packages (from a cached `apt-get -s full-upgrade`), what
/// the upgrade would remove, what `autoremove` would remove, and the reboot flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpdatesStatus {
    /// `false`: no apt on this machine (everything else is empty).
    pub apt: bool,
    /// When the simulation ran (ms).
    #[serde(default)]
    pub read_at_ms: u64,
    /// When the package lists were last downloaded (`apt-get update`), when known.
    #[serde(default)]
    pub lists_updated_ms: Option<u64>,
    /// Packages to upgrade and new packages the upgrade pulls in (`current: None`).
    #[serde(default)]
    pub packages: Vec<UpdatePackage>,
    /// Packages a full upgrade would REMOVE: listed and confirmed before installing.
    #[serde(default)]
    pub removals: Vec<String>,
    /// Packages apt keeps back (held or not installable now).
    #[serde(default)]
    pub held: Vec<String>,
    /// What `apt-get autoremove` would remove (never run automatically).
    #[serde(default)]
    pub autoremovable: Vec<String>,
    /// `/var/run/reboot-required` exists.
    #[serde(default)]
    pub reboot_required: bool,
    /// `/var/run/reboot-required.pkgs`.
    #[serde(default)]
    pub reboot_packages: Vec<String>,
    /// The simulation failed (apt broken, lock held too long...): its message.
    #[serde(default)]
    pub error: Option<String>,
    /// An `updates.*` task is running (its id): the actions wait for it.
    #[serde(default)]
    pub running_task: Option<String>,
    /// unattended-upgrades is installed (automatic updates need it).
    #[serde(default)]
    pub unattended_installed: bool,
}

impl UpdatesStatus {
    pub fn security_count(&self) -> usize {
        self.packages.iter().filter(|p| p.security).count()
    }
}

/// `POST /updates/install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InstallRequest {
    /// Empty: everything (`apt-get full-upgrade`); otherwise only these packages
    /// (`apt-get install --only-upgrade`).
    #[serde(default)]
    pub packages: Vec<String>,
    /// The packages the upgrade would remove, as the client showed them: every removal must
    /// be in this list (else 400 `confirmation_required`).
    #[serde(default)]
    pub confirm_removals: Vec<String>,
}

/// `POST /updates/autoremove`: exactly the list the client showed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AutoremoveRequest {
    pub packages: Vec<String>,
}

/// Which updates unattended-upgrades installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutoMode {
    /// Lists are still refreshed daily (for the notification), nothing is installed.
    #[default]
    Off,
    /// Debian-Security only.
    Security,
    /// Debian stable, -security and -updates.
    All,
}

/// The `updates.auto` configuration key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoUpdates {
    #[serde(default)]
    pub mode: AutoMode,
    /// Start of the daily window, local time.
    #[serde(default = "default_hour")]
    pub hour: u8,
    #[serde(default)]
    pub minute: u8,
    /// Length of the window: the start is spread randomly within it (10..=360 min).
    #[serde(default = "default_window")]
    pub window_min: u16,
    /// Reboot by itself when an update needs it.
    #[serde(default)]
    pub reboot: bool,
    #[serde(default = "default_reboot_hour")]
    pub reboot_hour: u8,
    #[serde(default)]
    pub reboot_minute: u8,
}

fn default_hour() -> u8 {
    3
}
fn default_window() -> u16 {
    60
}
fn default_reboot_hour() -> u8 {
    4
}

impl Default for AutoUpdates {
    fn default() -> Self {
        Self {
            mode: AutoMode::Off,
            hour: default_hour(),
            minute: 0,
            window_min: default_window(),
            reboot: false,
            reboot_hour: default_reboot_hour(),
            reboot_minute: 0,
        }
    }
}

/// `GET /updates/backports`: Debian backports of this release (`<codename>-backports`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BackportsState {
    /// rynas can switch them here: plain Debian with a codename, no module system layer.
    pub available: bool,
    /// The release codename (`trixie`).
    pub codename: Option<String>,
    /// Some configured apt source has `<codename>-backports`.
    pub enabled: bool,
    /// rynas's own file (`/etc/apt/sources.list.d/rynas-backports.sources`) is there.
    pub by_rynas: bool,
    /// Another source of the system has it (shown read-only: rynas never touches it).
    pub by_system: bool,
}

/// `PUT /updates/backports` → 202 `TaskStarted` (task `updates.backports`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BackportsRequest {
    pub enabled: bool,
}

/// `GET/PUT /updates/auto`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AutoUpdatesState {
    pub applied: AutoUpdates,
    #[serde(default)]
    pub pending: Option<AutoUpdates>,
    /// unattended-upgrades is installed.
    #[serde(default)]
    pub unattended_installed: bool,
}

/// Kernel trial of a boot layer (a new kernel boots once on probation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KernelTrial {
    #[default]
    None,
    Armed,
    Trying,
}

/// A kernel written to the boot device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BootKernel {
    #[serde(default)]
    pub release: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// The boot device of a hardware module's boot layer (a boot flash with its own kernel
/// entries), as the module reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BootImage {
    /// Boot entry titles in GRUB order.
    #[serde(default)]
    pub entries: Vec<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub fallback: Vec<String>,
    #[serde(default)]
    pub trial: KernelTrial,
    /// Entry of this boot, when known.
    #[serde(default)]
    pub running: Option<String>,
    #[serde(default)]
    pub outdated: Option<String>,
    /// Kernel of the main entry.
    #[serde(default)]
    pub kernel: Option<BootKernel>,
    /// Kernel of the fallback entry (the previous kernel).
    #[serde(default)]
    pub kernel_old: Option<BootKernel>,
    #[serde(default)]
    pub free_bytes: Option<u64>,
}

/// `GET /updates/kernel[?refresh=1]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KernelStatus {
    /// `uname -r`.
    pub running: String,
    /// Kernels installed on the root filesystem (`/boot/vmlinuz-*`), newest first.
    #[serde(default)]
    pub installed: Vec<String>,
    /// A hardware module's boot layer manages the boot entries of this machine.
    #[serde(default)]
    pub boot_layer: bool,
    /// Its boot device (cached for hours: reading it may mount it); `None` without a boot
    /// layer or on error.
    #[serde(default)]
    pub boot: Option<BootImage>,
    #[serde(default)]
    pub read_at_ms: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}
