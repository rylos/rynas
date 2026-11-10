//! Docker (`/api/v1/docker/...`), wave D1 of phase 2 (`docs/phase2/docker-apps.md` §5).
//!
//! - [`DockerStatus`]: the engine as rynasd sees it (installed, adopted or missing, versions,
//!   storage, counts, detected managers). The only route that answers without Docker.
//! - Listings: [`ContainerSummary`], [`StackSummary`] (grouped by the compose labels, marked
//!   with the [`StackManager`]), [`ImageSummary`], [`VolumeSummary`], [`NetworkSummary`],
//!   [`DockerDiskUsage`].
//! - Stacks are ordinary folders `/<vol>/docker/stacks/<name>/compose.yaml` + `.env`, one
//!   btrfs subvolume each; actions run as rynasd tasks `docker.*`.
//! - [`DockerSettings`]: the configuration key `docker` (staged and applied like every key):
//!   the volume of the engine and the stacks, snapshot options, the preferred manager.
//! - [`DockerStats`]: one aggregated sample for the dashboard widget, pushed on
//!   `/api/v1/events` as `docker.stats`.

use serde::{Deserialize, Serialize};

/// Where the installed engine comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EngineSource {
    /// `docker-ce` from Docker's own apt repository (what rynas installs).
    DockerCe,
    /// `docker.io` of the Debian archive.
    DockerIo,
    /// A `docker`/`dockerd` binary that no Debian package owns (static binaries, Snap...).
    Other,
    /// No engine.
    #[default]
    None,
}

/// A Debian package of the engine and its installed version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DockerPackage {
    pub name: String,
    pub version: String,
}

/// An engine package dpkg left half-done (an interrupted install or upgrade): its status is
/// not "install ok installed".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IncompletePackage {
    pub name: String,
    /// The version dpkg recorded (may be empty).
    pub version: String,
    /// dpkg's abbreviated status (`${db:Status-Abbrev}` without the trailing blank): `iU`
    /// unpacked, `iF` half-configured, `iHR` half-installed and reinstall required, `iW`/`it`
    /// triggers awaiting/pending.
    pub status: String,
}

/// Why rynas cannot install Docker on this system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallBlock {
    /// Docker is already installed: it is adopted, never reinstalled.
    Installed,
    /// Not Debian (no `/etc/debian_version` or `ID=debian` in os-release).
    NotDebian,
    /// A Debian release Docker's repository does not serve (the codename is in the message
    /// of an install attempt).
    UnsupportedRelease,
    /// An architecture Docker does not build for.
    UnsupportedArch,
}

/// Something about the engine the administrator should know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockerWarning {
    /// The CLI is installed but the daemon does not answer on `/var/run/docker.sock`.
    DaemonDown,
    /// No `docker compose` plugin: stacks cannot be started.
    ComposeMissing,
    /// The data-root is outside every volume (Docker's default folder in the operating
    /// system's tree), not on a rynas volume nor in the system volume's folder.
    DataRootOnSystem,
    /// The data-root filesystem is almost full (the threshold of the settings).
    DataRootAlmostFull,
    /// `docker-ce` is not pinned to its major version (no rynas apt preferences file).
    NotPinned,
    /// A Docker older than the oldest API the managers use (API 1.44, Docker 25).
    EngineTooOld,
}

/// Filesystem usage of the data-root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FsUsage {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub avail_bytes: u64,
}

/// Counters of `docker info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DockerCounts {
    pub containers: u32,
    pub running: u32,
    pub paused: u32,
    pub stopped: u32,
    pub images: u32,
}

/// The container managers rynas recognises by image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ManagerKind {
    Arcane,
    Dockhand,
    Portainer,
    Dockge,
    Komodo,
}

/// A manager container found on this engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedManager {
    pub kind: ManagerKind,
    /// Container name (without the leading `/`).
    pub container: String,
    pub container_id: String,
    pub image: String,
    pub running: bool,
    /// First published TCP port: the UI builds `http://<the NAS host>:<port>/`.
    pub port: Option<u16>,
    /// The stack it belongs to (`com.docker.compose.project`).
    pub stack: Option<String>,
}

