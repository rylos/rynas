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
    /// Kernel metapackages rynas holds by the kernel policy (`updates.kernel`): kept back by
    /// choice, installed from the kernel card.
    #[serde(default)]
    pub kernel_held: Vec<String>,
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
    /// apt's state changed since this answer was computed (`read_at_ms`): it is the last one,
    /// and a newer one is being computed in the background (task `updates.status`; read
    /// again when it ends).
    #[serde(default)]
    pub refreshing: bool,
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

/// How new kernels are installed (`updates.kernel`, [`KernelPolicy::mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KernelMode {
    /// With the other updates, as apt does (nothing is held).
    #[default]
    System,
    /// Never by itself: held, installed with "Install kernel".
    Manual,
    /// As manual, plus a notification when a newer kernel is out (at most every
    /// [`KernelPolicy::interval_days`]).
    Notify,
    /// Installed by rynas inside the update window once it is [`KernelPolicy::min_age_days`]
    /// old, at most every [`KernelPolicy::interval_days`].
    Auto,
}

/// The `updates.kernel` configuration key. Its default depends on the machine
/// ([`KernelPolicy::default_for`]): kernels follow the other updates on a plain Debian; a
/// machine whose hardware module ships its own kernels (written to a boot flash at every
/// install) keeps them until the user installs one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelPolicy {
    #[serde(default)]
    pub mode: KernelMode,
    /// Notify and auto: at most one notification / installation every so many days (1-365).
    #[serde(default = "default_kernel_interval")]
    pub interval_days: u16,
    /// Auto: a kernel is installed once it is this many days old (0-365).
    #[serde(default = "default_kernel_age")]
    pub min_age_days: u16,
    /// Auto: restart at the reboot time of the automatic updates after installing.
    #[serde(default = "default_true")]
    pub reboot: bool,
}

fn default_kernel_interval() -> u16 {
    30
}
fn default_kernel_age() -> u16 {
    14
}
fn default_true() -> bool {
    true
}

impl Default for KernelPolicy {
    fn default() -> Self {
        Self {
            mode: KernelMode::System,
            interval_days: default_kernel_interval(),
            min_age_days: default_kernel_age(),
            reboot: true,
        }
    }
}

impl KernelPolicy {
    /// The value before the user chose one: manual where a hardware module ships its own
    /// kernels, else with the other updates.
    pub fn default_for(module_kernels: bool) -> Self {
        Self {
            mode: if module_kernels { KernelMode::Manual } else { KernelMode::System },
            ..Self::default()
        }
    }

    /// The kernel metapackages are held.
    pub fn holds(&self) -> bool {
        self.mode != KernelMode::System
    }
}

/// `GET/PUT /updates/kernel/policy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KernelPolicyState {
    pub applied: KernelPolicy,
    #[serde(default)]
    pub pending: Option<KernelPolicy>,
}

/// One kernel of a kernel feed ([`KernelFeed`], format 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FeedKernel {
    /// `uname -r` of the kernel.
    pub release: String,
    pub series: String,
    #[serde(default)]
    pub upstream: Option<String>,
    #[serde(default)]
    pub package: Option<String>,
    /// Debian version of its packages.
    pub version: String,
    /// When it was published (UTC, RFC 3339).
    #[serde(default)]
    pub published: Option<String>,
    /// Still in the package repository.
    #[serde(default)]
    pub available: bool,
    /// Upstream change log.
    #[serde(default)]
    pub changelog: Option<String>,
    #[serde(default)]
    pub previous: Option<String>,
    /// The publisher's own changes since the previous kernel of the series (empty: an
    /// upstream update only).
    #[serde(default)]
    pub changes: Vec<String>,
    /// CVEs fixed since the previous kernel of the series (`None`: not known).
    #[serde(default)]
    pub cves: Option<Vec<String>>,
}

/// A series of a kernel feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FeedSeries {
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub metapackage: Option<String>,
}

