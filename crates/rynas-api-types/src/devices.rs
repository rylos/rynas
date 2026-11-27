//! Mobile devices: pairing by QR code, device tokens, management (`docs/phase2/mobile-devices.md`,
//! protocol `rynas-apk/docs/pairing-protocol.md` v1 with the differences listed there).
//!
//! Times are Unix milliseconds in `*_ms` fields like the rest of the API; `ts` in the signed
//! messages stays Unix seconds. Encodings: `b64u` = base64url without padding; a public key is
//! the DER SubjectPublicKeyInfo of a P-256 key; `key_hash` = `b64u(SHA-256(DER SPKI))`.

use serde::{Deserialize, Serialize};

/// Version of the pairing protocol (`v` in the QR code and the claim, `api` in `/ping`).
pub const PROTOCOL_VERSION: u32 = 1;
/// Lifetime of a pairing code.
pub const PAIRING_TTL_S: u64 = 300;
/// Open pairings (waiting or claimed) a user may have at once.
pub const MAX_OPEN_PAIRINGS: usize = 3;
/// Longest wait of `POST /devices/pairing/poll`.
pub const POLL_WAIT_S: u64 = 25;
/// Lifetime of an access token.
pub const ACCESS_TTL_S: u64 = 900;
/// A refresh token replaced by a rotation is still accepted (with the device's signature)
/// for this long, in case the answer was lost.
pub const REFRESH_GRACE_S: u64 = 60;
/// Accepted distance between the server's clock and `ts` of a signed message.
pub const MAX_CLOCK_SKEW_S: u64 = 120;
/// A device without a refresh for this long expires.
pub const INACTIVE_EXPIRY_DAYS: u64 = 180;
/// A pairing created more than this long after the user's last password or TOTP check is
/// refused with `reauth_required`.
pub const REAUTH_WINDOW_S: u64 = 600;
/// Longest device name (characters, after trimming).
pub const MAX_NAME: usize = 64;
/// A revoked device's id and reason are kept this long: its refresh (or a call of its app
/// that reaches the store) is answered `device_revoked` with the `reason`; later without.
pub const REVOKED_KEPT_DAYS: u64 = 30;

/// `reason` of a `clock_skew` answer: which clock is likely wrong. The answer's `Date` header
/// carries the NAS' time.
pub mod clock_skew_reason {
    /// The NAS clock is not synchronized (phones take their time from the network); the
    /// administrator gets a notification pointing to Date and time.
    pub const NAS: &str = "nas_clock";
    /// The NAS clock is synchronized: the device's clock is off.
    pub const DEVICE: &str = "device_clock";
}

/// `reason` of a `device_revoked` answer and of the `device.revoked` event: why the device was
/// revoked. Absent for a device the NAS does not know (never paired, or revoked more than
/// [`REVOKED_KEPT_DAYS`] ago).
pub mod revoke_reason {
    /// Unpaired from the app itself.
    pub const SELF: &str = "self";
    /// Revoked by a user (its owner or an administrator) in the web interface.
    pub const USER: &str = "user";
    /// Approved but never picked up by the phone, or never refreshed after the pairing.
    pub const UNCLAIMED: &str = "unclaimed";
    /// The user name now belongs to another account (another uid).
    pub const ACCOUNT_REPLACED: &str = "account_replaced";
    /// The account no longer exists.
    pub const ACCOUNT_REMOVED: &str = "account_removed";
    /// The account is locked or expired.
    pub const ACCOUNT_DISABLED: &str = "account_disabled";
    /// An old refresh token came back: a copy may exist elsewhere.
    pub const REFRESH_REUSE: &str = "refresh_reuse";
    /// No refresh for [`super::INACTIVE_EXPIRY_DAYS`].
    pub const INACTIVE: &str = "inactive";
    pub const ALL: &[&str] = &[
        SELF,
        USER,
        UNCLAIMED,
        ACCOUNT_REPLACED,
        ACCOUNT_REMOVED,
        ACCOUNT_DISABLED,
        REFRESH_REUSE,
        INACTIVE,
    ];
}