/// `GET /api/v1/docker/status`. Answers also without Docker (`installed: false`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DockerStatus {
    /// A `docker` CLI or a `dockerd` binary exists, or an engine package is installed;
    /// false while engine packages are half-installed (`install_incomplete`).
    pub installed: bool,
    /// The daemon answered on its socket.
    pub running: bool,
    pub source: EngineSource,
    /// Installed packages among docker-ce, docker-ce-cli, containerd.io,
    /// docker-compose-plugin, docker-buildx-plugin, docker.io, docker-compose, containerd.
    pub packages: Vec<DockerPackage>,
    /// Engine version (`29.0.1`).
    pub version: Option<String>,
    pub api_version: Option<String>,
    pub min_api_version: Option<String>,
    /// `docker compose version --short`; `None`: no compose plugin.
    pub compose_version: Option<String>,
    pub buildx_version: Option<String>,
    /// `overlay2`, `overlayfs` (containerd snapshotter), `btrfs`...
    pub storage_driver: Option<String>,
    /// The engine's data-root (`/var/lib/docker`, `/vol1/docker/engine`).
    pub data_root: Option<String>,
    /// Images are kept by containerd (the default of new Docker 29 installs): they live in
    /// containerd's root, not in the data-root.
    pub containerd_snapshotter: bool,
    pub cgroup_driver: Option<String>,
    pub live_restore: Option<bool>,
    pub logging_driver: Option<String>,
    pub counts: DockerCounts,
    /// statvfs of the data-root.
    pub data_root_usage: Option<FsUsage>,
    pub managers: Vec<DetectedManager>,
    /// `Some`: the install action is refused, and why. `None` with `installed: true`: an
    /// install rynas started did not finish (the pin or the `docker-apps` account is
    /// missing); `POST /docker/install` finishes it.
    pub install_block: Option<InstallBlock>,
    /// Engine packages dpkg left half-done (a failed install). Non-empty: `installed` is
    /// false, and `POST /docker/install` repairs them (`dpkg --configure -a`,
    /// `apt-get install -f`) and finishes the install.
    #[serde(default)]
    pub install_incomplete: Vec<IncompletePackage>,
    /// A running `docker.install` or `docker.migrate` task.
    pub task_id: Option<String>,
    pub warnings: Vec<DockerWarning>,
    pub read_at_ms: u64,
}

/// `POST /api/v1/docker/install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    /// Volume label (`vol1`) for the engine and the stacks: the `docker` key is applied with
    /// it before the packages are installed, so the engine starts on the volume.
    #[serde(default)]
    pub volume: Option<String>,
}

/// `POST /api/v1/docker/engine/migrate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MigrateRequest {
    /// Target volume label.
    pub volume: String,
}

// ------------------------------------------------------------------ settings (key `docker`)

/// One entry of `default-address-pools` in `daemon.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddressPool {
    /// IPv4 CIDR, e.g. `10.200.0.0/16`.
    pub base: String,
    /// Prefix length of each network cut from it (16-30, longer than the base's).
    pub size: u8,
}

impl Default for AddressPool {
    fn default() -> Self {
        Self {
            base: "10.200.0.0/16".into(),
            size: 24,
        }
    }
}

fn yes() -> bool {
    true
}

fn default_full_pct() -> u8 {
    90
}

fn default_restarts() -> u32 {
    5
}

fn default_window() -> u32 {
    10
}

