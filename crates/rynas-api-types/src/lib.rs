//! Types of the rynas HTTP API (`/api/v1`).
//!
//! Shared by `rynas-web` (server), `rynas-ui` (wasm) and plugins, so they must compile on
//! `wasm32-unknown-unknown`: serde and serde_json only, no std-only dependencies.
//!
//! Units: bytes, bytes per second, percent `0.0..=100.0`, degrees Celsius, milliseconds since
//! the Unix epoch for timestamps, seconds for durations.

use serde::{Deserialize, Serialize};

pub mod network;
pub mod notify;
pub mod power;
pub mod scheduler;
mod services;
pub mod snapshots;
pub mod storage;
pub mod synology;
pub mod validate;
pub use services::*;

/// `GET /api/v1/system`: static or slow-changing facts about the machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SystemInfo {
    pub hostname: String,
    /// `PRETTY_NAME` from `/etc/os-release`, e.g. "Debian GNU/Linux 13 (trixie)".
    pub os: String,
    pub kernel: String,
    /// DMI vendor and product, e.g. "Synology Inc. DS923+"; empty if unknown.
    pub model: String,
    pub cpu_model: String,
    pub cpu_threads: u32,
    pub mem_total: u64,
    pub uptime_s: u64,
    pub rynas_version: String,
}

/// One point of the live metrics, sampled once per second.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MetricsSample {
    pub ts_ms: u64,
    /// Total CPU busy time over the last interval.
    pub cpu_pct: f32,
    pub cpu_per_thread: Vec<f32>,
    /// 1, 5 and 15 minute load averages.
    pub load: [f32; 3],
    pub mem: MemUsage,
    pub net: Vec<NetRate>,
    pub disks: Vec<DiskRate>,
    pub temps: Vec<Temperature>,
    /// Initialized zram devices (compressed swap in RAM); absent when there are none.
    /// Live samples only: `/metrics/history` carries it on its newest point.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub zram: Vec<ZramDevice>,
}

/// One zram device (`/sys/block/zramN`): sizes in bytes from `mm_stat`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ZramDevice {
    /// "zram0".
    pub name: String,
    /// Selected compression algorithm (`comp_algorithm`, the one in brackets), e.g. "zstd".
    pub algorithm: String,
    /// Uncompressed capacity (`disksize`).
    pub disksize: u64,
    /// Data stored, uncompressed (`orig_data_size`).
    pub data: u64,
    /// Data stored, compressed (`compr_data_size`).
    pub compressed: u64,
    /// Physical RAM the device uses, allocator overhead included (`mem_used_total`).
    pub mem_used: u64,
    /// Pages stored uncompressed because they did not compress (`huge_pages`).
    #[serde(default)]
    pub huge_pages: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct MemUsage {
    pub total: u64,
    /// total - available (what applications really use).
    pub used: u64,
    /// page cache + reclaimable slab.
    pub cache: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

/// Throughput of a network interface (physical ones, bonds, bridges, VLANs; not `lo`, veth).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NetRate {
    pub name: String,
    pub rx_bps: u64,
    pub tx_bps: u64,
    pub up: bool,
    /// Link speed in Mbit/s when known.
    pub speed_mbps: Option<u32>,
}

/// I/O of a whole block device (sdX, nvmeXnY, mdX), not partitions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DiskRate {
    pub name: String,
    pub read_bps: u64,
    pub write_bps: u64,
    /// Share of the interval the device was busy (`io_ticks`).
    pub busy_pct: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Temperature {
    /// Stable id, e.g. "cpu", "nvme0", "bay2", "nic:eth0".
    pub id: String,
    /// Human label, e.g. "CPU", "NVMe 0", "Bay 2".
    pub label: String,
    pub celsius: Option<f32>,
}

