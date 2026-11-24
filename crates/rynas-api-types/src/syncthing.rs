//! Syncthing integrated (`/api/v1/syncthing/...`), `docs/phase2/syncthing.md`. Phase S1a:
//! adoption of an existing Syncthing instance into the catalogue app.
//!
//! - [`SyncthingAdoptPreviewRequest`] → [`SyncthingAdoptPreview`] (no side effects): the
//!   instances found on this NAS ([`AdoptCandidate`]) and, for the chosen one, its files,
//!   folders mapped to `/shares/<share>/...`, markers, `.stignore` proposals and owners.
//! - [`SyncthingAdoptRequest`] → task `syncthing.adopt`.
//! - Phase S1b: [`SyncthingState`] (`GET /api/v1/syncthing`), the folders' actions
//!   (`POST .../folders/{id}/pause|resume|rescan`) and [`SyncthingSettings`].
//!
//! Never in these types: the API key, the GUI password, the keys; device ids only as their
//! first 7 characters.

use serde::{Deserialize, Serialize};

fn not(b: &bool) -> bool {
    !*b
}

/// The `run_as` value naming the apps' account (`docker-apps`) explicitly, in a request
/// and in [`SyncthingAdoptPreview::run_as`].
pub const RUN_AS_APPS: &str = "@apps";

/// Where an instance to adopt comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AdoptSourceKind {
    /// The catalogue app installed with the older layout (one share at `/data`); `id` is the
    /// stack.
    #[default]
    App,
    /// Another container on this host; `id` is the container id or name.
    Container,
    /// A Syncthing run by systemd on the host (`syncthing@<user>`, a user unit) or by hand;
    /// `id` is the user.
    Service,
    /// A Syncthing home folder copied from elsewhere; `path` is the folder holding
    /// `config.xml`.
    Path,
}

/// The chosen source of an adoption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdoptSourceRef {
    pub kind: AdoptSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// One line of the volume table: a folder as the old instance saw it → the same folder on
/// this NAS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdoptVolume {
    pub container: String,
    pub host: String,
}

/// `POST /api/v1/syncthing/adopt/preview`. Without a source: only the candidates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SyncthingAdoptPreviewRequest {
    pub source: Option<AdoptSourceRef>,
    /// Lines added to (and overriding) the volume table read from the source.
    pub volumes: Vec<AdoptVolume>,
    /// The NAS user the app will run as, instead of the proposal
    /// ([`SyncthingAdoptPreview::run_as`]).
    pub run_as: Option<String>,
}

/// An instance found on this NAS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdoptCandidate {
    pub source: AdoptSourceRef,
    /// Container name, unit name, stack or folder, for people.
    pub name: String,
    /// Its home folder on this NAS (holding `config.xml`), when found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// It runs now (two instances with one device id must never run together).
    pub running: bool,
    /// Container image, unit, or the app version.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// The database of the home.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AdoptDatabase {
    /// No database: Syncthing scans every folder again (nothing is lost, it takes time).
    #[default]
    None,
    /// `index-v2` (Syncthing 2).
    V2,
    /// `index-v0.14.0.db` (Syncthing 1): migrated on the first start, slowly.
    V1,
}

/// What the home holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdoptHomeFiles {
    pub cert: bool,
    pub key: bool,
    pub config: bool,
    /// `https-cert.pem` / `https-key.pem` of the GUI.
    pub https_cert: bool,
    pub database: AdoptDatabase,
}

/// The `.stfolder` check of a folder on this NAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MarkerState {
    /// The marker folder holds the file named after this folder's id.
    Verified,
    /// A marker folder without that file (older Syncthing): the folder is paused after
    /// adoption, resume it in Syncthing once checked.
    Unverified,
    /// No marker: the folder is paused after adoption and never "fixed" by rynas.
    #[default]
    Missing,
    /// Not checked (the folder is not mapped to this NAS).
    Unknown,
}

/// A `.stignore` change proposed for a folder (applied only when the adoption asks).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StignoreProposal {
    pub before: String,
    pub after: String,
    /// Lines removed (patterns of other NAS systems).
    pub removed: Vec<String>,
    /// Lines added.
    pub added: Vec<String>,
    /// Lines of other systems' patterns kept: the folder holds such folders, which the line
    /// keeps out of the synchronization (removed only when none exist).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<String>,
}