/// The configuration key `docker`. Preferences only: stacks live on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerSettings {
    /// Volume label (`vol1`, mounted at `/vol1`) of the engine (`/vol1/docker/engine`),
    /// containerd (`/vol1/docker/containerd`) and the stacks (`/vol1/docker/stacks`).
    /// `None`: rynas leaves the engine configuration alone (an adopted engine keeps its own).
    #[serde(default)]
    pub volume: Option<String>,
    /// `default-address-pools` written only when `daemon.json` has none.
    #[serde(default)]
    pub address_pool: AddressPool,
    /// Read-only snapshot of a stack before pull/update (default on).
    #[serde(default = "yes")]
    pub snapshot_before_update: bool,
    /// Stop the stack while its snapshot is taken (databases); default off.
    #[serde(default)]
    pub stop_for_snapshot: bool,
    /// Snapshots kept per stack (oldest deleted first), 1-50; default 5.
    #[serde(default = "default_keep")]
    pub keep_snapshots: u32,
    /// Notify when the data-root filesystem is this full (50-99 %, default 90).
    #[serde(default = "default_full_pct")]
    pub data_root_full_pct: u8,
    /// Crash loop: this many restarts (2-100, default 5)...
    #[serde(default = "default_restarts")]
    pub crash_restarts: u32,
    /// ...within this many minutes (1-120, default 10).
    #[serde(default = "default_window")]
    pub crash_window_min: u32,
    /// The manager the UI links first.
    #[serde(default)]
    pub preferred_manager: Option<ManagerKind>,
    /// App catalogue: a faster (SSD) volume label where the install wizard suggests placing
    /// the folders an entry marks `ssd` (databases); `None`: no suggestion.
    #[serde(default)]
    pub apps_ssd_volume: Option<String>,
    /// Automatic image updates of chosen stacks (off for every stack by default).
    #[serde(default)]
    pub auto_update: AutoUpdateSettings,
    /// App catalogue: download signed catalogue updates daily (default on; they also need a
    /// catalogue key in this build).
    #[serde(default = "yes")]
    pub apps_online_catalog: bool,
    /// App catalogue: base URL of the online catalogue (`https://…/`, `index.json` below
    /// it); `None`: the official one.
    #[serde(default)]
    pub apps_catalog_url: Option<String>,
}

fn default_keep() -> u32 {
    5
}

/// Which version jumps an automatic update may take (D-UPDATES channel C). Versions are read
/// from the image tag (`v` and suffixes such as `-alpine` normalised, pre-releases never
/// taken) or, for a floating tag (`latest`, `stable`), from the image label
/// `org.opencontainers.image.version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdatePolicy {
    /// Same major and minor (`1.4.2` → `1.4.3`).
    Patch,
    /// Same major (`1.4` → `1.5`); with major 0, same minor (`0.4.1` → `0.4.2`).
    Minor,
    /// Calendar versions (`2026.11.0` → `2026.11.1`): same year and month.
    CalverPatch,
    /// Never automatic: only the notification.
    Notify,
}

impl UpdatePolicy {
    pub const ALL: [UpdatePolicy; 4] = [
        UpdatePolicy::Patch,
        UpdatePolicy::Minor,
        UpdatePolicy::CalverPatch,
        UpdatePolicy::Notify,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            UpdatePolicy::Patch => "patch",
            UpdatePolicy::Minor => "minor",
            UpdatePolicy::CalverPatch => "calver-patch",
            UpdatePolicy::Notify => "notify",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

/// Automatic updates of one stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StackAutoUpdate {
    #[serde(default)]
    pub enabled: bool,
    /// `None`: the catalogue entry's `update_policy`, else by the tag (calendar tags
    /// `calver-patch`, versions and floating tags `minor`, anything else `notify`).
    #[serde(default)]
    pub policy: Option<UpdatePolicy>,
}

fn default_auto_hour() -> u8 {
    4
}

fn default_auto_window() -> u16 {
    60
}

/// `docker.auto_update`: the daily window of automatic updates and the stacks that take
/// them. The image check itself runs every day whatever this says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoUpdateSettings {
    /// Start of the daily window, local time (0-23).
    #[serde(default = "default_auto_hour")]
    pub hour: u8,
    #[serde(default)]
    pub minute: u8,
    /// Length of the window (10-360 minutes): updates start only inside it.
    #[serde(default = "default_auto_window")]
    pub window_min: u16,
    /// Per stack name; a stack not listed is off.
    #[serde(default)]
    pub stacks: std::collections::BTreeMap<String, StackAutoUpdate>,
}

impl Default for AutoUpdateSettings {
    fn default() -> Self {
        Self {
            hour: default_auto_hour(),
            minute: 0,
            window_min: default_auto_window(),
            stacks: Default::default(),
        }
    }
}

impl AutoUpdateSettings {
    /// The stack's entry, or the default (off, policy by tag).
    pub fn of(&self, stack: &str) -> StackAutoUpdate {
        self.stacks.get(stack).cloned().unwrap_or_default()
    }
}

impl Default for DockerSettings {
    fn default() -> Self {
        Self {
            volume: None,
            address_pool: AddressPool::default(),
            snapshot_before_update: true,
            stop_for_snapshot: false,
            keep_snapshots: default_keep(),
            data_root_full_pct: default_full_pct(),
            crash_restarts: default_restarts(),
            crash_window_min: default_window(),
            preferred_manager: None,
            apps_ssd_volume: None,
            auto_update: AutoUpdateSettings::default(),
            apps_online_catalog: true,
            apps_catalog_url: None,
        }
    }
}