/// Routes a device may not call (403 `not_for_devices`): `(method, path prefix)` relative to
/// `/api/v1`, `"*"` for any method, a `*` path segment for any one segment. A device acts as
/// its user in v1 (everything a bare-metal PC restore needs stays allowed, the restore code
/// included), except showing an encrypted folder's recovery key: only in the web interface.
pub const NOT_FOR_DEVICES: &[(&str, &str)] = &[
    ("*", "/crypt/folders/*/*/reveal-recovery-key"),
    ("*", "/crypt/folders/*/*/recovery-key"),
];

/// `GET /api/v1/ping` (no session): who answers on this address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Ping {
    /// This NAS' stable identity (16 random bytes, b64u).
    pub sid: String,
    pub name: String,
    pub version: String,
    pub api: u32,
}

/// What a phone says about itself when it claims a pairing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeviceInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub app_version: String,
}

/// Answer of `POST /api/v1/devices/pairing`: render `qr` as a QR code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairingCreated {
    pub pairing_id: String,
    pub expires_ms: u64,
    /// `rynas://pair?v=1&sid=…&n=…&c=…&u=…&p=…&exp=…`
    pub qr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    #[default]
    Waiting,
    Claimed,
    Approved,
    Rejected,
    Expired,
}

/// The phone that claimed a pairing, shown to the user before approving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairingClaim {
    pub device: DeviceInfo,
    /// `"482 913"`: the same digits are shown on the phone.
    pub compare_code: String,
    pub ip: String,
    pub claimed_ms: u64,
}

/// `GET /api/v1/devices/pairing/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairingStatus {
    pub state: PairingState,
    pub expires_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<PairingClaim>,
}

/// `POST /api/v1/devices/pairing/{id}/approve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairingApprove {
    /// Default: the name the phone sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `POST /api/v1/devices/pairing/claim` (no session). Signed message:
/// `rynas-pair-v1\n{sid}\n{code}\n{key_hash}\n{ts}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ClaimRequest {
    pub v: u32,
    pub sid: String,
    pub code: String,
    pub public_key: String,
    #[serde(default)]
    pub device: DeviceInfo,
    pub ts: i64,
    pub sig: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ClaimResponse {
    pub claim_token: String,
    pub compare_code: String,
    pub poll_after_ms: u64,
    pub expires_ms: u64,
}

/// `POST /api/v1/devices/pairing/poll` (no session).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PollRequest {
    pub claim_token: String,
}

/// 202 of the poll: still waiting for the user's approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PollPending {
    pub state: PairingState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NasRef {
    pub sid: String,
    pub name: String,
}

/// 200 of the poll: the device exists; its tokens (given out once).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PollApproved {
    pub device_id: String,
    pub name: String,
    pub user: String,
    pub nas: NasRef,
    pub refresh_token: String,
    pub access_token: String,
    pub access_expires_in: u64,
}

/// `POST /api/v1/auth/device/refresh` (no session). Signed message:
/// `rynas-refresh-v1\n{sid}\n{device_id}\n{ts}\n{b64u(SHA-256(refresh_token))}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RefreshRequest {
    pub device_id: String,
    pub refresh_token: String,
    pub ts: i64,
    pub sig: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RefreshResponse {
    pub access_token: String,
    pub access_expires_in: u64,
    pub refresh_token: String,
}

/// A paired device (`GET /api/v1/devices`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub user: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub app_version: String,
    pub created_ms: u64,
    pub last_seen_ms: u64,
    #[serde(default)]
    pub last_ip: String,
    pub key_hash: String,
    /// A push subscription is registered.
    #[serde(default)]
    pub push: bool,
    /// The device making this request.
    #[serde(default)]
    pub current: bool,
}

/// `GET /api/v1/devices/self` (bearer): the device, the TLS pins and the base URLs (the same
/// as in the QR code), so the app can update them before a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeviceSelf {
    #[serde(flatten)]
    pub device: Device,
    /// SPKI SHA-256 (b64u) of the local CA first, then of the served leaf.
    #[serde(default)]
    pub tls_pins: Vec<String>,
    #[serde(default)]
    pub endpoints: Vec<String>,
}

/// `PATCH /api/v1/devices/{id}`, `PATCH /api/v1/devices/self`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeviceRename {
    /// 1-[`MAX_NAME`] characters after trimming (checked by rynasd: 400 `invalid_value`).
    #[serde(default)]
    pub name: String,
}

