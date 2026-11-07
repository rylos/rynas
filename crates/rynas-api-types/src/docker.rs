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
    /// The data-root is on the system disk, not on a rynas volume.
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
    /// A `docker` CLI or a `dockerd` binary exists.
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
    /// `Some`: the install action is refused, and why.
    pub install_block: Option<InstallBlock>,
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
}

fn default_keep() -> u32 {
    5
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
        }
    }
}

/// A rynas volume the engine and the stacks can go on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VolumeChoice {
    pub label: String,
    pub path: String,
    pub avail_bytes: Option<u64>,
}

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