/// A volume the engine and the stacks can go on (a rynas volume or the system volume).
pub type VolumeChoice = crate::storage::VolumePlace;

/// `GET/PUT /api/v1/docker/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DockerSettingsState {
    pub applied: DockerSettings,
    pub pending: Option<DockerSettings>,
    /// The mounted rynas volumes.
    pub volumes: Vec<VolumeChoice>,
    /// Where the stacks folder is (`/vol1/docker/stacks`), from the applied volume.
    pub stacks_dir: Option<String>,
}

/// A file of an engine configuration plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlannedFile {
    pub path: String,
    pub before: Option<String>,
    pub after: String,
}

/// `POST /api/v1/docker/plan/engine`: what applying these settings writes and runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EnginePlan {
    pub files: Vec<PlannedFile>,
    /// Subvolumes and folders created (`/vol1/docker`, `/vol1/docker/engine`...).
    pub create: Vec<String>,
    pub commands: Vec<String>,
    /// The engine must restart (data-root or containerd root changes).
    pub restart: bool,
}

// ------------------------------------------------------------------ containers

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContainerState {
    Created,
    Running,
    Paused,
    Restarting,
    Removing,
    Exited,
    Dead,
    Stopping,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerHealth {
    Starting,
    Healthy,
    Unhealthy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PortMapping {
    /// Host address of a published port (`0.0.0.0`, `::`); `None` when not published.
    pub ip: Option<String>,
    pub private_port: u16,
    pub public_port: Option<u16>,
    /// `tcp`, `udp`, `sctp`.
    pub protocol: String,
}

/// One container (`GET /api/v1/docker/containers`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ContainerSummary {
    /// Full id (64 hex); the routes accept it or a unique prefix of 12+ characters.
    pub id: String,
    /// Name without the leading `/`.
    pub name: String,
    pub image: String,
    pub image_id: String,
    pub state: ContainerState,
    /// Docker's own text ("Up 3 hours (healthy)"), English, not translated.
    pub status: String,
    pub health: Option<ContainerHealth>,
    /// Creation time, seconds since the epoch.
    pub created_s: i64,
    pub ports: Vec<PortMapping>,
    /// `com.docker.compose.project`.
    pub stack: Option<String>,
    /// `com.docker.compose.service`.
    pub service: Option<String>,
    /// Link for "open": the `rynas.url` or `homepage.href` label.
    pub url: Option<String>,
    /// The manager this container is (Portainer, Dockhand...).
    pub manager: Option<ManagerKind>,
}

// ------------------------------------------------------------------ stacks

/// Who manages a stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StackManager {
    /// The folder is in rynas's stacks folder (`/<vol>/docker/stacks/<name>`): editable here
    /// and by any folder-based manager (Dockhand, Dockge, Arcane, Komodo).
    Rynas,
    /// Created by Portainer (its compose file is in Portainer's data): start, stop, logs
    /// only; its compose file is not edited here.
    Portainer,
    /// Any other compose project (made from a shell, another folder): start, stop, logs.
    #[default]
    Untracked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StackState {
    /// Every container runs.
    Running,
    /// Some run, some do not.
    Partial,
    /// Containers exist, none runs.
    Stopped,
    /// No container (a folder never started, or after `down`).
    #[default]
    Down,
}

/// One stack (`GET /api/v1/docker/stacks`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StackSummary {
    pub name: String,
    pub manager: StackManager,
    pub state: StackState,
    /// `com.docker.compose.project.working_dir`, or the folder of a rynas stack.
    pub working_dir: Option<String>,
    /// `com.docker.compose.project.config_files`.
    pub config_files: Vec<String>,
    pub containers: u32,
    pub running: u32,
    pub unhealthy: u32,
    /// Files can be edited and compose actions run (rynas stacks whose folder exists).
    pub editable: bool,
    /// The folder is a btrfs subvolume: snapshots and rollback are possible.
    pub snapshots: bool,
    pub images: Vec<String>,
    /// A compose project rynas adopted where it is (§5.7): its folder and files.
    #[serde(default)]
    pub adopted: Option<AdoptedInfo>,
}

