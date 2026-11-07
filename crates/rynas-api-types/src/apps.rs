//! The app catalogue (`/api/v1/apps/...`), wave A2 of phase 2 (`docs/phase2/docker-apps.md`
//! §5.2 for the routes, §6 for the catalogue format these types mirror).
//!
//! - [`AppCatalog`]: the cards of the catalogue page ([`AppCard`]), with the RAM gate against
//!   the installed RAM and the installed state read from the stacks on disk.
//! - [`AppDetail`]: one entry: description, the install form ([`AppForm`], a JSON Schema
//!   subset rendered by one generic form), ports, the `web` block, folders, shares, notes.
//! - [`AppInstallRequest`] → [`AppInstallPlan`] (no side effects) → [`AppInstallStarted`]
//!   (task `apps.install`).
//! - [`InstalledApp`]: stacks whose `compose.yaml` carries an `x-rynas` block;
//!   [`AppUpdatePlan`] before an update (task `apps.update`, snapshot and rollback).
//!
//! Secrets never appear in these types except [`ShownSecret`], once, in the install answer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::docker::{StackSnapshot, StackState};

fn yes() -> bool {
    true
}

fn not(b: &bool) -> bool {
    !*b
}

// ------------------------------------------------------------------ catalogue metadata

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppCategory {
    Management,
    Media,
    Photos,
    Files,
    Documents,
    Productivity,
    Security,
    HomeAutomation,
    Monitoring,
    Network,
    Sync,
    Development,
    Backup,
    Ai,
}

/// Honest warnings shown on the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppAntifeature {
    /// Upstream is not free software.
    NonFree,
    /// Needs more than 2 GB of RAM to be usable.
    HighResources,
    /// Needs a GPU or accelerator.
    NeedsGpu,
    /// Sends usage data upstream by default.
    Telemetry,
    /// No upstream release for a long time.
    Unmaintained,
    /// Clients refuse plain HTTP: needs the web access module or another TLS proxy.
    RequiresHttps,
    /// Root-equivalent access to the host (Docker socket, privileged, host devices).
    RootAccess,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    #[default]
    Tcp,
    Udp,
}

/// A published port of an entry (`[[ports]]` of `app.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppPort {
    /// Short identifier (`web`, `sync`), referenced by [`AppWeb::port`].
    pub name: String,
    /// The `.env` variable holding the host port; the key of [`AppInstallRequest::ports`].
    pub env: String,
    pub service: String,
    pub container: u16,
    /// Default host port.
    pub default: u16,
    pub protocol: PortProtocol,
    /// English text from the entry ("Web interface").
    pub purpose: String,
    /// The installer may move the host port.
    #[serde(default = "yes")]
    pub relocatable: bool,
    /// An HTTP port fronted by the web access proxy: bound to `RYNAS_BIND` (loopback unless
    /// the app is published on the LAN).
    #[serde(default)]
    pub proxied: bool,
}

/// The RAM gate: installed RAM (`MemTotal`, with a 12.5 % allowance for memory reserved by
/// firmware and the iGPU) against the entry's resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RamGate {
    Ok,
    /// Install allowed, with a warning.
    BelowRecommended {
        installed_mb: u64,
        recommended_mb: u32,
    },
    /// Install refused.
    BelowMinimum {
        installed_mb: u64,
        minimum_mb: u32,
    },
    /// The installed RAM could not be read.
    #[default]
    Unknown,
}

/// One catalogue card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppCard {
    pub id: String,
    pub name: String,
    /// One line (≤ 140 characters, English, from the entry).
    pub summary: String,
    pub categories: Vec<AppCategory>,
    /// Package version of the entry (`MAJOR.MINOR.PATCH`).
    pub version: String,
    /// Upstream version.
    pub app_version: String,
    /// SPDX expression of upstream.
    pub license: String,
    pub antifeatures: Vec<AppAntifeature>,
    pub ports: Vec<AppPort>,
    pub ram_min_mb: u32,
    pub ram_recommended_mb: u32,
    pub ram: RamGate,
    /// The entry has images for this machine's architecture.
    pub arch_ok: bool,
    /// This rynas is at least the entry's `min_rynas`.
    pub rynas_ok: bool,
    /// A Docker manager (mounts the Docker socket; never public).
    pub manager: bool,
    /// Relative URL of the icon (`/api/v1/apps/catalog/<id>/icon`, SVG).
    pub icon: String,
    /// Stacks where this entry is installed (usually zero or one, named after the id).
    pub installed: Vec<String>,
    /// One of the installed stacks has an update in the catalogue.
    pub update_available: bool,
}