/// `GET /api/v1/storage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StorageOverview {
    pub disks: Vec<Disk>,
    pub arrays: Vec<RaidArray>,
    pub filesystems: Vec<Filesystem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Disk {
    /// Kernel name, e.g. "sda", "nvme0n1".
    pub name: String,
    pub model: String,
    pub serial: String,
    pub size: u64,
    pub rotational: bool,
    /// "sata", "nvme", "usb", "sas", … as reported by the kernel/lsblk.
    pub transport: String,
    /// Bay number when a hardware provider maps it (Synology), else None.
    pub bay: Option<u32>,
    /// Never wakes the disk to find out (A16: the same type as `DiskSummary`).
    pub power_state: storage::PowerState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RaidArray {
    /// e.g. "md2".
    pub name: String,
    /// e.g. "raid5".
    pub level: String,
    pub size: u64,
    /// Member devices, e.g. ["sda3", "sdb3"].
    pub members: Vec<String>,
    /// "clean", "active", "degraded", "resyncing", "recovering", …
    pub state: String,
    /// Progress of a resync/recovery/check, 0..=100.
    pub sync_pct: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Filesystem {
    pub mountpoint: String,
    /// Source device, e.g. "/dev/md2".
    pub source: String,
    /// "btrfs", "ext4", "xfs", …
    pub fstype: String,
    pub size: u64,
    pub used: u64,
    pub avail: u64,
}

/// `GET /api/v1/hardware`: platform hardware through a provider (e.g. synohwd on Synology).
/// `provider` is None and the lists are empty when the machine has no provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HardwareStatus {
    /// e.g. "synology".
    pub provider: Option<String>,
    pub model: String,
    pub fans: Vec<Fan>,
    pub bays: Vec<Bay>,
    /// M.2 NVMe slots, by slot number (unknown numbers last). Absent from older providers.
    #[serde(default)]
    pub m2: Vec<M2Slot>,
    /// Fan duty set by the provider, 0..=100.
    pub fan_duty: Option<u8>,
    pub fan_fail: bool,
    /// Front status LED as a word: "green", "green-blink", "orange", "orange-blink", "off".
    pub status_led: Option<String>,
    /// Recent provider events, newest last: (time "YYYY-MM-DD HH:MM:SS", text).
    pub events: Vec<(String, String)>,
    /// Age of the provider's last update; the UI flags data older than a few periods.
    pub age_s: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Fan {
    pub label: String,
    pub rpm: Option<u32>,
    pub alarm: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Bay {
    pub number: u32,
    pub present: bool,
    pub powered: bool,
    /// Block device in the bay, e.g. "sda".
    pub device: Option<String>,
    pub celsius: Option<f32>,
    pub asleep: bool,
}

/// An M.2 NVMe slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct M2Slot {
    /// Physical slot as printed on the board; None when the provider does not know which
    /// slot holds the drive (the UI then says "M.2" without a number).
    pub number: Option<u32>,
    pub present: bool,
    /// Block device in the slot, e.g. "nvme0n1".
    pub device: Option<String>,
    pub celsius: Option<f32>,
}

/// Error body of every non-2xx API response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ApiError {
    /// Machine-readable code, e.g. "not_found", "bad_request", "internal".
    pub code: String,
    pub message: String,
    /// Form field the error is about, named as in the request's JSON body (`email`,
    /// `workgroup`, `label`…), when the server knows it (guidelines A10). The UI shows the
    /// translated `code` under that field instead of in the form banner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            field: None,
        }
    }
}