// ------------------------------------------------------------------ adopted stacks (§5.7)

/// A compose project rynas manages in its own folder (adopted or imported, not moved).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdoptedInfo {
    /// The project directory (`--project-directory`), absolute.
    pub dir: String,
    /// The compose files (`-f`, in order), absolute.
    pub files: Vec<String>,
    /// `--env-file` files, absolute; empty: compose reads `<dir>/.env`.
    #[serde(default)]
    pub env_files: Vec<String>,
    /// `--profile` names passed to compose.
    #[serde(default)]
    pub profiles: Vec<String>,
    /// The manager whose folder this is (Portainer, Dockge, Dockhand...), when recognised.
    #[serde(default)]
    pub origin: Option<ManagerKind>,
    /// Portainer still owns the files: viewed, started, stopped and checked for updates,
    /// never edited, `up` or `down` here.
    #[serde(default)]
    pub read_only: bool,
    pub adopted_ms: u64,
    /// A file changed on disk since rynas adopted or last wrote it (another manager, a
    /// shell).
    #[serde(default)]
    pub changed_outside: bool,
}

/// Why an adoption or import deserves a look (the UI translates the code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdoptWarning {
    /// Another manager's folder: it can still edit the files; rynas notices the changes.
    OtherManager,
    /// Portainer's compose data: adopted read-only.
    Portainer,
    /// The files' `name:` differs from the stack name (rynas runs `-p <name>`).
    NameDiffers,
    /// Containers of another folder already use this project name.
    NameInUse,
    /// `external: true` networks: they must exist before `up`.
    ExternalNetworks,
    /// Services with `profiles:` start only with those profiles.
    Profiles,
    /// A top-level `include:`: the stack runs, but its files are not edited here.
    Include,
    /// A compose or env file is outside the project folder: not edited here.
    OutsideDir,
    /// A listed file does not exist.
    MissingFile,
}

/// What adopting or importing would record (`GET /docker/stacks/{name}/adopt`,
/// `POST /docker/stacks/import/preview`). Nothing is written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdoptPreview {
    pub name: String,
    pub dir: String,
    pub files: Vec<String>,
    pub env_files: Vec<String>,
    /// Profiles the files declare.
    pub profiles_found: Vec<String>,
    /// External networks the files use.
    pub external_networks: Vec<String>,
    /// The top-level `name:` of the files.
    pub name_in_file: Option<String>,
    pub origin: Option<ManagerKind>,
    pub read_only: bool,
    /// Containers of this project exist.
    pub running: bool,
    pub warnings: Vec<AdoptWarning>,
}

/// `POST /docker/stacks/{name}/adopt`: every field overrides what the labels say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AdoptRequest {
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub env_files: Vec<String>,
    #[serde(default)]
    pub profiles: Vec<String>,
}

/// `POST /docker/stacks/import/preview`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ImportPreviewRequest {
    pub dir: String,
}

/// `POST /docker/stacks/import`: a folder with compose files, registered where it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ImportStackRequest {
    pub dir: String,
    /// Default: the files' `name:`, else the folder name made valid.
    #[serde(default)]
    pub name: Option<String>,
    /// Default: the folder's compose file and its override.
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub env_files: Vec<String>,
    #[serde(default)]
    pub profiles: Vec<String>,
    /// `up -d` once registered.
    #[serde(default)]
    pub start: bool,
}

/// `201` of `POST /docker/stacks/import` and `200` of `POST /docker/stacks/{name}/adopt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StackAdopted {
    pub name: String,
    pub adopted: AdoptedInfo,
    pub task_id: Option<String>,
}

/// `POST /docker/stacks/{name}/move`: into `/<vol>/docker/stacks/<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MoveStackRequest {
    /// The stack name again (the stack is stopped while its folder is copied).
    pub confirm: String,
}

/// One file of a stack folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StackFile {
    /// Path relative to the stack folder (`config/app.ini`); no `..`, no absolute path,
    /// not `compose.yaml` / `.env` (they have their own fields).
    pub path: String,
    pub content: String,
    /// Octal mode (`0o600` for secrets); default 0o644.
    #[serde(default)]
    pub mode: Option<u32>,
}