/// An entry the catalogue could not load (shown to administrators, never installable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CatalogIssue {
    pub app: String,
    pub file: String,
    pub message: String,
}

/// `GET /api/v1/apps/catalog`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppCatalog {
    /// Source id written into `x-rynas.catalog` (`rynas` for the official catalogue).
    pub source: String,
    /// Sorted by name.
    pub apps: Vec<AppCard>,
    pub issues: Vec<CatalogIssue>,
    /// `MemTotal` in MiB; `None` when unreadable.
    pub installed_ram_mb: Option<u64>,
    /// `amd64`, `arm64`, or the machine's own name for other architectures.
    pub arch: String,
    /// Docker is installed (the install action needs it; plans work without).
    pub docker_installed: bool,
    /// Where new app stacks go (`/vol1/docker/stacks`); `None` until Docker has a volume.
    pub stacks_dir: Option<String>,
}

// ------------------------------------------------------------------ entry detail

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FormFieldType {
    #[default]
    String,
    Integer,
    Boolean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FormFormat {
    Hostname,
    /// An http(s) URL.
    Uri,
    Email,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FormWidget {
    Text,
    Password,
    Textarea,
    Select,
    Switch,
}

/// `x-ui` hints of a form field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FormUi {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget: Option<FormWidget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Folded under "Advanced" by default.
    #[serde(default)]
    pub advanced: bool,
    /// Labels of `enum` values, same order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enum_labels: Vec<String>,
    /// Shown (and required) only when every listed field has the given value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub visible_if: BTreeMap<String, Value>,
}

/// One question of the install form (JSON Schema keywords, `x-ui` hints).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FormField {
    #[serde(rename = "type")]
    pub ty: FormFieldType,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(rename = "enum", default, skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<i64>,
    #[serde(rename = "minLength", default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u32>,
    #[serde(rename = "maxLength", default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u32>,
    /// Anchored regular expression (simple syntax: classes, groups, repetitions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<FormFormat>,
    #[serde(rename = "x-ui", default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<FormUi>,
}

/// A section of the form: its fields in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FormGroup {
    pub title: String,
    pub fields: Vec<String>,
}

/// The install form (`form.json`). Property names are `.env` names; the keys of
/// [`AppInstallRequest::form`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AppForm {
    #[serde(default)]
    pub properties: BTreeMap<String, FormField>,
    #[serde(default)]
    pub required: Vec<String>,
    /// Every property exactly once: the order and sections of the form.
    #[serde(rename = "x-groups", default)]
    pub groups: Vec<FormGroup>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebScheme {
    #[default]
    Http,
    Https,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebAuth {
    /// The app has its own login.
    #[default]
    App,
    /// No login: the proxy must add the rynas login.
    None,
    /// Trusts the proxy's identity headers.
    Proxy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebPublic {
    #[default]
    Ok,
    /// Allowed with a warning.
    Warn,
    /// Never public (Docker managers).
    Never,
}

/// The `[web]` block: how the web access proxy fronts the app (D-PROXY, D-EXPOSE).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppWeb {
    /// Name of the port in [`AppDetail::card`] ports.
    pub port: String,
    /// Entry path of the UI.
    pub path: String,
    pub scheme: WebScheme,
    pub auth: WebAuth,
    #[serde(default)]
    pub websocket: bool,
    /// Works under a path prefix.
    #[serde(default)]
    pub subpath: bool,
    /// Largest request body (`50G`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Upstream read timeout (`600s`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    #[serde(default = "yes")]
    pub buffering: bool,
    /// Path prefixes that skip the rynas login.
    #[serde(default)]
    pub bypass: Vec<String>,
    pub public: WebPublic,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_proxies_env: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FolderIntent {
    Config,
    #[default]
    Data,
    Db,
    Media,
    Cache,
}

/// May the folder live on a faster (SSD) volume than the stack?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FolderSsd {
    #[default]
    No,
    Ok,
    Preferred,
}

/// An app-owned bind folder (`[[folders]]`): `./<path>` in the stack folder by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppFolder {
    /// The key of [`AppInstallRequest::folders`] (an absolute placement).
    pub env: String,
    pub path: String,
    pub intent: FolderIntent,
    #[serde(default)]
    pub ssd: FolderSsd,
    /// `uid:gid` when the image needs it; default the `docker-apps` user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ShareAccess {
    #[default]
    Ro,
    Rw,
}