/// Every value of [`ApiError::code`]: the single registry (guidelines A11). Adding a code =
/// a constant here (and in [`error_codes::ALL`]), a line in the table of `docs/phase1/plan.md`
/// and an `err.<code>` i18n key in the UI.
pub mod error_codes {
    /// No valid session cookie (401).
    pub const UNAUTHENTICATED: &str = "unauthenticated";
    /// Wrong user name or password (401). Same answer and timing for unknown users.
    pub const INVALID_CREDENTIALS: &str = "invalid_credentials";
    /// Password accepted but the account has TOTP enabled and `totp` was missing (401).
    pub const TOTP_REQUIRED: &str = "totp_required";
    /// Wrong TOTP or recovery code (401 at login, 400 elsewhere).
    pub const INVALID_TOTP: &str = "invalid_totp";
    /// The user authenticated but has no role in rynas (403).
    pub const FORBIDDEN: &str = "forbidden";
    /// Missing or wrong `X-CSRF-Token` header on a state-changing request (403).
    pub const CSRF: &str = "csrf";
    /// Too many failed logins; see the `Retry-After` header (429).
    pub const RATE_LIMITED: &str = "rate_limited";
    /// No authentication backend reachable (503).
    pub const AUTH_UNAVAILABLE: &str = "auth_unavailable";
    /// TOTP is already enabled; disable it first (409).
    pub const TOTP_ALREADY_ENABLED: &str = "totp_already_enabled";
    /// TOTP confirm/disable without an enrollment or with TOTP off (409).
    pub const TOTP_NOT_ENROLLED: &str = "totp_not_enrolled";
    /// Malformed request: bad query parameter or path (400). JSON bodies get the more
    /// precise codes below.
    pub const BAD_REQUEST: &str = "bad_request";
    /// The body is not JSON at all (syntax error, cut short) (400).
    pub const INVALID_JSON: &str = "invalid_json";
    /// A required field of the body is missing; `field` names it (400).
    pub const MISSING_FIELD: &str = "missing_field";
    /// The body has a field the endpoint does not know; `field` names it (400).
    pub const UNKNOWN_FIELD: &str = "unknown_field";
    /// A field of the body has the wrong JSON type, e.g. a string for a number; `field`
    /// names it (400).
    pub const INVALID_TYPE: &str = "invalid_type";
    /// No such object: revision, task, configuration key (404).
    pub const NOT_FOUND: &str = "not_found";
    /// A value failed validation: out of range, not one of the accepted values, refused by
    /// a domain rule (400); `field` names it when known.
    pub const INVALID_VALUE: &str = "invalid_value";
    /// Apply or restore with an empty changeset (409).
    pub const NOTHING_PENDING: &str = "nothing_pending";
    /// A staged value was based on a configuration that changed since: discard and stage
    /// again (409).
    pub const CONFLICT: &str = "conflict";
    /// Another apply is running, or too many tasks are queued (409).
    pub const BUSY: &str = "busy";
    /// Cancel of a task that has already ended (409).
    pub const TASK_FINISHED: &str = "task_finished";
    /// `rynasd` or a provider is not reachable (503); the message names which.
    pub const BACKEND_UNAVAILABLE: &str = "backend_unavailable";
    /// The object already exists (409).
    pub const ALREADY_EXISTS: &str = "already_exists";
    /// A destructive request without the confirmation it needs (400).
    pub const CONFIRMATION_REQUIRED: &str = "confirmation_required";
    /// The object is only in a pending change: apply it first (409), e.g. setting the
    /// password of a user that does not exist on the system yet.
    pub const NOT_APPLIED: &str = "not_applied";
    /// Unknown method on a known path (405).
    pub const METHOD_NOT_ALLOWED: &str = "method_not_allowed";
    /// Request body over the endpoint's limit (413).
    pub const PAYLOAD_TOO_LARGE: &str = "payload_too_large";
    /// Body without the content type the endpoint needs, e.g. JSON (415).
    pub const UNSUPPORTED_MEDIA_TYPE: &str = "unsupported_media_type";
    /// Unexpected server-side failure; details are in the server log (500).
    pub const INTERNAL: &str = "internal";
    /// `rynasd` answered something rynas-web could not understand (502).
    pub const BAD_GATEWAY: &str = "bad_gateway";
    /// `rynasd` (or a provider) did not answer in time (504).
    pub const TIMEOUT: &str = "timeout";
    /// File manager: the filesystem refused the operation for the user's permissions or ACLs
    /// (403). Not `forbidden`, which means the user has no role in rynas.
    pub const PERMISSION_DENIED: &str = "permission_denied";
    /// File manager: the volume is full or the user's quota is used up (507).
    pub const INSUFFICIENT_STORAGE: &str = "insufficient_storage";
    /// File manager: the item cannot change — inside an archive, a read-only snapshot, a
    /// read-only filesystem (409).
    pub const READ_ONLY: &str = "read_only";
    /// File manager: a shared folder itself cannot be renamed, copied, moved or deleted from
    /// the file manager (400).
    pub const SHARE_ROOT: &str = "share_root";
    /// File manager: the archive is protected by a password and none (or a wrong one) was
    /// given (400); the job message carries the same code.
    pub const PASSWORD_REQUIRED: &str = "password_required";
    /// File manager: an optional program the feature needs is not installed (7z, unrar,
    /// ffmpeg...) (503); the message names it.
    pub const TOOL_MISSING: &str = "tool_missing";
    /// File manager: a remote SMB/SFTP server cannot be reached or broke the connection (502).
    pub const REMOTE_UNREACHABLE: &str = "remote_unreachable";
    /// File manager: a remote server refused the user name, password or key (400; never 401,
    /// which would end the rynas session).
    pub const REMOTE_AUTH_FAILED: &str = "remote_auth_failed";
    /// File manager: an SFTP server presented a key other than the one pinned at the first
    /// connection (409).
    pub const HOST_KEY_CHANGED: &str = "host_key_changed";
    /// The feature is not available on this system (501), e.g. ejecting a drive when no
    /// removable-drives module is present.
    pub const NOT_AVAILABLE: &str = "not_available";