/// `POST /api/v1/docker/stacks`: a new rynas stack from given files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CreateStackRequest {
    /// 1-63 characters `a-z 0-9 - _`, starting with a letter or digit (compose project
    /// name rules).
    pub name: String,
    pub compose: String,
    #[serde(default)]
    pub env: String,
    /// More files (configuration templates of a catalogue app).
    #[serde(default)]
    pub files: Vec<StackFile>,
    /// Volume label; default: the volume of the `docker` key.
    #[serde(default)]
    pub volume: Option<String>,
    /// Start it (`up -d`) once created.
    #[serde(default)]
    pub start: bool,
}

/// `201` of `POST /api/v1/docker/stacks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StackCreated {
    pub name: String,
    pub path: String,
    /// The `docker.stack.up` task when `start` was asked.
    pub task_id: Option<String>,
    pub validation: StackValidation,
}

/// `GET/PUT /api/v1/docker/stacks/{name}/files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StackFiles {
    pub compose: String,
    #[serde(default)]
    pub env: String,
    /// The compose file name in the folder (`compose.yaml`, `docker-compose.yml`...);
    /// read-only (ignored by `PUT`).
    #[serde(default)]
    pub compose_file: String,
    /// Hash of the files as read (`GET`); on `PUT`, when given, the files must still have
    /// it (else 409 `conflict`: changed on disk meanwhile).
    #[serde(default)]
    pub hash: String,
    /// More `-f` files of an adopted stack (read-only here).
    #[serde(default)]
    pub other_files: Vec<String>,
    /// The files cannot be written here (adopted read-only, outside the folder, `include:`).
    #[serde(default)]
    pub read_only: bool,
}

/// `POST /api/v1/docker/stacks/validate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ValidateStackRequest {
    /// The stack the files are for (its folder resolves relative paths); default a new
    /// stack on the `docker` key's volume.
    #[serde(default)]
    pub name: Option<String>,
    pub compose: String,
    #[serde(default)]
    pub env: String,
}

/// A bind mount of a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BindCheck {
    pub service: String,
    /// Absolute host path after interpolation.
    pub source: String,
    pub target: String,
    pub exists: bool,
    /// Not below the stack's volume: warned, never created.
    pub outside_volume: bool,
}

/// What `docker compose config` made of a stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StackValidation {
    pub ok: bool,
    /// compose's error (English, not translated) when not ok.
    pub error: Option<String>,
    pub services: Vec<String>,
    pub images: Vec<String>,
    /// Published host ports (`tcp/8080`).
    pub ports: Vec<String>,
    /// Published host ports another container already uses.
    pub port_conflicts: Vec<String>,
    pub binds: Vec<BindCheck>,
}

/// Body of `POST /api/v1/docker/stacks/{name}/update` (and `pull`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpdateStackRequest {
    /// Override the settings: snapshot first.
    #[serde(default)]
    pub snapshot: Option<bool>,
    /// Override the settings: stop the stack during the snapshot.
    #[serde(default)]
    pub stop_first: Option<bool>,
}

/// Body of `POST /api/v1/docker/stacks/{name}/remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RemoveStackRequest {
    /// Also delete the folder (subvolume) with its data and snapshots.
    #[serde(default)]
    pub delete_files: bool,
    /// The stack name again: required with `delete_files` (else 400
    /// `confirmation_required`).
    #[serde(default)]
    pub confirm: Option<String>,
}

/// Image of a service when a snapshot was taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ServiceImage {
    pub service: String,
    /// The reference in the compose file (`nginx:1.27`).
    pub image: String,
    /// The image id the service ran (`sha256:...`).
    pub image_id: String,
    /// The registry digest (`nginx@sha256:...`), when known.
    pub digest: Option<String>,
}

/// A read-only snapshot of a stack folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StackSnapshot {
    /// `YYYYMMDDTHHMMSS` (UTC); the folder name below `/<vol>/docker/.snapshots/<stack>/`.
    pub id: String,
    pub created_s: i64,
    /// `manual`, `update`, `pull`, `before-rollback`.
    pub reason: String,
    pub images: Vec<ServiceImage>,
}

/// `POST /api/v1/docker/stacks/{name}/rollback`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RollbackRequest {
    pub snapshot: String,
}

/// `GET /api/v1/docker/stacks/{name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StackDetail {
    pub summary: StackSummary,
    pub containers: Vec<ContainerSummary>,
    pub snapshots: Vec<StackSnapshot>,
}