/// `PUT /api/v1/devices/self/push` (bearer): the device's Web Push subscription (its
/// UnifiedPush endpoint). `endpoint` https only; `p256dh` the 65-byte uncompressed P-256
/// point, `auth` 16 bytes, both b64u.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

/// Largest push payload (plaintext JSON, before encryption); the body is shortened to fit.
pub const PUSH_MAX_PAYLOAD: usize = 3072;
/// Longest endpoint URL accepted.
pub const PUSH_MAX_ENDPOINT: usize = 2048;

/// The plaintext of a push message: the app posts it as a notification and dedupes by `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PushPayload {
    pub v: u32,
    /// The notification's id.
    pub id: u64,
    pub title: String,
    pub body: String,
    pub severity: String,
    pub category: String,
    pub source: String,
    pub ts_ms: u64,
    /// A file transfer's events (`files.job_*`): the job, how it ended (`done`, `failed`,
    /// `canceled`) and its owner; absent otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub job_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// Something the app offers to do from the notification (approve a PC's bare-metal
    /// restore); absent for plain notifications. Added after rynas 0.9.16: older apps ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<PushAction>,
}

/// [`PushPayload::action`]. `kind` `pc_restore` (the only one so far): a restore TUI waits for
/// an administrator's approval (`docs/phase2/pc-restore-approval.md`); the app shows the
/// claimed PC and the address, asks the approver to type the six digits the PC's screen shows
/// (or reads them from that screen's QR code), checks the user locally (biometrics), then
/// calls `POST /api/v1/pcbackup/restore-requests/{request}/approve` or `…/deny`. The code is
/// never in the push.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PushAction {
    pub kind: String,
    /// The request's id.
    pub request: String,
    /// The PC as the requester claimed it.
    pub pc: String,
    /// The backup PC the claim matches, `null` when none or several do (a suggestion from
    /// the claim, not a check).
    pub device: Option<String>,
    /// Where the request came from.
    pub from_ip: String,
    /// Over TLS; `false`: plain HTTP (red warning).
    pub tls: bool,
    /// Other waiting requests for the same PC.
    pub same_pc: u32,
    pub expires_ms: u64,
}

/// `PUT /api/v1/devices/self/push` (bearer): the subscription and, optionally, the language
/// of the device's notification target (`en`, `it`, `ru`; anything else is English). Without
/// `lang` a new target is English and an existing one keeps its language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PushRegistration {
    #[serde(flatten)]
    pub subscription: PushSubscription,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

/// The languages a device may ask for its notifications.
pub const PUSH_LANGUAGES: [&str; 3] = ["en", "it", "ru"];

/// Where every release asset of the project lives; an Android app URL must start with it.
pub const ANDROID_RELEASE_PREFIX: &str = "https://github.com/rylos/rynas/releases/download/";
/// The newest Android app, always at this address (release tag `android`, never "Latest"):
/// the web interface links it directly, so the download works without the metadata.
pub const ANDROID_APK_URL: &str = "https://github.com/rylos/rynas/releases/download/android/rynas-android.apk";
/// The metadata of [`ANDROID_APK_URL`] (`android.json`), read by the NAS, not the browser
/// (GitHub's downloads have no CORS headers).
pub const ANDROID_METADATA_URL: &str = "https://github.com/rylos/rynas/releases/download/android/android.json";

/// `GET /api/v1/devices/android-app` (any signed-in user): the Android app published on
/// GitHub, `{"state":"available", ...}`, `{"state":"unavailable"}` when the NAS could not
/// read the metadata (offline, GitHub down) or `{"state":"invalid"}` when it read a file it
/// cannot trust (a malformed or incomplete release). Never an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AndroidApp {
    Available(AndroidRelease),
    #[default]
    Unavailable,
    Invalid,
}

/// The published Android app, as checked by the NAS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AndroidRelease {
    /// The app's version name (`0.2.0`).
    pub version: String,
    /// The size of the APK in bytes.
    pub size: u64,
    /// SHA-256 of the APK, lowercase hex.
    pub sha256: String,
    /// SHA-256 of the signing certificate, lowercase hex.
    pub signer_sha256: String,
    /// The APK's address, under [`ANDROID_RELEASE_PREFIX`].
    pub url: String,
}