    /// The code of a status when nothing more precise applies: used to turn framework
    /// rejections (bad query string, 405, 413...) into an `ApiError`.
    pub fn for_status(status: u16) -> &'static str {
        match status {
            401 => UNAUTHENTICATED,
            403 => FORBIDDEN,
            404 => NOT_FOUND,
            405 => METHOD_NOT_ALLOWED,
            408 | 504 => TIMEOUT,
            409 => CONFLICT,
            413 => PAYLOAD_TOO_LARGE,
            415 => UNSUPPORTED_MEDIA_TYPE,
            429 => RATE_LIMITED,
            502 => BAD_GATEWAY,
            503 => BACKEND_UNAVAILABLE,
            400..=499 => BAD_REQUEST,
            _ => INTERNAL,
        }
    }

    /// Every code above, for registry checks (tests, i18n completeness).
    pub const ALL: &[&str] = &[
        UNAUTHENTICATED,
        INVALID_CREDENTIALS,
        TOTP_REQUIRED,
        INVALID_TOTP,
        FORBIDDEN,
        CSRF,
        RATE_LIMITED,
        AUTH_UNAVAILABLE,
        TOTP_ALREADY_ENABLED,
        TOTP_NOT_ENROLLED,
        BAD_REQUEST,
        INVALID_JSON,
        MISSING_FIELD,
        UNKNOWN_FIELD,
        INVALID_TYPE,
        NOT_FOUND,
        INVALID_VALUE,
        NOTHING_PENDING,
        CONFLICT,
        BUSY,
        TASK_FINISHED,
        BACKEND_UNAVAILABLE,
        ALREADY_EXISTS,
        CONFIRMATION_REQUIRED,
        NOT_APPLIED,
        METHOD_NOT_ALLOWED,
        PAYLOAD_TOO_LARGE,
        UNSUPPORTED_MEDIA_TYPE,
        INTERNAL,
        BAD_GATEWAY,
        TIMEOUT,
        PERMISSION_DENIED,
        INSUFFICIENT_STORAGE,
        READ_ONLY,
        SHARE_ROOT,
        PASSWORD_REQUIRED,
        TOOL_MISSING,
        REMOTE_UNREACHABLE,
        REMOTE_AUTH_FAILED,
        HOST_KEY_CHANGED,
        NOT_AVAILABLE,
    ];
}

/// Name of the session cookie (`HttpOnly; SameSite=Strict`).
pub const SESSION_COOKIE: &str = "rynas_session";
/// Header carrying [`Session::csrf_token`] on every non-GET request.
pub const CSRF_HEADER: &str = "X-CSRF-Token";
/// Request header (value `1`) of requests the client makes on its own — timers, polling,
/// reconnects — rather than for a user action: they do not renew the session's idle
/// timeout. Event streams (`/events`, `*/stream`) never renew it either.
pub const BACKGROUND_HEADER: &str = "X-Rynas-Background";

/// `POST /api/v1/auth/login`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    /// 6-digit TOTP code or a recovery code; required when the account has TOTP enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp: Option<String>,
}

/// What a user may do. Only `Admin` exists for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Admin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UserInfo {
    pub username: String,
    /// Where the account lives: "pam" (system users, via rynasd) or "rynas" (internal realm).
    pub realm: String,
    pub role: Role,
}