/// A shared folder of the NAS mapped into the app (`[[shares]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppShare {
    /// The key of [`AppInstallRequest::shares`] (an absolute path of a shared folder).
    pub env: String,
    pub label: String,
    pub description: String,
    pub access: ShareAccess,
    #[serde(default)]
    pub required: bool,
    /// Stack sub-folder used when an optional share is not chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// A generated secret (`[[secrets]]`): never its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppSecret {
    pub env: String,
    #[serde(default)]
    pub description: String,
    /// Shown once in the install answer (an initial administrator password).
    #[serde(default, skip_serializing_if = "not")]
    pub show: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GpuNeed {
    #[default]
    None,
    Optional,
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GpuKind {
    IntelQsv,
    Vaapi,
    Nvidia,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppHardware {
    #[serde(default)]
    pub gpu: GpuNeed,
    #[serde(default)]
    pub gpu_kinds: Vec<GpuKind>,
    /// `x86-64-v2`, `avx`, `avx2`.
    #[serde(default)]
    pub cpu_features: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NoteLevel {
    #[default]
    Info,
    Warning,
}

/// Shown in the wizard before confirming (English, from the entry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppNote {
    pub level: NoteLevel,
    pub text: String,
}

/// A compose overlay merged when a form switch (or `RYNAS_CPU_AVX2`) holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppOverlay {
    pub file: String,
    /// A boolean form field or provided variable, optionally `!`-negated.
    pub when: String,
    pub description: String,
}

/// A named Docker volume (disposable content, outside snapshots).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppNamedVolume {
    pub name: String,
    pub description: String,
}

/// `GET /api/v1/apps/catalog/{id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AppDetail {
    pub card: AppCard,
    /// Long description (Markdown, English).
    pub description: String,
    pub homepage: String,
    pub source: Option<String>,
    pub changelog_url: String,
    pub maintainers: Vec<String>,
    /// Every service on the host network: ports are fixed and only informative.
    pub host_network: bool,
    pub form: AppForm,
    pub web: Option<AppWeb>,
    pub folders: Vec<AppFolder>,
    pub shares: Vec<AppShare>,
    pub secrets: Vec<AppSecret>,
    pub hardware: AppHardware,
    pub overlays: Vec<AppOverlay>,
    pub named_volumes: Vec<AppNamedVolume>,
    pub notes: Vec<AppNote>,
    /// Relative URLs of the screenshots (`/api/v1/apps/catalog/<id>/screenshots/<file>`).
    pub screenshots: Vec<String>,
}

// ------------------------------------------------------------------ install

/// `POST /api/v1/apps/catalog/{id}/plan` and `/install`: the wizard's answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppInstallRequest {
    /// Stack name (compose project and folder name); default the app id.
    pub stack: Option<String>,
    /// Form answers by property name (strings, integers, booleans).
    pub form: BTreeMap<String, Value>,
    /// Share env → absolute path of the chosen shared folder (must exist).
    pub shares: BTreeMap<String, String>,
    /// Folder env → absolute host path, to place a folder outside the stack folder (on an
    /// SSD volume): below a mounted volume, `/<label>/...`.
    pub folders: BTreeMap<String, String>,
    /// Port env → host port, for relocatable ports that are not at their default.
    pub ports: BTreeMap<String, u16>,
    /// Bind the proxied ports on every address (`RYNAS_BIND=0.0.0.0`) instead of loopback, so
    /// the app is reachable on the LAN without the web access proxy. `None` (the default):
    /// loopback plus a web access route while web access is active, the LAN otherwise
    /// (`docs/phase2/docker-apps.md` §7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publish_lan: Option<bool>,
    /// Bring the stack up after creating it (default true).
    pub start: bool,
}