/// A kernel publisher's list of its kernels (format 1): newest first, the whole history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KernelFeed {
    pub format: u32,
    #[serde(default)]
    pub generated: Option<String>,
    #[serde(default)]
    pub series: std::collections::BTreeMap<String, FeedSeries>,
    #[serde(default)]
    pub kernels: Vec<FeedKernel>,
    #[serde(default)]
    pub cve_titles: std::collections::BTreeMap<String, String>,
}

/// A kernel newer than the installed one, in the series in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KernelOffer {
    /// `uname -r` (from the image metapackage's dependency or the feed).
    pub release: String,
    /// Debian version of the series' image metapackage.
    pub version: String,
    /// Installable now (the package repository has it).
    pub available: bool,
    /// Publication time (ms), from the feed; else when rynas first saw it.
    #[serde(default)]
    pub published_ms: Option<u64>,
    /// `published_ms` comes from the publisher's feed (else: first seen by rynas).
    #[serde(default)]
    pub published_known: bool,
    #[serde(default)]
    pub upstream: Option<String>,
    #[serde(default)]
    pub changelog: Option<String>,
    #[serde(default)]
    pub changes: Vec<String>,
    #[serde(default)]
    pub cves: Option<Vec<String>>,
}

/// A kernel series this machine can follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KernelSeriesState {
    /// Stable id (`default` on a plain Debian).
    pub id: String,
    /// Upstream branch, from the feed (`6.18`).
    #[serde(default)]
    pub branch: Option<String>,
    /// Image metapackage.
    pub image: String,
    /// Its image metapackage is installed.
    pub installed: bool,
    /// The series the machine boots.
    pub current: bool,
}

/// `GET /updates/kernel/channel[?refresh=1]`: the kernels the machine can install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KernelChannelState {
    /// A kernel metapackage is installed (else nothing below applies).
    pub supported: bool,
    /// A hardware module ships the kernels of this machine.
    #[serde(default)]
    pub module: bool,
    /// The series (more than one: the user can switch).
    #[serde(default)]
    pub series: Vec<KernelSeriesState>,
    /// Installed kernel metapackages.
    #[serde(default)]
    pub metapackages: Vec<String>,
    /// Those held now.
    #[serde(default)]
    pub held: Vec<String>,
    /// The series' image metapackage version installed.
    #[serde(default)]
    pub installed_version: Option<String>,
    /// The kernel it installs (`uname -r`).
    #[serde(default)]
    pub installed_release: Option<String>,
    /// Newer kernels of the series, newest first (the first is the one "Install kernel"
    /// installs; the feed's older ones only count how far behind the machine is).
    #[serde(default)]
    pub newer: Vec<KernelOffer>,
    /// Titles of the CVEs named in `newer`.
    #[serde(default)]
    pub cve_titles: std::collections::BTreeMap<String, String>,
    /// The feed could not be read (the list then comes from apt only).
    #[serde(default)]
    pub feed_error: Option<String>,
    #[serde(default)]
    pub feed_read_ms: Option<u64>,
    #[serde(default)]
    pub last_install_ms: Option<u64>,
    #[serde(default)]
    pub last_notified_ms: Option<u64>,
    /// Auto: the earliest time the newest kernel may be installed (age and interval), when
    /// one is waiting.
    #[serde(default)]
    pub auto_due_ms: Option<u64>,
    /// The policy in force.
    #[serde(default)]
    pub policy: KernelPolicy,
}

impl KernelChannelState {
    /// The kernel "Install kernel" installs.
    pub fn candidate(&self) -> Option<&KernelOffer> {
        self.newer.iter().find(|k| k.available)
    }

    /// Union of the CVEs fixed by the newer kernels (`None` when one of them is not known).
    pub fn cves(&self) -> Option<Vec<String>> {
        let mut all = std::collections::BTreeSet::new();
        for k in &self.newer {
            all.extend(k.cves.as_ref()?.iter().cloned());
        }
        Some(all.into_iter().collect())
    }
}

/// `POST /updates/kernel/install` → 202 `TaskStarted` (task `updates.kernel`): the version
/// the page showed (refused when another one would be installed now).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct KernelInstallRequest {
    pub version: String,
}

/// `POST /updates/kernel/series` → 202 `TaskStarted` (task `updates.kernel-series`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct KernelSeriesRequest {
    pub series: String,
}