// ------------------------------------------------------------------ images, volumes, networks

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ImageSummary {
    pub id: String,
    pub tags: Vec<String>,
    pub digests: Vec<String>,
    pub size_bytes: u64,
    pub created_s: i64,
    /// Containers using it (-1: not computed by the engine).
    pub containers: i64,
    /// No tag.
    pub dangling: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VolumeSummary {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    /// `com.docker.compose.project`.
    pub stack: Option<String>,
    /// From the disk usage when known.
    pub size_bytes: Option<u64>,
    /// Containers using it, from the disk usage when known.
    pub in_use: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetworkSummary {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub scope: String,
    pub subnets: Vec<String>,
    pub internal: bool,
    pub stack: Option<String>,
}

/// `GET /api/v1/docker/df` (`docker system df`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DockerDiskUsage {
    pub images_bytes: u64,
    pub images_reclaimable_bytes: u64,
    pub containers_bytes: u64,
    pub volumes_bytes: u64,
    pub build_cache_bytes: u64,
}

// ------------------------------------------------------------------ logs, stats, updates

/// One line of `GET /api/v1/docker/containers/{id}/logs/stream` (SSE event `log`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LogLine {
    /// `stdout` or `stderr`.
    pub stream: String,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ContainerStat {
    pub id: String,
    pub name: String,
    pub stack: Option<String>,
    /// Percent of one CPU (can exceed 100 on several cores).
    pub cpu_pct: f32,
    pub mem_bytes: u64,
}

/// `GET /api/v1/docker/stats` and the `docker.stats` event: one aggregated sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DockerStats {
    pub running: u32,
    pub total: u32,
    /// Sum over running containers, percent of one CPU.
    pub cpu_pct: f32,
    /// Number of CPUs (the 100 % of the whole machine is `cpus * 100`).
    pub cpus: u32,
    pub mem_bytes: u64,
    /// Total memory of the machine.
    pub mem_total_bytes: u64,
    /// Top 5 by CPU then memory.
    pub top: Vec<ContainerStat>,
    pub at_ms: u64,
    /// Every running container of the sample (the containers table), compact: id, CPU in
    /// percent of one CPU (one decimal), memory in bytes. Added 2026-11-07; absent from
    /// older servers.
    #[serde(default)]
    pub usage: Vec<ContainerUsage>,
}

/// One running container's numbers in [`DockerStats::usage`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ContainerUsage {
    /// Full container id (as in `ContainerSummary::id`).
    pub id: String,
    /// Percent of one CPU.
    pub cpu: f32,
    /// Memory in bytes (cgroup `memory.current` minus `inactive_file`).
    pub mem: u64,
}

/// One image reference checked for updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ImageUpdate {
    /// The reference containers use (`nginx:1.27`).
    pub image: String,
    /// Registry digest of the local image (`sha256:...`), from its repo digests.
    pub local_digest: Option<String>,
    /// Registry digest of the same tag now.
    pub remote_digest: Option<String>,
    pub update_available: bool,
    /// Why the check failed (registry unreachable, private image...).
    pub error: Option<String>,
    pub stacks: Vec<String>,
    pub containers: Vec<String>,
}

/// `GET /api/v1/docker/updates`: the last update check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpdateReport {
    /// 0: never checked.
    pub checked_at_ms: u64,
    pub images: Vec<ImageUpdate>,
    /// When the next automatic check is due (0: not scheduled, e.g. Docker absent).
    #[serde(default)]
    pub next_check_ms: u64,
    /// Every stack with an update, and what its policy does with it (computed when the
    /// report is read, from the last check and the applied settings).
    #[serde(default)]
    pub stacks: Vec<StackUpdate>,
    /// The last automatic update of each stack.
    #[serde(default)]
    pub auto: Vec<AutoResult>,
    /// Start of the next automatic update window (0: no stack takes automatic updates).
    #[serde(default)]
    pub next_window_ms: u64,
}

/// What the stack's policy does with its update (whether it runs depends on
/// [`StackUpdate::enabled`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpdateVerdict {
    /// Allowed: applied in the next window when automatic updates are on.
    Auto,
    /// Decided after the download, from the image version label (floating or short tags).
    AfterPull,
    /// The policy does not allow it (see the reason): notification only.
    #[default]
    Hold,
}

