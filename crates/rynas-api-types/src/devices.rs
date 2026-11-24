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

/// Routes a device may not call (403 `not_for_devices`): `(method, path prefix)` relative to
/// `/api/v1`, `"*"` for any method. A device acts as its user in v1, so the list is empty; it
/// stays the one place per-device scopes will extend.
pub const NOT_FOR_DEVICES: &[(&str, &str)] = &[];

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
        assert_eq!(clean_name("  Phone\n of Marco "), Some("Phone of Marco".into()));
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