/// Answer of login and `GET /api/v1/auth/session`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Session {
    pub user: UserInfo,
    /// Send it back in the `X-CSRF-Token` header on every non-GET request.
    pub csrf_token: String,
    pub totp_enabled: bool,
    pub created_ms: u64,
    /// The session ends at this time whatever the activity.
    pub expires_ms: u64,
    /// The session ends at this time unless there is activity before.
    pub idle_expires_ms: u64,
    pub idle_timeout_s: u64,
}

/// Answer of `POST /api/v1/auth/totp/enroll`: the pending secret, to be confirmed with a code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TotpEnrollment {
    /// Base32 secret (RFC 4648, no padding), for manual entry.
    pub secret: String,
    /// `otpauth://totp/...` URI; the UI renders it as a QR code.
    pub otpauth_uri: String,
    pub issuer: String,
    pub account: String,
    pub digits: u32,
    pub period_s: u32,
}

/// `POST /api/v1/auth/totp/confirm` and `POST /api/v1/auth/totp/disable`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TotpCode {
    /// Current 6-digit code (disable also accepts a recovery code).
    pub code: String,
}

/// Alias kept for readability at call sites.
pub type TotpConfirm = TotpCode;

/// Answer of `POST /api/v1/auth/totp/confirm`: shown once, stored only as hashes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TotpConfirmed {
    pub recovery_codes: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Configuration: pending changes, apply, revisions, drift (`/api/v1/config/*`)
// ---------------------------------------------------------------------------------------------

/// Kind of a [`DiffEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DiffOp {
    Added,
    Removed,
    #[default]
    Changed,
}

/// One line of a semantic diff, e.g. `smb.shares[media].read_only: false → true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DiffEntry {
    /// Configuration key followed by the path inside its document: `.field`, and `[name]`
    /// for list items that have a `name` or `id`.
    pub path: String,
    pub op: DiffOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<serde_json::Value>,
}

/// One staged change of a configuration key (a domain document such as `system.hostname`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PendingChange {
    pub key: String,
    /// Value when the change was staged (`None`: the key had no value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<serde_json::Value>,
    /// Staged value (`None`: the key is removed, the domain is no longer managed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    pub staged_ms: u64,
    /// The applied value changed after staging: apply refuses until the change is staged again.
    pub conflict: bool,
    /// Number of entries of the semantic diff (always set).
    #[serde(default)]
    pub diff_count: u32,
    /// The semantic diff; only with `?detail=1` (empty in the compact answer).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diff: Vec<DiffEntry>,
}

/// `GET /api/v1/config/pending`: the changeset of the current user. By default compact —
/// key, `staged_ms`, `conflict` and `diff_count` of each change, what the pending-changes bar
/// polls; `?detail=1` adds `base`, `value` and `diff` (the review dialog).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PendingChanges {
    /// Changesets are per user: the owner is the logged-in user name.
    pub owner: String,
    pub changes: Vec<PendingChange>,
}

/// Data of the `config.pending` event of `GET /api/v1/events`: some changeset may have
/// changed (a stage, a discard, an apply — which can also flip the `conflict` flags of
/// other users' changes). Re-read `GET /api/v1/config/pending` with `If-None-Match`: the
/// answer is `304` when the user's own changeset is the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PendingEvent {
    /// Increases with every change; starts again from 0 when rynasd restarts.
    pub generation: u64,
}

/// `POST /api/v1/config/apply`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ApplyRequest {
    /// Free text stored with the revision.
    #[serde(default)]
    pub comment: String,
}

/// Answer of the endpoints that start a background task (202).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TaskStarted {
    pub task_id: String,
}

/// An applied changeset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RevisionSummary {
    /// Increasing, never reused.
    pub id: u64,
    pub author: String,
    pub ts_ms: u64,
    pub comment: String,
    /// Keys changed by the revision.
    pub keys: Vec<String>,
    /// Set when the changeset was staged by "restore revision N".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_from: Option<u64>,
}

/// `GET /api/v1/config/revisions/{n}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RevisionDetail {
    pub revision: RevisionSummary,
    /// Semantic diff against the previous configuration.
    pub diff: Vec<DiffEntry>,
    /// Files written by the apply.
    pub files: Vec<String>,
}