/// Why an update is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    /// A new major version.
    Major,
    /// A new minor version where the policy allows patches only (or with major 0).
    Minor,
    /// A new calendar month (or year) under `calver-patch`.
    Month,
    /// The new version is a pre-release.
    Prerelease,
    /// Another image variant (`-alpine` to `-bookworm`…).
    Variant,
    /// An older version.
    Downgrade,
    /// The version cannot be read from the tag or the image label.
    UnknownVersion,
    /// The policy is `notify`.
    Policy,
    /// The new catalogue entry needs answers or a new package major.
    NeedsReview,
    /// rynas cannot update this stack (not in its folder, adopted read-only).
    NotManaged,
}

/// One stack's update in [`UpdateReport::stacks`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StackUpdate {
    pub stack: String,
    /// The catalogue app of the stack (its update comes from the catalogue).
    #[serde(default)]
    pub app: Option<String>,
    /// Images (or `service: from → to` for an app) with an update.
    pub images: Vec<String>,
    /// The policy that applies (the stack's own, the catalogue's, or by tag).
    pub policy: Option<UpdatePolicy>,
    /// Automatic updates are on for the stack (off: notification only, whatever the
    /// verdict).
    pub enabled: bool,
    pub verdict: UpdateVerdict,
    #[serde(default)]
    pub reason: Option<HoldReason>,
    /// Version text of the change when known (`2026.11.0 → 2026.11.1`).
    #[serde(default)]
    pub change: Option<String>,
}

/// How an automatic update ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutoOutcome {
    #[default]
    Updated,
    /// The new version failed (pull, start or health): the snapshot was put back.
    RolledBack,
    /// The download showed a version the policy does not allow: nothing changed.
    Held,
    /// Failed and could not be rolled back (or there was no snapshot).
    Failed,
}

/// One entry of [`UpdateReport::auto`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AutoResult {
    pub stack: String,
    pub at_ms: u64,
    pub outcome: AutoOutcome,
    /// The task that ran it.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Why it failed or was held (English, from the task).
    #[serde(default)]
    pub detail: Option<String>,
}

/// Data of the `docker.changed` event: containers, stacks or images changed (re-read the
/// lists the page shows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DockerChanged {
    pub generation: u64,
}

// ------------------------------------------------------------------ validation

/// A valid stack (compose project) name: 1-63 of `a-z 0-9 - _`, first a letter or digit.
/// Also refuses names that collide with rynas's own folders.
pub fn valid_stack_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_'))
}

/// A container reference of the routes: 12-64 lowercase hex characters (an id or a
/// prefix), or a container name (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`, up to 128).
pub fn valid_container_ref(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_names() {
        for ok in ["immich", "my-app", "a", "app_2", "9lives"] {
            assert!(valid_stack_name(ok), "{ok}");
        }
        for bad in [
            "",
            "-x",
            "_x",
            "My",
            "a b",
            "a/b",
            "..",
            "é",
            &"a".repeat(64),
        ] {
            assert!(!valid_stack_name(bad), "{bad}");
        }
    }

    #[test]
    fn settings_defaults_round_trip() {
        let s: DockerSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, DockerSettings::default());
        assert!(s.snapshot_before_update && s.keep_snapshots == 5);
        assert!(serde_json::from_str::<DockerSettings>(r#"{"nope":1}"#).is_err());
        assert!(s.auto_update.stacks.is_empty() && s.auto_update.hour == 4);
        let a: AutoUpdateSettings = serde_json::from_str(
            r#"{"stacks":{"ha":{"enabled":true,"policy":"calver-patch"}}}"#,
        )
        .unwrap();
        assert_eq!(a.of("ha").policy, Some(UpdatePolicy::CalverPatch));
        assert!(!a.of("other").enabled);
        for p in UpdatePolicy::ALL {
            assert_eq!(UpdatePolicy::parse(p.as_str()), Some(p));
            assert_eq!(serde_json::to_value(p).unwrap(), p.as_str());
        }
    }

    #[test]
    fn container_refs() {
        assert!(valid_container_ref("0123456789ab"));
        assert!(valid_container_ref("immich_server"));
        assert!(
            !valid_container_ref("../x") && !valid_container_ref("a/b") && !valid_container_ref("")
        );
    }
}