/// The language code of a `lang` sent with a push registration: one of [`PUSH_LANGUAGES`],
/// English for anything else.
pub fn push_language(lang: &str) -> &'static str {
    let l = lang.trim().to_ascii_lowercase();
    PUSH_LANGUAGES.into_iter().find(|c| *c == l).unwrap_or("en")
}

/// The name of a device as stored: trimmed, control characters dropped, 1-[`MAX_NAME`]
/// characters; `None` when nothing is left.
pub fn clean_name(raw: &str) -> Option<String> {
    let s: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_NAME)
        .collect();
    let s = s.trim_end().to_string();
    (!s.is_empty()).then_some(s)
}

/// A free text the phone sends (model, OS, app version): control characters dropped, at most
/// [`MAX_NAME`] characters.
pub fn clean_text(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Internal protocol between rynas-web and rynasd (`/v1/devices/…` on rynasd's socket; never
/// public). rynas-web holds pairings and access tokens in memory; rynasd keeps the device
/// store and checks refreshes (it has the keys and the system accounts).
pub mod internal {
    use super::*;

    /// `GET /v1/devices/sid`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
    pub struct Sid {
        pub sid: String,
    }

    /// `GET /v1/devices/trust`: what a QR code and `devices/self` carry.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
    pub struct Trust {
        pub sid: String,
        /// Base URLs, https only, LAN → Tailscale → own domains.
        pub endpoints: Vec<String>,
        /// SPKI SHA-256 (b64u): local CA first, then the served leaf.
        pub tls_pins: Vec<String>,
    }

    /// `GET /v1/devices/live`: a live device and its user's groups now (rynas-web ends the
    /// access tokens of devices not listed, and of users whose role is gone). `groups`:
    /// empty for a user that no longer exists, `None` when the lookup failed (nothing is
    /// decided then).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
    pub struct Live {
        pub id: String,
        #[serde(default)]
        pub groups: Option<Vec<String>>,
    }

    /// `POST /v1/devices/register` (`x-rynas-user`: the approving user): an approved pairing.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
    pub struct Register {
        pub name: String,
        pub device: DeviceInfo,
        /// DER SPKI, b64u.
        pub public_key: String,
        pub ip: String,
        /// b64u(SHA-256(refresh token)).
        pub refresh_sha: String,
    }

    /// `POST /v1/devices/refresh`: rynasd checks the signature, the account and the token,
    /// and rotates to `new_refresh_sha`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
    pub struct RefreshCheck {
        pub device_id: String,
        pub ts: i64,
        pub sig: String,
        pub refresh_sha: String,
        pub new_refresh_sha: String,
        pub ip: String,
        /// rynas-web's administrator groups: rynasd rotates only when the user is in one of
        /// them, so rynas-web never drops a rotated token for want of a role.
        #[serde(default)]
        pub admin_groups: Vec<String>,
    }

    /// `ApiError::field` of a refresh refused for what only a guesser gets wrong: an unknown
    /// device id (`device_revoked`) or a signature of another key (`refresh_invalid`).
    /// rynas-web counts exactly these in the device limiter, and drops the field before
    /// answering the phone. Every other refusal (a `ts` not newer, a role, the account, a
    /// token reuse with a valid signature) comes without it and counts nothing.
    pub const GUESS_DEVICE_ID: &str = "device_id";
    pub const GUESS_SIGNATURE: &str = "sig";

    /// Answer of a refresh check: the device, its user's groups (rynas-web derives the role)
    /// and whether the previous token was accepted in its grace window.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
    pub struct RefreshOk {
        pub device: Device,
        pub groups: Vec<String>,
        #[serde(default)]
        pub grace: bool,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  Phone\n of Ann "), Some("Phone of Ann".into()));
        assert_eq!(clean_name(" \t "), None);
        assert_eq!(clean_name(&"x".repeat(80)).unwrap().chars().count(), MAX_NAME);
        assert_eq!(clean_text("SM-S931B\u{7}"), "SM-S931B");
    }

    #[test]
    fn device_self_is_flat() {
        let d = DeviceSelf {
            device: Device {
                id: "d_1".into(),
                ..Default::default()
            },
            tls_pins: vec!["a".into()],
            endpoints: vec![],
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["id"], "d_1");
        assert_eq!(v["tls_pins"][0], "a");
    }
}