/// Answer of `POST /api/v1/config/revisions/{n}/restore` and of a drift import.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StagedChanges {
    /// Number of keys staged; review them with `GET /api/v1/config/pending`.
    pub staged: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DriftStatus {
    /// The file changed after rynas wrote it.
    #[default]
    Modified,
    /// The file is gone.
    Missing,
}

/// A managed file that no longer matches what rynas wrote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DriftEntry {
    pub path: String,
    /// Configuration key whose apply wrote the file; resolve with
    /// `POST /api/v1/config/drift/{key}/import` or `.../overwrite`.
    pub key: String,
    pub status: DriftStatus,
    pub expected_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_sha256: Option<String>,
}

/// `GET /api/v1/config/drift`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DriftReport {
    /// Managed files checked.
    pub checked: u32,
    /// Only the files that drifted.
    pub entries: Vec<DriftEntry>,
    /// Differences of the system state that are not files (users, Samba accounts, ACLs).
    #[serde(default)]
    pub state: Vec<StateDrift>,
}

/// Kind of a [`StateDrift`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StateDriftKind {
    /// In the configuration but not on the system (e.g. a user deleted with `userdel`).
    #[default]
    Missing,
    /// On the system, in the range rynas manages, but not in its configuration (created
    /// outside rynas).
    Unmanaged,
    /// The object exists but differs from the configuration (attributes, ACL entries).
    Mismatch,
    /// A user with SMB access has no Samba account (its password must be set again).
    MissingInSamba,
    /// A Samba account without a Linux user.
    MissingInPasswd,
}

/// A difference between the configuration and the system state that is not a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StateDrift {
    /// Configuration key: resolve with `.../drift/{key}/import` or `.../overwrite`.
    pub key: String,
    /// What drifted, e.g. `user:alice`, `group:staff`, `share:photos`.
    pub object: String,
    pub kind: StateDriftKind,
    /// Human-readable explanation.
    pub detail: String,
}

/// Document of the `system.hostname` key, body of `PUT /api/v1/system/hostname`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HostnameConfig {
    /// RFC 1123 label: 1-63 of `a-z 0-9 -`, not starting or ending with `-`.
    pub hostname: String,
}

/// `GET /api/v1/system/hostname` (and answer of `PUT`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HostnameState {
    /// Applied configuration (or what the system has when rynas never applied it).
    pub applied: HostnameConfig,
    /// Staged by the current user, not applied yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<HostnameConfig>,
    /// Hostname the kernel reports right now.
    pub running: String,
}

// ---------------------------------------------------------------------------------------------
// Tasks (`/api/v1/tasks/*`)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// Waiting for a free slot (limit of concurrent tasks).
    #[default]
    Queued,
    Running,
    Ok,
    Error,
    Cancelled,
}

impl TaskStatus {
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Ok | Self::Error | Self::Cancelled)
    }
}

/// A background task (Proxmox-style UPID).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TaskInfo {
    /// `UPID:<node>:<pid>:<seq>:<start>:<type>:<user>:` (hex numbers).
    pub id: String,
    /// What the task does, e.g. "config.apply".
    #[serde(rename = "type")]
    pub kind: String,
    pub user: String,
    pub start_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<u64>,
    pub status: TaskStatus,
    /// 0..=100 when the task reports progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    /// Error message of a failed task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `event:` name of the SSE stream `GET /api/v1/tasks/stream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TaskEventKind {
    #[default]
    Started,
    /// Status or progress changed.
    Updated,
    Finished,
}

/// Data of an event of `GET /api/v1/tasks/stream`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TaskEvent {
    pub kind: TaskEventKind,
    pub task: TaskInfo,
}

/// Response header of `GET /api/v1/tasks/{id}/log`: offset to ask for next.
pub const TASK_LOG_NEXT_OFFSET: &str = "X-Rynas-Next-Offset";
/// Response header of `GET /api/v1/tasks/{id}/log`: "true" once the task has ended and the
/// log was read to the end.
pub const TASK_LOG_COMPLETE: &str = "X-Rynas-Log-Complete";
