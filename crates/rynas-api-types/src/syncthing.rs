//! Syncthing integrated (`/api/v1/syncthing/...`), `docs/phase2/syncthing.md`. Phase S1a:
//! adoption of an existing Syncthing instance into the catalogue app.
//!
//! - [`SyncthingAdoptPreviewRequest`] → [`SyncthingAdoptPreview`] (no side effects): the
//!   instances found on this NAS ([`AdoptCandidate`]) and, for the chosen one, its files,
//!   folders mapped to `/shares/<share>/...`, markers, `.stignore` proposals and owners.
//! - [`SyncthingAdoptRequest`] → task `syncthing.adopt`.
//!
//! Never in these types: the API key, the GUI password, the keys; device ids only as their
//! first 7 characters.

use serde::{Deserialize, Serialize};

fn not(b: &bool) -> bool {
    !*b
}

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
    /// The account the app runs as (`docker-apps`): it gets read and write access to each
    /// share (ACL), whoever owns the files.
    pub app_uid: u32,
    pub app_gid: u32,
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
    /// The shares the app will mount (each at `/shares/<share>`).
    pub shares: Vec<String>,
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
}