impl Default for AppInstallRequest {
    fn default() -> Self {
        Self {
            stack: None,
            form: BTreeMap::new(),
            shares: BTreeMap::new(),
            folders: BTreeMap::new(),
            ports: BTreeMap::new(),
            publish_lan: None,
            start: true,
        }
    }
}

/// Why an answer of the install request is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldErrorKind {
    /// Fails the form's rules (type, range, length, pattern, format, enum).
    InvalidValue,
    /// Required and not answered.
    MissingValue,
    /// Not a field, share, folder or port of the entry.
    UnknownValue,
    /// Contains a newline, a single quote or NUL (cannot be written to `.env`).
    UnsafeValue,
    /// The chosen shared folder does not exist.
    ShareMissing,
    /// A folder placement outside the mounted volumes, or not an absolute path.
    BadPath,
    /// The port cannot be moved, or 0.
    BadPort,
    /// Not a valid stack name, or the stack folder exists.
    BadStack,
}

/// One refused answer; `field` is the form property, share, folder or port env, or `stack`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldError {
    pub field: String,
    pub kind: FieldErrorKind,
    /// English detail.
    pub message: String,
}

/// A host port the app would take that is already used (by a container or a listening
/// socket of the host) or taken twice by the app itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortConflict {
    pub name: String,
    pub env: String,
    pub port: u16,
    pub protocol: PortProtocol,
    pub relocatable: bool,
    /// A free port to propose (send it back in [`AppInstallRequest::ports`]).
    pub suggestion: Option<u16>,
}

/// A port as it will be published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedPort {
    pub name: String,
    pub env: String,
    pub port: u16,
    pub protocol: PortProtocol,
    pub proxied: bool,
    /// Host address: `127.0.0.1` (only the proxy), `0.0.0.0` (the LAN); `None` with the host
    /// network (the app binds itself).
    pub bind: Option<String>,
    pub purpose: String,
}

/// A host folder of the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedFolder {
    pub env: String,
    /// Absolute host path.
    pub path: String,
    pub intent: Option<FolderIntent>,
    pub ssd: FolderSsd,
    /// A user's shared folder: checked, never created or chowned.
    pub share: bool,
    pub exists: bool,
    /// `uid:gid` a created folder gets.
    pub owner: String,
    /// Suggested placement on the SSD volume of the preferences (`ssd` ok or preferred).
    pub suggested: Option<String>,
}

/// Something the administrator should know before installing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanWarning {
    /// Docker is not installed: the plan is a preview, the install is refused.
    DockerNotInstalled,
    /// Docker has no volume yet (settings): the install is refused.
    NoVolume,
    /// The engine did not answer: ports of containers were not checked (listening sockets
    /// were).
    EngineUnreachable,
    /// Below the recommended RAM.
    RamBelowRecommended,
    /// Proxied ports are published on the LAN (`publish_lan`).
    PublishedOnLan,
    /// Proxied ports bind to loopback while web access is not active: the app is
    /// unreachable until web access runs.
    ProxyInactive,
}

/// `POST /api/v1/apps/catalog/{id}/plan`: what the install would do. No side effects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AppInstallPlan {
    pub app: String,
    pub version: String,
    pub stack: String,
    /// `/<vol>/docker/stacks/<stack>`; empty without a Docker volume.
    pub stack_dir: String,
    /// The install would be accepted as it is.
    pub ok: bool,
    pub errors: Vec<FieldError>,
    pub port_conflicts: Vec<PortConflict>,
    pub ports: Vec<PlannedPort>,
    pub ram: RamGate,
    pub arch_ok: bool,
    pub rynas_ok: bool,
    pub folders: Vec<PlannedFolder>,
    /// Secrets that will be generated (never their values).
    pub secrets: Vec<AppSecret>,
    pub images: Vec<String>,
    pub overlays: Vec<String>,
    pub notes: Vec<AppNote>,
    pub warnings: Vec<PlanWarning>,
    /// The `compose.yaml` that will be written (empty when the answers do not render).
    pub compose: String,
    /// The `.env` that will be written, every secret replaced by `********`.
    pub env: String,
    /// Web access is active (rynas-caddy installed, built-in mode): the default binds
    /// proxied ports to loopback and adds a route.
    #[serde(default)]
    pub proxy_active: bool,
    /// The address the app gets through web access (`https://<label>-<host>.local/`), when
    /// the install adds a route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
}