/// Why a folder is not mapped to a share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnmappedReason {
    /// No line of the volume table covers its path: add one.
    NoVolume,
    /// Its folder on this NAS is outside every share: make a share of it, or leave it paused.
    NoShare,
    /// A relative or otherwise unusable path.
    BadPath,
}

/// A device of the configuration (never its full id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdoptDevice {
    /// The first 7 characters of the device id.
    pub id_short: String,
    pub name: String,
}

/// A folder of the configuration and where it lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdoptFolder {
    pub id: String,
    pub label: String,
    /// The path in the old configuration.
    pub path: String,
    /// `sendreceive`, `sendonly`, `receiveonly`, `receiveencrypted`.
    pub folder_type: String,
    /// Short ids of the devices it is shared with.
    pub devices: Vec<String>,
    pub paused: bool,
    pub marker_name: String,
    /// The folder on this NAS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_path: Option<String>,
    /// The rynas share holding it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<String>,
    /// The path inside the app (`/shares/<share>/<rest>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unmapped: Option<UnmappedReason>,
    pub marker: MarkerState,
    /// Paused after adoption (unmapped, or the marker is not verified).
    pub will_pause: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stignore: Option<StignoreProposal>,
}

/// Who owns the synchronized folders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdoptOwners {
    /// The most common owner of the folder roots and their top-level entries.
    pub uid: u32,
    pub gid: u32,
    /// Top-level entries with another owner.
    pub others: u32,
    /// The NAS user with that uid, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

/// `POST /api/v1/syncthing/adopt/preview`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SyncthingAdoptPreview {
    pub candidates: Vec<AdoptCandidate>,
    /// The chosen source, when the request named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<AdoptCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    pub files: AdoptHomeFiles,
    /// The GUI address of the old configuration (the app listens on its own port).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gui_address: Option<String>,
    pub devices: Vec<AdoptDevice>,
    pub folders: Vec<AdoptFolder>,
    /// The volume table used (read from the source, then the request's lines).
    pub volumes: Vec<AdoptVolume>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owners: Option<AdoptOwners>,
    /// The NAS user the app will run as: the request's `run_as`, else the installed app's
    /// user, else the user owning most synchronized files (`owners.user`); [`RUN_AS_APPS`]
    /// (or `None` when nothing could be proposed): the apps' account. Files arriving from the
    /// devices belong to this user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_as: Option<String>,
    /// The shares the app will mount (each at `/shares/<share>`).
    pub shares: Vec<String>,
    /// The share permission entries the adoption adds or raises for the app's user (they
    /// also let that user into the share over SMB and NFS).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<crate::apps::PlannedShareGrant>,
    /// A container source started by a compose project of another tool: it may come back
    /// with `compose up` until the project is removed there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose: Option<AdoptCompose>,
    /// The old GUI served HTTPS itself: the copy turns it off (`tls="false"`), rynas' proxy
    /// serves the interface over HTTPS.
    #[serde(default, skip_serializing_if = "not")]
    pub gui_tls_off: bool,
    /// Sync listen addresses other than `default` or port 22000 (the app publishes 22000):
    /// the adoption needs `reset_listen` to set them back to `default`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listen_addresses: Vec<String>,
    /// The catalogue app installed now (stack), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
    /// The adoption can run (the files are there and nothing blocks it; a running source
    /// still needs `stop_source`).
    pub ok: bool,
    /// Why not, or what to know (English).
    pub problems: Vec<String>,
}

/// `POST /api/v1/syncthing/adopt`: task `syncthing.adopt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SyncthingAdoptRequest {
    pub source: AdoptSourceRef,
    pub volumes: Vec<AdoptVolume>,
    /// Stop (and disable) the old instance first; without it a running source is refused.
    #[serde(skip_serializing_if = "not")]
    pub stop_source: bool,
    /// Write the proposed `.stignore` changes.
    #[serde(skip_serializing_if = "not")]
    pub apply_stignore: bool,
    /// Stack name of a new install (default `syncthing`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// The NAS user the app runs as ([`RUN_AS_APPS`]: the apps' account), instead of the
    /// preview's proposal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_as: Option<String>,
    /// Set custom sync listen addresses ([`SyncthingAdoptPreview::listen_addresses`]) back
    /// to `default`; without it such a source is refused.
    #[serde(skip_serializing_if = "not")]
    pub reset_listen: bool,
}