/// A generated secret the entry marks `show`: in the install answer only, once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShownSecret {
    pub env: String,
    pub description: String,
    pub value: String,
}

/// `POST /api/v1/apps/catalog/{id}/install` (202).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppInstallStarted {
    pub task_id: String,
    pub stack: String,
    pub stack_dir: String,
    pub secrets: Vec<ShownSecret>,
}

// ------------------------------------------------------------------ installed apps

/// A stack installed from the catalogue (its `compose.yaml` has an `x-rynas` block).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InstalledApp {
    pub stack: String,
    /// The entry id of `x-rynas.app`.
    pub app: String,
    /// The catalogue name, or the id when the entry left the catalogue.
    pub name: String,
    /// Installed package version and upstream version.
    pub version: String,
    pub app_version: String,
    /// `x-rynas.catalog`.
    pub catalog: String,
    /// The catalogue still has this entry, and its version.
    pub in_catalog: bool,
    pub catalog_version: Option<String>,
    pub update_available: bool,
    /// `None` when the engine did not answer.
    pub state: Option<StackState>,
    pub containers: u32,
    pub running: u32,
    pub stack_dir: String,
    /// Host port of the web UI (from `.env`) and its path, for the Open button.
    pub web_port: Option<u16>,
    pub web_path: Option<String>,
    /// `RYNAS_BIND` of the stack: `127.0.0.1` (behind the proxy) or `0.0.0.0` (LAN).
    pub bind: Option<String>,
    /// The web access route of the stack (`https://<label>-<host>.local/`), when it has one;
    /// the Open button uses it with loopback binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
    /// The stack folder is a btrfs subvolume: updates take a snapshot first.
    pub snapshots: bool,
}

/// `GET /api/v1/apps/installed/{stack}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InstalledAppDetail {
    pub app: InstalledApp,
    /// Newest first.
    pub snapshots: Vec<StackSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageChangeKind {
    Added,
    Removed,
    /// The tag changed (and so the digest).
    Tag,
    /// Same tag, new digest.
    Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageChange {
    pub service: String,
    pub kind: ImageChangeKind,
    /// Full references (`repo:tag@sha256:...`).
    pub from: Option<String>,
    pub to: Option<String>,
}

/// `GET /api/v1/apps/installed/{stack}/update`: what an update would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppUpdatePlan {
    pub stack: String,
    pub app: String,
    pub from_version: String,
    pub to_version: String,
    pub from_app_version: String,
    pub to_app_version: String,
    /// Same version and images: nothing to do.
    pub up_to_date: bool,
    /// The catalogue is older than the installed entry: refused.
    pub downgrade: bool,
    /// Versions of the value migrations that will run, in order.
    pub migrations: Vec<String>,
    pub image_changes: Vec<ImageChange>,
    /// Required form fields the stored values do not answer: send them as
    /// [`AppUpdateRequest::answers`].
    pub missing_values: Vec<String>,
    pub changelog_url: String,
    /// A snapshot is taken first (the stack folder is a subvolume).
    pub snapshot: bool,
}

/// `POST /api/v1/apps/installed/{stack}/update`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppUpdateRequest {
    /// Answers for [`AppUpdatePlan::missing_values`].
    pub answers: BTreeMap<String, Value>,
    /// Take a snapshot first (default: the Docker setting).
    pub snapshot: Option<bool>,
    /// Stop the stack while the snapshot is taken (default: the Docker setting).
    pub stop_first: Option<bool>,
}

/// `POST /api/v1/apps/installed/{stack}/rollback`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppRollbackRequest {
    /// A snapshot id of the stack (`YYYYMMDDTHHMMSS`).
    pub snapshot: String,
}

/// `POST /api/v1/apps/installed/{stack}/uninstall`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppUninstallRequest {
    /// Also delete the stack folder (compose, `.env`, data), its snapshots and folders
    /// placed outside it. Default false: `down` only, everything stays on disk.
    pub delete_data: bool,
    /// The stack name, required with `delete_data`.
    pub confirm: Option<String>,
}