/// The compose project a container source belongs to (its labels).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdoptCompose {
    /// `com.docker.compose.project`.
    pub project: String,
    /// `com.docker.compose.project.working_dir`, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// S1b: state of the installed app (`GET /api/v1/syncthing`), folder actions, settings
// ---------------------------------------------------------------------------------------------

/// The installed catalogue app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SyncthingInstalled {
    pub stack: String,
    /// The catalogue entry's version.
    pub version: String,
    /// Its container runs.
    pub running: bool,
}

/// This NAS as a Syncthing device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SyncthingSelf {
    /// The first 7 characters of the device id.
    pub id_short: String,
    pub name: String,
    /// Syncthing's version (`v2.0.10`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
}

/// One line of `/rest/folder/errors`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SyncthingItemError {
    pub path: String,
    pub error: String,
}

/// How far a remote device is with a folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SyncthingCompletion {
    pub id_short: String,
    pub name: String,
    /// 0 to 100.
    pub completion: f64,
    pub need_bytes: u64,
    pub connected: bool,
}

/// A folder of the installed app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SyncthingFolder {
    pub id: String,
    pub label: String,
    /// The path inside the app (`/shares/<share>/<rest>`).
    pub path: String,
    /// The share holding it (the share set item), when the path is below one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<String>,
    /// The part below the share (empty: the share itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_path: Option<String>,
    /// `sendreceive`, `sendonly`, `receiveonly`, `receiveencrypted`.
    pub folder_type: String,
    /// Syncthing's state: `idle`, `scanning`, `scan-waiting`, `syncing`, `sync-waiting`,
    /// `sync-preparing`, `cleaning`, `error`, `paused`, `unknown`.
    pub state: String,
    pub paused: bool,
    /// Paused by rynas while its encrypted share is locked: resumed after the unlock.
    pub paused_by_rynas: bool,
    /// Its share is an encrypted folder that is locked now.
    pub locked: bool,
    /// The folder's own error (state `error`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The first 20 item errors.
    pub errors: Vec<SyncthingItemError>,
    /// How many item errors there are in all.
    pub error_count: u32,
    pub need_bytes: u64,
    pub global_bytes: u64,
    pub in_sync_bytes: u64,
    /// The remote devices it is shared with.
    pub completion: Vec<SyncthingCompletion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scan_ms: Option<u64>,
    /// The last file changed (path inside the folder).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_file: Option<String>,
    /// Conflict copies made since Syncthing started (its counter), plus those seen since.
    pub conflicts: u32,
    /// The latest conflict copies that still exist (paths inside the folder, newest first, at
    /// most 50).
    pub conflict_files: Vec<String>,
}

/// A remote device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SyncthingDevice {
    pub id_short: String,
    pub name: String,
    pub connected: bool,
    pub paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// `tcp-client`, `quic-server`, `relay-client`...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_type: Option<String>,
    /// Last connection (`/rest/stats/device`); `None`: never seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<u64>,
    /// Labels of the folders shared with it.
    pub folders: Vec<String>,
}

/// A share the app synchronizes and whether snapshots keep its versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SyncthingShareNote {
    pub share: String,
    /// An enabled scheduled snapshot job takes snapshots of it.
    pub snapshots_scheduled: bool,
}

/// `GET|PUT /api/v1/syncthing/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncthingSettings {
    /// Alert when a device seen before has not connected for this many days (0: off).
    pub device_unseen_days: u32,
    /// Alert on new conflict copies.
    pub conflicts_alert: bool,
}

impl Default for SyncthingSettings {
    fn default() -> Self {
        Self { device_unseen_days: 7, conflicts_alert: true }
    }
}

/// Largest [`SyncthingSettings::device_unseen_days`].
pub const MAX_UNSEEN_DAYS: u32 = 365;

/// `GET /api/v1/syncthing[?stack=]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SyncthingState {
    /// `None`: the catalogue app is not installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<SyncthingInstalled>,
    /// Other installed stacks of the app (`?stack=` shows one of them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_stacks: Vec<String>,
    /// Its REST interface answers.
    pub reachable: bool,
    /// Why not (English).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<SyncthingSelf>,
    pub folders: Vec<SyncthingFolder>,
    pub devices: Vec<SyncthingDevice>,
    pub shares: Vec<SyncthingShareNote>,
    pub settings: SyncthingSettings,
    /// When the state was last read in full from Syncthing (ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_ms: Option<u64>,
}
