//! Paired mobile devices, root side (`docs/phase2/mobile-devices.md`).
//!
//! rynas-web runs the pairing (codes and claims in memory) and holds the access tokens in
//! memory; rynasd keeps what must survive a restart and what needs root:
//!
//! - `<state>/devices/devices.json` (0600, written atomically): each device's owner, name,
//!   model, public key (DER SPKI), SHA-256 of the current refresh token and of the previous
//!   one during its grace window, last refresh. No token in clear.
//! - `<state>/nas-sid`: this NAS' identity for the app (16 random bytes, b64u), made once.
//! - The refresh check: the device's signature, the account (it exists, is not locked, PAM
//!   account management accepts it, it may sign in), rotation with reuse detection.
//! - Every 10 minutes: devices without a refresh for 180 days expire; devices never refreshed
//!   after their pairing (`unclaimed`), of removed or disabled accounts, or of a name that now
//!   belongs to another uid (`account_replaced`) are revoked.
//!
//! Internal routes (rynas-web only, never public; the user in `x-rynas-user`):
//!
//! | Method | Path | Body | Answer |
//! |---|---|---|---|
//! | GET | `/v1/devices/sid` | | `internal::Sid` |
//! | GET | `/v1/devices/trust` | | `internal::Trust` (sid, base URLs, TLS pins) |
//! | GET | `/v1/devices/live` | | `Vec<internal::Live>`: the live devices and their user's groups (rynas-web's sweep every 10 s ends the access tokens of the others and of users who lost the role) |
//! | POST | `/v1/devices/register` | `internal::Register` | `Device` (the approving user owns it) |
//! | POST | `/v1/devices/refresh` | `internal::RefreshCheck` | `internal::RefreshOk`; 401 `device_revoked` / `refresh_invalid` (`field` set only for guesses: `internal::GUESS_*`), 400 `clock_skew` |
//! | GET | `/v1/devices` | | `Vec<Device>`: the user's; administrators everyone's |
//! | PATCH | `/v1/devices/{id}` | `DeviceRename` | `Device` (owner or administrator) |
//! | DELETE | `/v1/devices/{id}?reason=user\|self` | | 204 (owner or administrator) |
//! | GET | `/v1/devices/{id}/self` | | `DeviceSelf` (the owner: `devices/self` of the app) |
//! | PUT | `/v1/devices/{id}/push` | `PushSubscription` | `Device` (the owner); creates the `device` notification target and its rule (warning and above) |
//! | DELETE | `/v1/devices/{id}/push` | | 204 (the owner); removes the target |
//! | GET | `/v1/devices/android-app` | | `AndroidApp` (no user needed; [`crate::android_app`]) |
//!
//! Push: the subscription is kept with the device; the notification engine's `device` target
//! names the device and finds the subscription through [`PushDirectory`]; a 404/410 of the
//! push service drops it. Revoking a device removes its target.

use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use rynas_api_types::devices::internal::{
    GUESS_DEVICE_ID, GUESS_SIGNATURE, Live, RefreshCheck, RefreshOk, Register, Sid, Trust,
};
use rynas_api_types::devices::{
    ACCESS_TTL_S, Device, DeviceRename, DeviceSelf, INACTIVE_EXPIRY_DAYS, PAIRING_TTL_S,
    PushRegistration, PushSubscription, REFRESH_GRACE_S, REVOKED_KEPT_DAYS, clean_name, clean_text,
    clock_skew_reason, push_language, revoke_reason,
};
use rynas_api_types::notify::{
    FieldMatch, NotifyCategory, NotifyLanguage, NotifyRule, NotifySeverity, TargetConfig, TargetRequest,
};
use rynas_notify::engine::webpush::{PushDirectory, parse_subscription};
use rynas_api_types::error_codes;
use rynas_auth::Clock;
use rynas_auth::backend::BoxFuture;
use rynas_auth::device::{
    DELIVERY_GRACE_MS, b64u, b64u_decode, ct_eq_bytes, key_hash, new_id, p256_point,
    refresh_message, ts_ok, verify,
};
use rynas_notify::Notifier;
use rynas_notify::checks::{
    device_clock_ok, device_clock_skew, device_paired, device_refresh_reuse, device_renamed, device_revoked,
};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::ops::user;
use crate::roles::AdminPolicy;

type Res<T> = Result<T, Error>;

/// How often expired devices and removed accounts are looked for.
pub const CHECK_EVERY: Duration = Duration::from_secs(600);
const DAY_MS: u64 = 86_400_000;
/// Devices kept at most (a registration beyond is refused).
pub const MAX_DEVICES: usize = 500;
/// A device never refreshed this long after its approval is revoked (`unclaimed`): the
/// code's lifetime, the tokens' wait for the phone's poll, and the first access token's
/// lifetime are over, so nobody can hold a working token of it (rynas-web restarted before
/// the phone picked them up, say).
pub const UNCLAIMED_AFTER_MS: u64 = (PAIRING_TTL_S + ACCESS_TTL_S) * 1000 + DELIVERY_GRACE_MS;

/// What the account behind a device is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Account {
    Ok,
    /// No such user any more.
    Gone,
    /// Locked (`!`/`*` in shadow) or expired.
    Disabled,
    /// The check failed (NSS or PAM error): nothing is decided, nothing is revoked.
    Unknown,
}

/// A user's groups: `Ok(None)` when the user does not exist, `Err` when the lookup failed.
pub type Lookup = fn(&str) -> Result<Option<Vec<String>>, String>;
/// A user's numeric id: `Ok(None)` when the user does not exist, `Err` when the lookup failed
/// ([`crate::groups::user_uid`]).
pub type UidLookup = fn(&str) -> Result<Option<u32>, String>;

/// The account check (blocking): PAM in production ([`crate::pam::account_state`]).
pub type AccountCheck = Arc<dyn Fn(&str) -> Account + Send + Sync>;
/// The base URLs (LAN → Tailscale → own domains) and the TLS pins, for QR codes and
/// `devices/self`.
pub type TrustProvider = Arc<dyn Fn() -> BoxFuture<'static, (Vec<String>, Vec<String>)> + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Record {
    id: String,
    user: String,
    name: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    os: String,
    #[serde(default)]
    app_version: String,
    /// DER SPKI, b64u.
    public_key: String,
    created_ms: u64,
    last_seen_ms: u64,
    #[serde(default)]
    last_ip: String,
    /// The last refresh (or the pairing): 180 days after it the device expires.
    last_refresh_ms: u64,
    /// b64u(SHA-256) of the current refresh token.
    refresh_sha: String,
    /// Tokens replaced in the last [`REFRESH_GRACE_S`], still accepted with a valid signature
    /// (a lost answer, two refreshes at once): each until its own end.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    prev: Vec<PrevToken>,
    /// `ts` of the last accepted refresh: the next one must be newer (no replay of a captured
    /// request inside the grace window).
    #[serde(default)]
    last_ts: i64,
    /// The Web Push subscription (endpoint, keys of the app): what the `device` notification
    /// target of this device delivers to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    push: Option<PushSubscription>,
    /// The owner's uid at the pairing (filled at the next refresh for older records): a
    /// different uid under the same name is another account (removed, then made again), and
    /// revokes the device (`account_replaced`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uid: Option<u32>,
    /// The `files` rule of the device's target was made (with the target, or once by the
    /// migration of targets older than that rule): never made again, so a rule the
    /// administrator deleted stays deleted.
    #[serde(default)]
    files_rule_added: bool,
    /// b64u(SHA-256) of the token replaced most recently: presented after its grace window
    /// (an app that lost the answer and waited) it is refused with `refresh_invalid` and the
    /// device stays; only an older token is a reuse.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    last_replaced: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PrevToken {
    sha: String,
    until_ms: u64,
}

/// What is left of a revoked device for [`REVOKED_KEPT_DAYS`]: its id and why, so its app
/// hears the reason. No key, no token hash.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct Tombstone {
    id: String,
    /// One of [`revoke_reason`].
    reason: String,
    revoked_ms: u64,
}

/// Tombstones kept at most (the oldest go first).
const MAX_TOMBSTONES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Registry {
    #[serde(default)]
    devices: Vec<Record>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    revoked: Vec<Tombstone>,
}

impl Registry {
    fn bury(&mut self, id: &str, reason: &str, now: u64) {
        self.revoked.retain(|t| t.id != id);
        self.revoked.push(Tombstone {
            id: id.to_string(),
            reason: reason.to_string(),
            revoked_ms: now,
        });
        if self.revoked.len() > MAX_TOMBSTONES {
            self.revoked.remove(0);
        }
    }

    fn unbury(&mut self, id: &str) {
        self.revoked.retain(|t| t.id != id);
    }

    fn tombstone(&self, id: &str) -> Option<&Tombstone> {
        self.revoked.iter().find(|t| t.id == id)
    }

    /// The answer of a device's own route (`devices/self`, push) whose device is not there
    /// for its user: another user's device is 404; a revoked one is `device_revoked` with its
    /// reason (without one once the tombstone is gone).
    fn missing(&self, id: &str) -> Error {
        if self.devices.iter().any(|d| d.id == id) {
            return not_found();
        }
        match self.tombstone(id) {
            Some(t) => revoked().reason(&t.reason),
            None => revoked(),
        }
    }
}

pub struct DevicesSvc {
    dir: PathBuf,
    /// `<state>/nas-sid`, read once ([`load_sid`]); an error stays an error.
    sid: Result<String, String>,
    reg: Mutex<Registry>,
    notifier: Arc<dyn Notifier>,
    admins: AdminPolicy,
    /// The user's groups: `Ok(None)` for a user that does not exist, `Err` when the lookup
    /// failed ([`crate::groups::user_groups_checked`]).
    lookup: Lookup,
    uid_of: UidLookup,
    account: AccountCheck,
    trust: TrustProvider,
    clock: Arc<dyn Clock>,
    /// Whether the NAS clock is synchronized ([`host_clock_synced`]; replaced in tests).
    sync_probe: Mutex<SyncProbe>,
    /// A [`device_clock_skew`] alarm is open.
    skew_open: std::sync::atomic::AtomicBool,
    /// The notification engine: one `device` target per push subscription.
    engine: std::sync::OnceLock<rynas_notify::engine::Engine>,
}

pub type DevState = Option<Arc<DevicesSvc>>;

/// Answers whether the NAS clock is synchronized.
pub type SyncProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// The kernel's view (`adjtimex`, read only): synchronized unless `STA_UNSYNC` is set or the
/// state is `TIME_ERROR`, as `timedatectl` reports it.
pub fn host_clock_synced() -> bool {
    // SAFETY: modes 0 only reads; the struct is plain data
    let mut t: libc::timex = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::adjtimex(&mut t) };
    r >= 0 && r != libc::TIME_ERROR && t.status & libc::STA_UNSYNC == 0
}

/// The engine's view of the push subscriptions ([`PushDirectory`]).
struct PushDir(std::sync::Weak<DevicesSvc>);

impl PushDirectory for PushDir {
    fn subscription(&self, device_id: &str) -> Option<PushSubscription> {
        let svc = self.0.upgrade()?;
        let reg = svc.lock();
        reg.devices
            .iter()
            .find(|d| d.id == device_id)
            .and_then(|d| d.push.clone())
    }

    fn owner(&self, device_id: &str) -> Option<String> {
        let svc = self.0.upgrade()?;
        let reg = svc.lock();
        reg.devices.iter().find(|d| d.id == device_id).map(|d| d.user.clone())
    }

    fn gone(&self, device_id: &str) {
        let Some(svc) = self.0.upgrade() else { return };
        let id = device_id.to_string();
        // called from the delivery loop: the store and the engine's database off its thread
        tokio::task::spawn_blocking(move || svc.push_gone(&id));
    }
}

/// The name of a device's notification target and rule: its name and the end of its id
/// (names are unique in the engine; two phones may share a name).
fn target_name(device_id: &str, name: &str) -> String {
    let tail = &device_id[device_id.len().saturating_sub(6)..];
    format!("{name} · {tail}")
}

/// The rule that brings `user`'s own file transfers (`files.job_*`, category `files`) to the
/// device's target `target`: every severity, only events whose `user` is the owner (the
/// engine also refuses other users' events for a device target).
fn files_rule(device_id: &str, name: &str, user: &str, target: u64) -> NotifyRule {
    NotifyRule {
        name: format!("{} · Files", target_name(device_id, name)),
        enabled: true,
        categories: vec![NotifyCategory::Files],
        fields: vec![FieldMatch { field: "user".into(), value: user.into(), regex: false }],
        targets: vec![target],
        ..Default::default()
    }
}

fn audit(msg: std::fmt::Arguments) {
    tracing::info!(target: "rynas::audit", "devices: {msg}");
}

fn revoked() -> Error {
    Error::new(
        StatusCode::UNAUTHORIZED,
        error_codes::DEVICE_REVOKED,
        "device unknown or revoked".into(),
    )
}

fn refresh_invalid() -> Error {
    Error::new(
        StatusCode::UNAUTHORIZED,
        error_codes::REFRESH_INVALID,
        "refresh refused".into(),
    )
}

fn clock_skew() -> Error {
    Error::new(
        StatusCode::BAD_REQUEST,
        error_codes::CLOCK_SKEW,
        "the device's clock is more than 120 s away from the NAS'".into(),
    )
}

fn unavailable(what: &str) -> Error {
    Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        error_codes::BACKEND_UNAVAILABLE,
        format!("{what}: try again later"),
    )
}

fn not_found() -> Error {
    Error::not_found("no such device")
}

/// The revocation reason of an account that is gone or disabled.
fn account_reason(a: Account) -> &'static str {
    match a {
        Account::Disabled => revoke_reason::ACCOUNT_DISABLED,
        _ => revoke_reason::ACCOUNT_REMOVED,
    }
}

fn valid_sha(s: &str) -> bool {
    s.len() == 43 && b64u_decode(s).is_some_and(|b| b.len() == 32)
}

/// Writes `data` to `path` through a temporary file (0600, synced) and a rename.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// This NAS' identity for the app: made (16 random bytes, b64u) only when the file does not
/// exist. Any other failure, or a file that does not hold an id, is an error and the file is
/// left as it is.
pub fn load_sid(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let s = s.trim();
            if s.len() == 22 && b64u_decode(s).is_some_and(|b| b.len() == 16) {
                Ok(s.to_string())
            } else {
                Err("not a NAS id (left as it is)".into())
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut b = [0u8; 16];
            getrandom::fill(&mut b).map_err(|e| e.to_string())?;
            let sid = b64u(&b);
            write_private(path, format!("{sid}\n").as_bytes()).map_err(|e| e.to_string())?;
            Ok(sid)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// LAN base URLs: the IPv4 addresses that are not loopback, link-local or Tailscale's
/// (100.64.0.0/10), with the HTTPS port.
pub fn lan_urls(addrs: &[std::net::IpAddr], port: u16) -> Vec<String> {
    let mut out = Vec::new();
    for a in addrs {
        if let std::net::IpAddr::V4(v4) = a {
            let o = v4.octets();
            let cgnat = o[0] == 100 && (o[1] & 0xc0) == 64;
            if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || cgnat {
                continue;
            }
            let u = format!("https://{v4}:{port}");
            if !out.contains(&u) {
                out.push(u);
            }
        }
    }
    out
}

/// The running system's base URLs and pins: every LAN IPv4 address with the HTTPS port, the
/// MagicDNS name and Tailscale IPv4 address while Tailscale is up, each own domain the proxy
/// serves the interface on; the local CA's pin and the served leaf's.
pub async fn system_trust(
    tls: Arc<crate::tls::TlsSvc>,
    tailscale: crate::tailscale::TsState,
    web: Arc<crate::web_access::WebAccessSvc>,
    port: u16,
) -> (Vec<String>, Vec<String>) {
    let (pins, mut urls, domains) = tokio::task::spawn_blocking(move || {
        let (_, addrs) = (crate::tls::TlsSvc::system_probe())();
        (tls.device_pins(), lan_urls(&addrs, port), web.ui_domain_urls())
    })
    .await
    .unwrap_or_default();
    if let Some(ts) = tailscale {
        let s = ts.status(false).await;
        if s.running && s.online {
            if let Some(n) = s.dns_name.filter(|n| !n.is_empty()) {
                urls.push(format!("https://{n}:{port}"));
            }
            for a in s.addresses.iter().filter(|a| a.parse::<std::net::Ipv4Addr>().is_ok()) {
                urls.push(format!("https://{a}:{port}"));
            }
        }
    }
    for u in domains {
        if !urls.contains(&u) {
            urls.push(u);
        }
    }
    (urls, pins)
}

impl DevicesSvc {
    /// `state_dir`: rynasd's state folder (`devices/` and `nas-sid` go there).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state_dir: &Path,
        notifier: Arc<dyn Notifier>,
        admins: AdminPolicy,
        lookup: Lookup,
        uid_of: UidLookup,
        account: AccountCheck,
        trust: TrustProvider,
        clock: Arc<dyn Clock>,
    ) -> std::io::Result<Arc<Self>> {
        let dir = state_dir.join("devices");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let reg = match std::fs::read(dir.join("devices.json")) {
            Ok(b) => serde_json::from_slice(&b).map_err(std::io::Error::other)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Registry::default(),
            Err(e) => return Err(e),
        };
        let sid = load_sid(&state_dir.join("nas-sid"));
        if let Err(e) = &sid {
            tracing::error!("devices: the NAS id {}: {e}", state_dir.join("nas-sid").display());
        }
        let svc = Arc::new(Self {
            dir,
            sid,
            reg: Mutex::new(reg),
            notifier,
            admins,
            lookup,
            uid_of,
            account,
            trust,
            clock,
            sync_probe: Mutex::new(Arc::new(host_clock_synced)),
            skew_open: std::sync::atomic::AtomicBool::new(false),
            engine: std::sync::OnceLock::new(),
        });
        Ok(svc)
    }

    /// Ties the devices to the notification engine: `device` targets find their subscription
    /// here, and subscriptions create and remove their targets. Targets made before the
    /// `files` rule existed get it once ([`DevicesSvc::add_missing_files_rules`]).
    pub fn bind_engine(self: &Arc<Self>, engine: rynas_notify::engine::Engine) {
        engine.set_push_directory(Arc::new(PushDir(Arc::downgrade(self))));
        let _ = self.engine.set(engine);
        self.add_missing_files_rules();
    }

    /// One-time migration: each device with push whose record does not say its `files` rule
    /// was made gets one, unless a rule already sends category `files` to its target; then
    /// the record says so and it is never looked at again (blocking: the engine's database).
    fn add_missing_files_rules(&self) {
        let Some(e) = self.engine.get() else { return };
        let todo: Vec<(String, String, String)> = self
            .lock()
            .devices
            .iter()
            .filter(|d| d.push.is_some() && !d.files_rule_added)
            .map(|d| (d.id.clone(), d.name.clone(), d.user.clone()))
            .collect();
        if todo.is_empty() {
            return;
        }
        let (Ok(targets), Ok(rules)) = (e.targets(), e.rules()) else {
            tracing::warn!("devices: the notification targets could not be read: files rules checked at the next start");
            return;
        };
        let mut done = Vec::new();
        for (id, name, user) in todo {
            let Some(t) = targets
                .iter()
                .find(|t| matches!(&t.config, TargetConfig::Device { device_id: d } if *d == id))
            else {
                // no target (yet): the next registration makes it with both rules
                continue;
            };
            let has = rules
                .iter()
                .any(|r| r.targets.contains(&t.id) && r.categories.contains(&NotifyCategory::Files));
            if !has {
                if let Err(err) = e.create_rule(&files_rule(&id, &name, &user, t.id)) {
                    tracing::warn!("devices: the files rule of {id}: {err}");
                    continue;
                }
                audit(format_args!("files rule added to the notification target of {id}"));
            }
            done.push(id);
        }
        self.mark_files_rule(&done);
    }

    /// Records that the `files` rule of these devices' targets was made.
    fn mark_files_rule(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let mut reg = self.lock();
        let mut changed = false;
        for d in reg.devices.iter_mut().filter(|d| ids.contains(&d.id)) {
            changed |= !d.files_rule_added;
            d.files_rule_added = true;
        }
        if changed && let Err(e) = self.save(&reg) {
            tracing::error!("devices: {}", e.2);
        }
    }

    /// The `device` target of `device_id` (owned by `user`), unless it exists, with two
    /// rules: warnings and above, and the owner's own file transfers (category `files`, field
    /// `user`). A target made now speaks `lang` (English without one); an existing target
    /// takes `lang` when one is given and differs (blocking: the engine's database).
    fn ensure_target(&self, device_id: &str, name: &str, user: &str, lang: Option<NotifyLanguage>) {
        let Some(e) = self.engine.get() else { return };
        let targets = e.targets().unwrap_or_default();
        let mine = |c: &TargetConfig| matches!(c, TargetConfig::Device { device_id: d } if d == device_id);
        if let Some(t) = targets.iter().find(|t| mine(&t.config)) {
            if let Some(l) = lang.filter(|l| *l != t.language) {
                let req = TargetRequest {
                    name: t.name.clone(),
                    enabled: t.enabled,
                    language: l,
                    config: t.config.clone(),
                    secrets: Default::default(),
                };
                if let Err(err) = e.update_target(t.id, &req) {
                    tracing::warn!("devices: the language of the notification target of {device_id}: {err}");
                }
            }
            return;
        }
        let req = TargetRequest {
            name: target_name(device_id, name),
            enabled: true,
            language: lang.unwrap_or_default(),
            config: TargetConfig::Device {
                device_id: device_id.to_string(),
            },
            secrets: Default::default(),
        };
        match e.create_target(&req) {
            Ok(t) => {
                let rules = [
                    NotifyRule {
                        name: target_name(device_id, name),
                        enabled: true,
                        min_severity: NotifySeverity::Warning,
                        targets: vec![t.id],
                        ..Default::default()
                    },
                    files_rule(device_id, name, user, t.id),
                ];
                let mut files_made = false;
                for (i, rule) in rules.iter().enumerate() {
                    match e.create_rule(rule) {
                        Ok(_) => files_made |= i == 1,
                        Err(err) => tracing::warn!("devices: the rule {} of {device_id}: {err}", rule.name),
                    }
                }
                // a failed one is made by the migration at the next start
                if files_made {
                    self.mark_files_rule(&[device_id.to_string()]);
                }
            }
            Err(err) => tracing::warn!("devices: the notification target of {device_id}: {err}"),
        }
    }

    /// Removes the `device` target of `device_id`: out of every rule (a rule left without
    /// targets goes too), then the target itself.
    fn remove_target(&self, device_id: &str) {
        let Some(e) = self.engine.get() else { return };
        let ids: Vec<u64> = e
            .targets()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| matches!(&t.config, TargetConfig::Device { device_id: d } if d == device_id))
            .map(|t| t.id)
            .collect();
        for id in ids {
            for r in e.rules().unwrap_or_default().into_iter().filter(|r| r.targets.contains(&id)) {
                let mut left = r.clone();
                left.targets.retain(|t| *t != id);
                let res = if left.targets.is_empty() {
                    e.delete_rule(r.id)
                } else {
                    e.update_rule(r.id, &left).map(|_| ())
                };
                if let Err(err) = res {
                    tracing::warn!("devices: rule {} of {device_id}: {err}", r.id);
                }
            }
            if let Err(err) = e.delete_target(id) {
                tracing::warn!("devices: the notification target of {device_id}: {err}");
            }
        }
    }

    /// The device's new name on its notification target.
    fn rename_target(&self, device_id: &str, name: &str) {
        let Some(e) = self.engine.get() else { return };
        for t in e.targets().unwrap_or_default() {
            if matches!(&t.config, TargetConfig::Device { device_id: d } if d == device_id) {
                let req = TargetRequest {
                    name: target_name(device_id, name),
                    enabled: t.enabled,
                    language: t.language,
                    config: t.config.clone(),
                    secrets: Default::default(),
                };
                if let Err(err) = e.update_target(t.id, &req) {
                    tracing::warn!("devices: the notification target of {device_id}: {err}");
                }
            }
        }
    }

    /// `PUT …/self/push`: the device's subscription, and its notification target in `lang`
    /// (`en`, `it`, `ru`, anything else English; `None` keeps an existing target's language).
    pub fn set_push(&self, who: &str, id: &str, sub: PushSubscription, lang: Option<&str>) -> Res<Device> {
        if let Err(m) = parse_subscription(&sub) {
            let field = m.split(':').next().unwrap_or("endpoint");
            return Err(Error::invalid(field, m));
        }
        let sub = PushSubscription {
            endpoint: sub.endpoint.trim().to_string(),
            ..sub
        };
        let lang = lang.and_then(|l| NotifyLanguage::new(push_language(l)));
        let view = {
            let mut reg = self.lock();
            let Some(i) = reg.devices.iter().position(|d| d.id == id && d.user == who) else {
                return Err(reg.missing(id));
            };
            let r = &mut reg.devices[i];
            r.push = Some(sub);
            let view = Self::view(r);
            self.save(&reg)?;
            view
        };
        // the endpoint is a capability: never in the log
        audit(format_args!("{who} registered push for {id} ({})", view.name));
        self.ensure_target(id, &view.name, &view.user, lang);
        Ok(view)
    }

    /// `DELETE …/self/push`: no more push to this device.
    pub fn clear_push(&self, who: &str, id: &str) -> Res<()> {
        {
            let mut reg = self.lock();
            let Some(i) = reg.devices.iter().position(|d| d.id == id && d.user == who) else {
                return Err(reg.missing(id));
            };
            if reg.devices[i].push.take().is_some() {
                self.save(&reg)?;
            }
        }
        audit(format_args!("{who} removed push of {id}"));
        self.remove_target(id);
        Ok(())
    }

    /// The push service no longer knows the subscription (404/410).
    fn push_gone(&self, id: &str) {
        {
            let mut reg = self.lock();
            if let Some(r) = reg.devices.iter_mut().find(|d| d.id == id)
                && r.push.take().is_some()
                && let Err(e) = self.save(&reg)
            {
                tracing::error!("devices: {}", e.2);
            }
        }
        audit(format_args!("push subscription of {id} gone: dropped"));
        self.remove_target(id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.reg.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, reg: &Registry) -> Res<()> {
        let json = serde_json::to_vec_pretty(reg).map_err(Error::internal)?;
        write_private(&self.dir.join("devices.json"), &json).map_err(Error::internal)
    }

    /// This NAS' identity for the app (read once at start). A file that could not be read
    /// answers 503 everywhere it is needed: a new id would unpair every phone.
    pub fn sid(&self) -> Res<String> {
        self.sid.clone().map_err(|e| {
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                error_codes::BACKEND_UNAVAILABLE,
                format!("the NAS id is unreadable: {e}"),
            )
        })
    }

    fn view(r: &Record) -> Device {
        Device {
            id: r.id.clone(),
            name: r.name.clone(),
            user: r.user.clone(),
            model: r.model.clone(),
            os: r.os.clone(),
            app_version: r.app_version.clone(),
            created_ms: r.created_ms,
            last_seen_ms: r.last_seen_ms,
            last_ip: r.last_ip.clone(),
            key_hash: b64u_decode(&r.public_key)
                .map(|k| key_hash(&k))
                .unwrap_or_default(),
            push: r.push.is_some(),
            current: false,
        }
    }

    /// The live device ids.
    /// The live devices with their user's groups now (blocking: NSS), for rynas-web's sweep.
    pub fn live(&self) -> Vec<Live> {
        let devices: Vec<(String, String)> = self
            .lock()
            .devices
            .iter()
            .map(|d| (d.id.clone(), d.user.clone()))
            .collect();
        let mut users: std::collections::HashMap<String, Option<Vec<String>>> = Default::default();
        devices
            .into_iter()
            .map(|(id, user)| {
                let groups = users
                    .entry(user.clone())
                    .or_insert_with(|| match (self.lookup)(&user) {
                        Ok(Some(g)) => Some(g),
                        Ok(None) => Some(Vec::new()),
                        Err(_) => None,
                    })
                    .clone();
                Live { id, groups }
            })
            .collect()
    }

    pub fn ids(&self) -> Vec<String> {
        self.lock().devices.iter().map(|d| d.id.clone()).collect()
    }

    /// The devices `who` may see: their own, or everyone's for an administrator.
    pub fn list(&self, who: &str) -> Vec<Device> {
        let admin = self.admins.is_admin(who);
        self.lock()
            .devices
            .iter()
            .filter(|d| admin || d.user == who)
            .map(Self::view)
            .collect()
    }

    /// An approved pairing becomes a device of `user`.
    pub fn register(&self, user: &str, r: &Register) -> Res<Device> {
        let key = b64u_decode(&r.public_key).filter(|k| p256_point(k).is_some());
        if key.is_none() {
            return Err(Error::invalid("public_key", "not a P-256 public key"));
        }
        if !valid_sha(&r.refresh_sha) {
            return Err(Error::invalid("refresh_sha", "not a SHA-256 in base64url"));
        }
        let name = clean_name(&r.name).ok_or_else(|| Error::invalid("name", "empty name"))?;
        let now = self.clock.now_ms();
        // a failed lookup leaves it for the first refresh
        let uid = (self.uid_of)(user).ok().flatten();
        let rec = Record {
            id: new_id("d_"),
            user: user.to_string(),
            name,
            model: clean_text(&r.device.model),
            os: clean_text(&r.device.os),
            app_version: clean_text(&r.device.app_version),
            public_key: r.public_key.clone(),
            created_ms: now,
            last_seen_ms: now,
            last_ip: clean_text(&r.ip),
            last_refresh_ms: now,
            refresh_sha: r.refresh_sha.clone(),
            prev: Vec::new(),
            last_ts: 0,
            push: None,
            uid,
            files_rule_added: false,
            last_replaced: String::new(),
        };
        let view = Self::view(&rec);
        {
            let mut reg = self.lock();
            if reg.devices.len() >= MAX_DEVICES {
                return Err(Error::new(
                    StatusCode::CONFLICT,
                    error_codes::CONFLICT,
                    format!("at most {MAX_DEVICES} devices"),
                ));
            }
            reg.devices.push(rec);
            if let Err(e) = self.save(&reg) {
                reg.devices.pop();
                return Err(e);
            }
        }
        audit(format_args!("{} paired {} ({}) from {}", user, view.id, view.name, view.last_ip));
        self.notifier
            .emit(device_paired(user, &view.id, &view.name, &view.last_ip));
        Ok(view)
    }

    fn may_manage(&self, who: &str, r: &Record) -> bool {
        r.user == who || self.admins.is_admin(who)
    }

    pub fn rename(&self, who: &str, id: &str, raw: &str) -> Res<Device> {
        let name = clean_name(raw).ok_or_else(|| Error::invalid("name", "empty name"))?;
        let (view, old) = {
            let mut reg = self.lock();
            let r = reg
                .devices
                .iter_mut()
                .find(|d| d.id == id)
                .filter(|d| d.user == who || self.admins.is_admin(who))
                .ok_or_else(not_found)?;
            let old = std::mem::replace(&mut r.name, name);
            let view = Self::view(r);
            self.save(&reg)?;
            (view, old)
        };
        if old != view.name {
            audit(format_args!("{who} renamed {id} from {old} to {}", view.name));
            self.notifier
                .emit(device_renamed(&view.user, id, &old, &view.name));
            if view.push {
                self.rename_target(id, &view.name);
            }
        }
        Ok(view)
    }

    /// Revokes the device `id`: it is gone from the store (its refresh token with it); its
    /// tombstone keeps `reason` (one of [`revoke_reason`]).
    pub fn revoke(&self, who: &str, id: &str, reason: &str) -> Res<()> {
        let now = self.clock.now_ms();
        let rec = {
            let mut reg = self.lock();
            let i = reg
                .devices
                .iter()
                .position(|d| d.id == id && self.may_manage(who, d))
                .ok_or_else(not_found)?;
            let rec = reg.devices.remove(i);
            reg.bury(id, reason, now);
            if let Err(e) = self.save(&reg) {
                reg.unbury(id);
                reg.devices.insert(i, rec);
                return Err(e);
            }
            rec
        };
        audit(format_args!("{who} revoked {} ({}) of {}: {reason}", rec.id, rec.name, rec.user));
        self.notifier
            .emit(device_revoked(&rec.user, &rec.id, &rec.name, reason));
        if rec.push.is_some() {
            self.remove_target(&rec.id);
        }
        Ok(())
    }

    /// Removes the devices `drop` picks, with one notification each.
    fn revoke_where(&self, reason: &str, pick: impl Fn(&Record) -> bool) -> Vec<String> {
        let now = self.clock.now_ms();
        let gone: Vec<Record> = {
            let mut reg = self.lock();
            let (gone, keep): (Vec<Record>, Vec<Record>) =
                std::mem::take(&mut reg.devices).into_iter().partition(|d| pick(d));
            reg.devices = keep;
            for r in &gone {
                reg.bury(&r.id, reason, now);
            }
            if !gone.is_empty()
                && let Err(e) = self.save(&reg)
            {
                tracing::error!("devices: {}", e.2);
            }
            gone
        };
        for r in &gone {
            audit(format_args!("{} ({}) of {} revoked: {reason}", r.id, r.name, r.user));
            self.notifier
                .emit(device_revoked(&r.user, &r.id, &r.name, reason));
            if r.push.is_some() {
                self.remove_target(&r.id);
            }
        }
        gone.into_iter().map(|r| r.id).collect()
    }

    /// The refresh of a device: signature, clock, account, then the token with rotation, the
    /// grace window and reuse detection. Only an unknown device and a bad signature are
    /// marked as guesses ([`GUESS_DEVICE_ID`], [`GUESS_SIGNATURE`]).
    pub fn refresh(&self, c: &RefreshCheck) -> Res<RefreshOk> {
        let now = self.clock.now_ms();
        let sid = self.sid()?;
        if !valid_sha(&c.refresh_sha) || !valid_sha(&c.new_refresh_sha) {
            return Err(refresh_invalid());
        }
        let (user, key, uid, name) = {
            let reg = self.lock();
            let Some(r) = reg.devices.iter().find(|d| d.id == c.device_id) else {
                // revoked: the app hears why (not a guess); never known: a guess, no reason
                return Err(match reg.tombstone(&c.device_id) {
                    Some(t) => revoked().reason(&t.reason),
                    None => revoked().field(GUESS_DEVICE_ID),
                });
            };
            (r.user.clone(), b64u_decode(&r.public_key).unwrap_or_default(), r.uid, r.name.clone())
        };
        let msg = refresh_message(&sid, &c.device_id, c.ts, &c.refresh_sha);
        if !verify(&key, &msg, &c.sig) {
            return Err(refresh_invalid().field(GUESS_SIGNATURE));
        }
        // the device's own key signed a time far from ours: its clock (or ours) is wrong
        if !ts_ok(c.ts, now) {
            return Err(self.clock_refused(&user, &name));
        }
        self.clock_accepted();
        match (self.account)(&user) {
            Account::Ok => {}
            a @ (Account::Gone | Account::Disabled) => {
                let reason = account_reason(a);
                self.revoke_where(reason, |d| d.user == user);
                return Err(revoked().reason(reason));
            }
            Account::Unknown => return Err(unavailable("the account check failed")),
        }
        let groups = match (self.lookup)(&user) {
            Ok(Some(g)) => g,
            Ok(None) => {
                self.revoke_where(revoke_reason::ACCOUNT_REMOVED, |d| d.user == user);
                return Err(revoked().reason(revoke_reason::ACCOUNT_REMOVED));
            }
            Err(e) => {
                tracing::warn!("devices: refresh of {}: {e}", c.device_id);
                return Err(unavailable("the account check failed"));
            }
        };
        let now_uid = match (self.uid_of)(&user) {
            Ok(Some(u)) => u,
            Ok(None) => {
                self.revoke_where(revoke_reason::ACCOUNT_REMOVED, |d| d.user == user);
                return Err(revoked().reason(revoke_reason::ACCOUNT_REMOVED));
            }
            Err(e) => {
                tracing::warn!("devices: refresh of {}: {e}", c.device_id);
                return Err(unavailable("the account check failed"));
            }
        };
        // the name now belongs to another account (removed and made again)
        if uid.is_some_and(|u| u != now_uid) {
            self.revoke_where(revoke_reason::ACCOUNT_REPLACED, |d| {
                d.user == user && d.uid.is_some_and(|u| u != now_uid)
            });
            return Err(revoked().reason(revoke_reason::ACCOUNT_REPLACED));
        }
        // the account may sign in to the web interface (today: administrators), by rynasd's
        // groups and by rynas-web's: a token is never rotated for an answer rynas-web drops
        let web_ok = c.admin_groups.is_empty() || groups.iter().any(|g| c.admin_groups.contains(g));
        if !self.admins.is_admin(&user) || !web_ok {
            return Err(refresh_invalid());
        }
        let mut reg = self.lock();
        let Some(i) = reg.devices.iter().position(|d| d.id == c.device_id) else {
            // revoked meanwhile
            let reason = reg.tombstone(&c.device_id).map(|t| t.reason.clone());
            return Err(match reason {
                Some(r) => revoked().reason(r),
                None => revoked(),
            });
        };
        let r = &mut reg.devices[i];
        // an old or replayed request: refused, the device stays
        if c.ts <= r.last_ts {
            return Err(refresh_invalid());
        }
        r.prev.retain(|p| now < p.until_ms);
        let current = ct_eq_bytes(r.refresh_sha.as_bytes(), c.refresh_sha.as_bytes());
        let previous = !current
            && r
                .prev
                .iter()
                .any(|p| ct_eq_bytes(p.sha.as_bytes(), c.refresh_sha.as_bytes()));
        // the token replaced most recently, after its window: an app that lost the answer and
        // came back late, not a copy; refused, the device stays (the app pairs again)
        if !current
            && !previous
            && !r.last_replaced.is_empty()
            && ct_eq_bytes(r.last_replaced.as_bytes(), c.refresh_sha.as_bytes())
        {
            return Err(refresh_invalid());
        }
        if !current && !previous {
            let rec = reg.devices.remove(i);
            reg.bury(&rec.id, revoke_reason::REFRESH_REUSE, now);
            if let Err(e) = self.save(&reg) {
                tracing::error!("devices: {}", e.2);
            }
            drop(reg);
            tracing::warn!(
                target: "rynas::audit",
                "devices: refresh token reuse for {} ({}) of {} from {}: revoked",
                rec.id,
                rec.name,
                rec.user,
                clean_text(&c.ip)
            );
            self.notifier
                .emit(device_refresh_reuse(&rec.user, &rec.id, &rec.name, &clean_text(&c.ip)));
            if rec.push.is_some() {
                self.remove_target(&rec.id);
            }
            return Err(revoked().reason(revoke_reason::REFRESH_REUSE));
        }
        // the replaced token stays good for the grace window (a lost answer, a second refresh
        // sent at the same time); every token of the window keeps its own end
        let replaced = std::mem::replace(&mut r.refresh_sha, c.new_refresh_sha.clone());
        r.last_replaced = replaced.clone();
        r.prev.push(PrevToken {
            sha: replaced,
            until_ms: now + REFRESH_GRACE_S * 1000,
        });
        if r.prev.len() > 8 {
            r.prev.remove(0);
        }
        r.last_ts = c.ts;
        r.last_refresh_ms = now;
        r.last_seen_ms = now;
        r.last_ip = clean_text(&c.ip);
        // records from before the uid was kept get it now
        if r.uid.is_none() {
            r.uid = Some(now_uid);
        }
        let view = Self::view(r);
        self.save(&reg)?;
        Ok(RefreshOk {
            device: view,
            groups,
            grace: previous,
        })
    }

    /// Replaces the probe of the NAS clock's synchronization (tests).
    pub fn set_sync_probe(&self, probe: SyncProbe) {
        *self.sync_probe.lock().unwrap_or_else(|e| e.into_inner()) = probe;
    }

    /// The `clock_skew` answer, with the side that is likely wrong as `reason`: the NAS when
    /// its clock is not synchronized (then an alarm opens, once for all the devices), else the
    /// device.
    fn clock_refused(&self, user: &str, device: &str) -> Error {
        let probe = self.sync_probe.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if probe() {
            return clock_skew().reason(clock_skew_reason::DEVICE);
        }
        if !self.skew_open.swap(true, std::sync::atomic::Ordering::SeqCst) {
            tracing::warn!("devices: {device} of {user} refused: the NAS clock is not synchronized");
            self.notifier.emit(device_clock_skew(user, device));
        }
        clock_skew().reason(clock_skew_reason::NAS)
    }

    /// A device's time accepted: an open [`device_clock_skew`] alarm closes.
    fn clock_accepted(&self) {
        if self.skew_open.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.notifier.emit(device_clock_ok());
        }
    }

    /// The device `id` as its own app sees it, with the TLS pins and base URLs.
    pub async fn self_view(&self, who: &str, id: &str) -> Res<DeviceSelf> {
        let mut device = {
            let reg = self.lock();
            let r = reg
                .devices
                .iter()
                .find(|d| d.id == id && d.user == who)
                .ok_or_else(|| reg.missing(id))?;
            Self::view(r)
        };
        device.current = true;
        let (endpoints, tls_pins) = (self.trust)().await;
        Ok(DeviceSelf {
            device,
            tls_pins,
            endpoints,
        })
    }

    pub async fn trust(&self) -> Res<Trust> {
        let sid = self.sid()?;
        let (endpoints, tls_pins) = (self.trust)().await;
        Ok(Trust {
            sid,
            endpoints,
            tls_pins,
        })
    }

    /// Expires devices without a refresh for [`INACTIVE_EXPIRY_DAYS`], revokes devices never
    /// refreshed [`UNCLAIMED_AFTER_MS`] after their approval, and the devices of removed or
    /// disabled accounts or of a name that now belongs to another uid (blocking: PAM, NSS).
    /// Drops tombstones older than [`REVOKED_KEPT_DAYS`]. Returns the revoked ids.
    pub fn check(&self) -> Vec<String> {
        let now = self.clock.now_ms();
        {
            let mut reg = self.lock();
            let before = reg.revoked.len();
            reg.revoked
                .retain(|t| now.saturating_sub(t.revoked_ms) < REVOKED_KEPT_DAYS * DAY_MS);
            if reg.revoked.len() != before
                && let Err(e) = self.save(&reg)
            {
                tracing::error!("devices: {}", e.2);
            }
        }
        let limit = INACTIVE_EXPIRY_DAYS * DAY_MS;
        let mut out = self.revoke_where(revoke_reason::INACTIVE, |d| {
            now.saturating_sub(d.last_refresh_ms) >= limit
        });
        out.extend(self.revoke_where(revoke_reason::UNCLAIMED, |d| {
            d.last_ts == 0 && now.saturating_sub(d.created_ms) >= UNCLAIMED_AFTER_MS
        }));
        let users: BTreeSet<String> = self.lock().devices.iter().map(|d| d.user.clone()).collect();
        for u in users {
            match (self.account)(&u) {
                Account::Ok => match (self.uid_of)(&u) {
                    Ok(Some(uid)) => {
                        out.extend(self.revoke_where(revoke_reason::ACCOUNT_REPLACED, |d| {
                            d.user == u && d.uid.is_some_and(|x| x != uid)
                        }));
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("devices: the uid of {u}: {e}: checked again later"),
                },
                a @ (Account::Gone | Account::Disabled) => {
                    out.extend(self.revoke_where(account_reason(a), |d| d.user == u));
                }
                Account::Unknown => {
                    tracing::warn!("devices: the account check of {u} failed: its devices stay, checked again later");
                }
            }
        }
        out
    }
}

/// The running device service, for the users' password change ([`password_changed`]).
static RUNNING: std::sync::OnceLock<std::sync::Weak<DevicesSvc>> = std::sync::OnceLock::new();

/// Makes `svc` the one [`password_changed`] reports to (rynasd's start).
pub fn set_running(svc: &Arc<DevicesSvc>) {
    let _ = RUNNING.set(Arc::downgrade(svc));
}

/// A user's password was changed: paired devices keep their access (they hold their own
/// key), and a security notification lists them, so a device paired by someone else shows.
pub fn password_changed(user: &str) {
    if let Some(svc) = RUNNING.get().and_then(std::sync::Weak::upgrade) {
        svc.note_password_changed(user);
    }
}

impl DevicesSvc {
    fn note_password_changed(&self, user: &str) {
        let names: Vec<String> = self
            .lock()
            .devices
            .iter()
            .filter(|d| d.user == user)
            .map(|d| d.name.clone())
            .collect();
        if !names.is_empty() {
            audit(format_args!("password of {user} changed: {} paired devices keep access", names.len()));
            self.notifier
                .emit(rynas_notify::checks::devices_kept_after_password(user, &names));
        }
    }
}

/// The periodic check of [`DevicesSvc::check`].
pub fn spawn_watch(svc: Arc<DevicesSvc>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(CHECK_EVERY);
        loop {
            tick.tick().await;
            let s = svc.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || s.check()).await {
                tracing::error!("devices: {e}");
            }
        }
    });
}

fn svc(st: &DevState) -> Res<Arc<DevicesSvc>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "mobile devices unavailable".into(),
        )
    })
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Res<T> + Send + 'static) -> Res<T> {
    tokio::task::spawn_blocking(f).await.map_err(Error::internal)?
}

pub fn router(st: DevState) -> Router {
    Router::new()
        .route("/v1/devices/sid", get(sid_get))
        .route("/v1/devices/trust", get(trust_get))
        .route("/v1/devices/live", get(live_get))
        .route("/v1/devices/register", post(register_post))
        .route("/v1/devices/refresh", post(refresh_post))
        .route("/v1/devices", get(list_get))
        .route("/v1/devices/{id}", patch(rename_patch).delete(revoke_delete))
        .route("/v1/devices/{id}/self", get(self_get))
        .route("/v1/devices/{id}/push", put(push_put).delete(push_delete))
        .with_state(st)
}

async fn push_put(
    State(st): State<DevState>,
    h: HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: Result<Json<PushRegistration>, JsonRejection>,
) -> Res<Json<Device>> {
    let s = svc(&st)?;
    let who = user(&h)?;
    let Json(reg) = body?;
    Ok(Json(blocking(move || s.set_push(&who, &id, reg.subscription, reg.lang.as_deref())).await?))
}

async fn push_delete(
    State(st): State<DevState>,
    h: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Res<StatusCode> {
    let s = svc(&st)?;
    let who = user(&h)?;
    blocking(move || s.clear_push(&who, &id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn sid_get(State(st): State<DevState>) -> Res<Json<Sid>> {
    let s = svc(&st)?;
    Ok(Json(Sid {
        sid: s.sid()?,
    }))
}

async fn trust_get(State(st): State<DevState>) -> Res<Json<Trust>> {
    Ok(Json(svc(&st)?.trust().await?))
}

async fn live_get(State(st): State<DevState>) -> Res<Json<Vec<Live>>> {
    let s = svc(&st)?;
    Ok(Json(blocking(move || Ok(s.live())).await?))
}

async fn register_post(
    State(st): State<DevState>,
    h: HeaderMap,
    body: Result<Json<Register>, JsonRejection>,
) -> Res<Json<Device>> {
    let s = svc(&st)?;
    let who = user(&h)?;
    let Json(r) = body?;
    Ok(Json(blocking(move || s.register(&who, &r)).await?))
}

async fn refresh_post(
    State(st): State<DevState>,
    body: Result<Json<RefreshCheck>, JsonRejection>,
) -> Res<Json<RefreshOk>> {
    let s = svc(&st)?;
    let Json(c) = body?;
    Ok(Json(blocking(move || s.refresh(&c)).await?))
}

async fn list_get(State(st): State<DevState>, h: HeaderMap) -> Res<Json<Vec<Device>>> {
    let s = svc(&st)?;
    let who = user(&h)?;
    Ok(Json(blocking(move || Ok(s.list(&who))).await?))
}

async fn rename_patch(
    State(st): State<DevState>,
    h: HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: Result<Json<DeviceRename>, JsonRejection>,
) -> Res<Json<Device>> {
    let s = svc(&st)?;
    let who = user(&h)?;
    let Json(r) = body?;
    Ok(Json(blocking(move || s.rename(&who, &id, &r.name)).await?))
}

#[derive(Deserialize, Default)]
struct RevokeQuery {
    #[serde(default)]
    reason: Option<String>,
}

async fn revoke_delete(
    State(st): State<DevState>,
    h: HeaderMap,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<RevokeQuery>,
) -> Res<StatusCode> {
    let s = svc(&st)?;
    let who = user(&h)?;
    let reason = match q.reason.as_deref() {
        None => revoke_reason::USER,
        Some(r) => [revoke_reason::USER, revoke_reason::SELF, revoke_reason::UNCLAIMED]
            .into_iter()
            .find(|x| *x == r)
            .ok_or_else(|| Error::bad_request("reason: user, self or unclaimed"))?,
    };
    blocking(move || s.revoke(&who, &id, reason)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn self_get(
    State(st): State<DevState>,
    h: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Res<Json<DeviceSelf>> {
    let s = svc(&st)?;
    let who = user(&h)?;
    Ok(Json(s.self_view(&who, &id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::devices::DeviceInfo;
    use rynas_auth::ManualClock;
    use rynas_auth::device::{sha_b64u, spki_of_point};
    use rynas_notify::MemoryNotifier;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};

    const T0: u64 = 1_800_000_000_000;

    struct Key(EcdsaKeyPair, SystemRandom);

    impl Key {
        fn new() -> Self {
            let rng = SystemRandom::new();
            let p = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
            Key(
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, p.as_ref(), &rng).unwrap(),
                rng,
            )
        }
        fn spki(&self) -> String {
            b64u(&spki_of_point(self.0.public_key().as_ref()))
        }
        fn sign(&self, m: &str) -> String {
            b64u(self.0.sign(&self.1, m.as_bytes()).unwrap().as_ref())
        }
    }

    fn groups(u: &str) -> Option<Vec<String>> {
        match u {
            "alice" | "admin" => Some(vec!["rynas-admin".into()]),
            "bob" => Some(vec!["users".into()]),
            _ => None,
        }
    }

    thread_local! {
        /// The group lookup fails (NSS down) on this test's thread.
        static LOOKUP_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn lookup(u: &str) -> Result<Option<Vec<String>>, String> {
        if LOOKUP_FAILS.with(|f| f.get()) {
            return Err("nss: connection refused".into());
        }
        Ok(groups(u))
    }

    thread_local! {
        /// alice was removed and made again with another uid, on this test's thread.
        static ALICE_REPLACED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn uid(u: &str) -> Result<Option<u32>, String> {
        if LOOKUP_FAILS.with(|f| f.get()) {
            return Err("nss: connection refused".into());
        }
        Ok(groups(u).map(|_| match u {
            "alice" if ALICE_REPLACED.with(|f| f.get()) => 1500,
            "alice" => 1000,
            "bob" => 1001,
            _ => 1002,
        }))
    }

    struct Rig {
        svc: Arc<DevicesSvc>,
        clock: Arc<ManualClock>,
        events: Arc<MemoryNotifier>,
        gone: Arc<Mutex<Vec<String>>>,
        /// Users whose account check fails (PAM or NSS error).
        failing: Arc<Mutex<Vec<String>>>,
        _dir: tempfile::TempDir,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::new(T0));
        let events = Arc::new(MemoryNotifier::default());
        let gone: Arc<Mutex<Vec<String>>> = Arc::default();
        let failing: Arc<Mutex<Vec<String>>> = Arc::default();
        let (g, f) = (gone.clone(), failing.clone());
        let account: AccountCheck = Arc::new(move |u: &str| {
            if f.lock().unwrap().iter().any(|x| x == u) {
                Account::Unknown
            } else if u == "locked" {
                Account::Disabled
            } else if g.lock().unwrap().iter().any(|x| x == u) || groups(u).is_none() {
                Account::Gone
            } else {
                Account::Ok
            }
        });
        let trust: TrustProvider = Arc::new(|| {
            Box::pin(async { (vec!["https://192.0.2.5:8443".to_string()], vec!["pin".to_string()]) })
        });
        let svc = DevicesSvc::new(
            dir.path(),
            events.clone(),
            AdminPolicy::new(vec!["rynas-admin".into()], groups),
            lookup,
            uid,
            account,
            trust,
            clock.clone(),
        )
        .unwrap();
        Rig { svc, clock, events, gone, failing, _dir: dir }
    }

    fn register(r: &Rig, user: &str, key: &Key, token: &str) -> Device {
        register_in(&r.svc, user, key, token)
    }

    fn register_in(svc: &DevicesSvc, user: &str, key: &Key, token: &str) -> Device {
        svc
            .register(
                user,
                &Register {
                    name: "  Phone ".into(),
                    device: DeviceInfo {
                        model: "SM-S931B".into(),
                        ..Default::default()
                    },
                    public_key: key.spki(),
                    ip: "192.0.2.9".into(),
                    refresh_sha: sha_b64u(token),
                },
            )
            .unwrap()
    }

    /// A refresh signed now (a second after the previous one: each must be newer).
    fn check(r: &Rig, key: &Key, id: &str, old: &str, new: &str) -> Res<RefreshOk> {
        r.clock.advance_ms(1000);
        r.svc.refresh(&signed(r, key, id, old, new))
    }

    fn signed(r: &Rig, key: &Key, id: &str, old: &str, new: &str) -> RefreshCheck {
        let sid = r.svc.sid().unwrap();
        let ts = (r.clock.now_ms() / 1000) as i64;
        let sha = sha_b64u(old);
        RefreshCheck {
            device_id: id.into(),
            ts,
            sig: key.sign(&refresh_message(&sid, id, ts, &sha)),
            refresh_sha: sha,
            new_refresh_sha: sha_b64u(new),
            ip: "192.0.2.10".into(),
            admin_groups: vec!["rynas-admin".into(), "sudo".into()],
        }
    }

    #[test]
    fn a_replayed_request_is_refused_without_revoking() {
        let r = rig();
        let key = Key::new();
        let d = register(&r, "alice", &key, "R1");
        r.clock.advance_ms(1000);
        let req = signed(&r, &key, &d.id, "R1", "R2");
        r.svc.refresh(&req).unwrap();
        // the same request again (captured), inside the grace window: refused, device kept
        let e = r.svc.refresh(&req).unwrap_err();
        assert_eq!(e.1, error_codes::REFRESH_INVALID);
        // an older signature of the old token, too
        let mut old = req.clone();
        old.new_refresh_sha = sha_b64u("X");
        assert_eq!(r.svc.refresh(&old).unwrap_err().1, error_codes::REFRESH_INVALID);
        assert_eq!(r.svc.ids(), vec![d.id.clone()]);
        // the app's next refresh, signed later, works
        check(&r, &key, &d.id, "R2", "R3").unwrap();
    }

    #[test]
    fn two_refreshes_at_once_keep_the_device() {
        let r = rig();
        let key = Key::new();
        let d = register(&r, "alice", &key, "R1");
        // two refreshes of R1 sent together (the second signed a second later): both pass
        check(&r, &key, &d.id, "R1", "A").unwrap();
        check(&r, &key, &d.id, "R1", "B").unwrap();
        // whichever answer the app kept works, the other one too while the window lasts
        check(&r, &key, &d.id, "A", "C").unwrap();
        check(&r, &key, &d.id, "C", "D").unwrap();
        assert_eq!(r.svc.ids(), vec![d.id.clone()]);
        // after the window an old token is a reuse
        r.clock.advance_ms(61_000);
        assert_eq!(check(&r, &key, &d.id, "B", "E").unwrap_err().1, error_codes::DEVICE_REVOKED);
    }

    #[test]
    fn failed_account_checks_revoke_nothing() {
        let r = rig();
        let key = Key::new();
        let d = register(&r, "alice", &key, "R1");
        r.failing.lock().unwrap().push("alice".into());
        let e = check(&r, &key, &d.id, "R1", "R2").unwrap_err();
        assert_eq!((e.0, e.1), (StatusCode::SERVICE_UNAVAILABLE, error_codes::BACKEND_UNAVAILABLE));
        assert!(r.svc.check().is_empty(), "the periodic check skips the user");
        assert_eq!(r.svc.ids(), vec![d.id.clone()]);
        r.failing.lock().unwrap().clear();
        // NSS down for the group lookup: the same, and the token is not rotated
        LOOKUP_FAILS.with(|f| f.set(true));
        let e = check(&r, &key, &d.id, "R1", "R2").unwrap_err();
        assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(r.svc.live().iter().all(|l| l.groups.is_none()));
        LOOKUP_FAILS.with(|f| f.set(false));
        check(&r, &key, &d.id, "R1", "R2").unwrap();
        // a user outside rynas-web's administrator groups: no rotation
        let mut req = signed(&r, &key, &d.id, "R2", "R3");
        req.ts += 1;
        req.sig = key.sign(&refresh_message(&r.svc.sid().unwrap(), &d.id, req.ts, &req.refresh_sha));
        req.admin_groups = vec!["wheel".into()];
        assert_eq!(r.svc.refresh(&req).unwrap_err().1, error_codes::REFRESH_INVALID);
        check(&r, &key, &d.id, "R2", "R3").unwrap();
    }

    #[test]
    fn an_unreadable_nas_id_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        // not readable as a file: the id is unknown, nothing is written
        std::fs::create_dir(dir.path().join("nas-sid")).unwrap();
        let svc = DevicesSvc::new(
            dir.path(),
            Arc::new(rynas_notify::NullNotifier),
            AdminPolicy::new(vec!["rynas-admin".into()], groups),
            lookup,
            uid,
            Arc::new(|_: &str| Account::Ok),
            Arc::new(|| Box::pin(async { (Vec::new(), Vec::new()) })),
            Arc::new(ManualClock::new(T0)),
        )
        .unwrap();
        let e = svc.sid().unwrap_err();
        assert_eq!((e.0, e.1), (StatusCode::SERVICE_UNAVAILABLE, error_codes::BACKEND_UNAVAILABLE));
        assert!(dir.path().join("nas-sid").is_dir());
        // garbage in the file: the same, the file is left as it is
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("nas-sid"), "not an id\n").unwrap();
        assert!(load_sid(&other.path().join("nas-sid")).is_err());
        assert_eq!(std::fs::read_to_string(other.path().join("nas-sid")).unwrap(), "not an id\n");
        // only a missing file makes a new id, and the same one is read back
        let p = other.path().join("new-sid");
        let a = load_sid(&p).unwrap();
        assert_eq!(load_sid(&p).unwrap(), a);
    }

    #[test]
    fn store_survives_and_holds_no_token() {
        let r = rig();
        let key = Key::new();
        let d = register(&r, "alice", &key, "R1");
        assert_eq!(d.name, "Phone");
        assert!(d.id.starts_with("d_"));
        let text = std::fs::read_to_string(r._dir.path().join("devices/devices.json")).unwrap();
        assert!(!text.contains("\"R1\""));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(r._dir.path().join("devices/devices.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let sid = r.svc.sid().unwrap();
        assert_eq!(sid.len(), 22);
        // a second service on the same folder reads both back
        let again = DevicesSvc::new(
            r._dir.path(),
            Arc::new(rynas_notify::NullNotifier),
            AdminPolicy::new(vec!["rynas-admin".into()], groups),
            lookup,
            uid,
            Arc::new(|_: &str| Account::Ok),
            r.svc.trust.clone(),
            r.clock.clone(),
        )
        .unwrap();
        assert_eq!(again.sid().unwrap(), sid);
        assert_eq!(again.list("alice"), vec![d]);
        let ev = r.events.take();
        assert_eq!(ev[0].code, "device.paired");
    }

    #[test]
    fn rotation_grace_and_reuse() {
        let r = rig();
        let key = Key::new();
        let d = register(&r, "alice", &key, "R1");
        assert!(!check(&r, &key, &d.id, "R1", "R2").unwrap().grace);
        // the answer was lost: R1 again within 60 s is accepted once more (a new pair)
        r.clock.advance_ms(10_000);
        let ok = check(&r, &key, &d.id, "R1", "R3").unwrap();
        assert!(ok.grace);
        assert_eq!(ok.groups, vec!["rynas-admin".to_string()]);
        // R3 is current, R2 (never delivered) is dead: using it is a reuse
        assert!(!check(&r, &key, &d.id, "R3", "R4").unwrap().grace);
        // bad signature: refused, nothing changes; marked as a guess
        let other = Key::new();
        let e = check(&r, &other, &d.id, "R4", "R5").unwrap_err();
        assert_eq!((e.1, e.3.as_deref()), (error_codes::REFRESH_INVALID, Some(GUESS_SIGNATURE)));
        // the grace window is over: R3, the token replaced most recently, is refused and
        // the device stays (an app that lost the answer and came back late)
        r.clock.advance_ms(61_000);
        r.events.take();
        let e = check(&r, &key, &d.id, "R3", "R9").unwrap_err();
        assert_eq!((e.0, e.1, e.3), (StatusCode::UNAUTHORIZED, error_codes::REFRESH_INVALID, None));
        assert_eq!(r.svc.ids(), vec![d.id.clone()]);
        assert!(r.events.take().is_empty());
        // an older one (R2, replaced before R3) is a reuse and the device goes
        let e = check(&r, &key, &d.id, "R2", "R9").unwrap_err();
        assert_eq!((e.0, e.1, e.3), (StatusCode::UNAUTHORIZED, error_codes::DEVICE_REVOKED, None));
        assert!(r.svc.ids().is_empty());
        assert_eq!(r.events.take()[0].code, "device.refresh_reuse");
        // the revoked device again: its reason, not a guess
        let e = check(&r, &key, &d.id, "R4", "R5").unwrap_err();
        assert_eq!((e.1, e.3, e.4.as_deref()), (error_codes::DEVICE_REVOKED, None, Some("refresh_reuse")));
        // a device never known: marked as a guess
        let e = check(&r, &key, "d_unknown", "R4", "R5").unwrap_err();
        assert_eq!((e.1, e.3.as_deref()), (error_codes::DEVICE_REVOKED, Some(GUESS_DEVICE_ID)));
    }

    #[test]
    fn a_late_retry_of_the_last_token_keeps_the_device() {
        let r = rig();
        let key = Key::new();
        let d = register(&r, "alice", &key, "R1");
        check(&r, &key, &d.id, "R1", "R2").unwrap();
        // the answer (R2) was lost and the app retries R1 two minutes later: refused, kept
        r.clock.advance_ms(120_000);
        assert_eq!(check(&r, &key, &d.id, "R1", "R3").unwrap_err().1, error_codes::REFRESH_INVALID);
        assert_eq!(r.svc.ids(), vec![d.id.clone()]);
        // the current token still works; after that rotation R1 is two tokens back: a reuse
        check(&r, &key, &d.id, "R2", "R3").unwrap();
        r.clock.advance_ms(61_000);
        assert_eq!(check(&r, &key, &d.id, "R1", "R4").unwrap_err().1, error_codes::DEVICE_REVOKED);
        assert!(r.svc.ids().is_empty());
    }

    #[test]
    fn old_timestamps_and_refused_accounts() {
        let r = rig();
        let key = Key::new();
        let d = register(&r, "alice", &key, "R1");
        let sid = r.svc.sid().unwrap();
        let ts = (T0 / 1000) as i64 - 500;
        let sha = sha_b64u("R1");
        let e = r
            .svc
            .refresh(&RefreshCheck {
                device_id: d.id.clone(),
                ts,
                sig: key.sign(&refresh_message(&sid, &d.id, ts, &sha)),
                refresh_sha: sha,
                new_refresh_sha: sha_b64u("R2"),
                ip: String::new(),
                admin_groups: Vec::new(),
            })
            .unwrap_err();
        // signed by the device, its clock 500 s behind: told so, not a guess, device kept
        assert_eq!((e.0, e.1, e.3), (StatusCode::BAD_REQUEST, error_codes::CLOCK_SKEW, None));
        assert_eq!(r.svc.ids(), vec![d.id.clone()]);
        // a user outside the administrators may not refresh (and keeps the device)
        let kb = Key::new();
        let db = register(&r, "bob", &kb, "B1");
        assert_eq!(check(&r, &kb, &db.id, "B1", "B2").unwrap_err().1, error_codes::REFRESH_INVALID);
        assert_eq!(r.svc.ids().len(), 2);
        // a removed account revokes every device of it
        register(&r, "alice", &Key::new(), "X");
        r.gone.lock().unwrap().push("alice".into());
        assert_eq!(check(&r, &key, &d.id, "R1", "R2").unwrap_err().1, error_codes::DEVICE_REVOKED);
        assert_eq!(r.svc.ids(), vec![db.id.clone()]);
    }

    #[test]
    fn clock_skew_names_the_clock_likely_wrong() {
        let r = rig();
        let key = Key::new();
        let d = register(&r, "alice", &key, "R1");
        let sid = r.svc.sid().unwrap();
        let sha = sha_b64u("R1");
        let ask = |ts: i64| {
            r.svc.refresh(&RefreshCheck {
                device_id: d.id.clone(),
                ts,
                sig: key.sign(&refresh_message(&sid, &d.id, ts, &sha)),
                refresh_sha: sha.clone(),
                new_refresh_sha: sha_b64u("R2"),
                ip: String::new(),
                admin_groups: Vec::new(),
            })
        };
        let skewed = (T0 / 1000) as i64 - 500;
        r.events.take();
        // the NAS clock synchronized: the device is off, nothing notified
        r.svc.set_sync_probe(Arc::new(|| true));
        let e = ask(skewed).unwrap_err();
        assert_eq!((e.1, e.4.as_deref()), (error_codes::CLOCK_SKEW, Some(clock_skew_reason::DEVICE)));
        assert!(r.events.take().is_empty());
        // not synchronized: the NAS is named and one alarm opens for repeated refusals
        r.svc.set_sync_probe(Arc::new(|| false));
        for _ in 0..3 {
            let e = ask(skewed).unwrap_err();
            assert_eq!(e.4.as_deref(), Some(clock_skew_reason::NAS));
        }
        let ev = r.events.take();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].code, "device.clock_skew");
        assert_eq!(ev[0].actions[0].url, "/system/time");
        // a time accepted again closes it
        ask((T0 / 1000) as i64 + 1).unwrap();
        let ev = r.events.take();
        assert_eq!(ev.iter().map(|e| e.code.as_str()).collect::<Vec<_>>(), vec!["device.clock_ok"]);
    }

    #[test]
    fn management_rights_and_expiry() {
        let r = rig();
        let ka = Key::new();
        let a = register(&r, "alice", &ka, "A1");
        let b = register(&r, "bob", &Key::new(), "B1");
        // own devices; an administrator sees everyone's
        assert_eq!(r.svc.list("bob"), vec![b.clone()]);
        assert_eq!(r.svc.list("alice").len(), 2);
        assert_eq!(r.svc.rename("bob", &a.id, "x").unwrap_err().0, StatusCode::NOT_FOUND);
        assert_eq!(r.svc.rename("alice", &b.id, " Bob's ").unwrap().name, "Bob's");
        assert_eq!(r.svc.rename("bob", &b.id, "  ").unwrap_err().1, error_codes::INVALID_VALUE);
        assert!(r.svc.revoke("bob", &a.id, "user").is_err());
        r.events.take();
        r.svc.revoke("bob", &b.id, "self").unwrap();
        let ev = r.events.take();
        assert_eq!(ev[0].code, "device.revoked");
        assert_eq!(ev[0].fields["reason"], "self");
        // 180 days without a refresh: expired; a refresh the day before keeps it
        r.clock.advance_ms(179 * DAY_MS);
        check(&r, &ka, &a.id, "A1", "A2").unwrap();
        r.clock.advance_ms(DAY_MS + 1);
        assert!(r.svc.check().is_empty());
        let ko = Key::new();
        let other = register(&r, "alice", &ko, "O1");
        check(&r, &ko, &other.id, "O1", "O2").unwrap();
        r.clock.advance_ms(179 * DAY_MS);
        assert_eq!(r.svc.check(), vec![a.id.clone()]);
        assert_eq!(r.svc.ids(), vec![other.id.clone()]);
        // the account goes: its devices too
        r.gone.lock().unwrap().push("alice".into());
        assert_eq!(r.svc.check(), vec![other.id]);
    }

    #[tokio::test]
    async fn self_view_has_pins_and_urls() {
        let r = rig();
        let d = register(&r, "alice", &Key::new(), "A1");
        let s = r.svc.self_view("alice", &d.id).await.unwrap();
        assert!(s.device.current);
        assert_eq!(s.tls_pins, vec!["pin".to_string()]);
        assert!(r.svc.self_view("bob", &d.id).await.is_err());
        let t = r.svc.trust().await.unwrap();
        assert_eq!(t.endpoints, vec!["https://192.0.2.5:8443".to_string()]);
    }

    fn sub() -> PushSubscription {
        PushSubscription {
            endpoint: " https://ntfy.example.org/upXYZ?up=1 ".into(),
            p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4".into(),
            auth: "BTBZMqHH6r4Tts7J_aSIgg".into(),
        }
    }

    #[tokio::test]
    async fn push_subscriptions_make_and_remove_targets() {
        let r = rig();
        let engine = rynas_notify::engine::Engine::open(rynas_notify::engine::EngineOptions::new(
            r._dir.path().join("notify"),
            "nas",
        ))
        .unwrap();
        r.svc.bind_engine(engine.clone());
        let d = register(&r, "alice", &Key::new(), "A1");
        let device_targets = || {
            engine
                .targets()
                .unwrap()
                .into_iter()
                .filter(|t| matches!(t.config, TargetConfig::Device { .. }))
                .collect::<Vec<_>>()
        };
        // checked: https, a point on the curve, 16 bytes of auth; only the owner
        let mut bad = sub();
        bad.endpoint = "http://ntfy.example.org/up".into();
        let e = r.svc.set_push("alice", &d.id, bad, None).unwrap_err();
        assert_eq!((e.1, e.3.as_deref()), (error_codes::INVALID_VALUE, Some("endpoint")));
        let mut bad = sub();
        bad.auth = "AAAA".into();
        assert_eq!(r.svc.set_push("alice", &d.id, bad, None).unwrap_err().3.as_deref(), Some("auth"));
        assert_eq!(r.svc.set_push("bob", &d.id, sub(), None).unwrap_err().0, StatusCode::NOT_FOUND);
        assert!(r.svc.set_push("alice", &d.id, sub(), None).unwrap().push);
        let t = device_targets();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].name, format!("Phone · {}", &d.id[d.id.len() - 6..]));
        assert_eq!(t[0].language, NotifyLanguage::EN);
        // two rules: warnings and above, and the owner's own file transfers
        let rules = engine.rules().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!((rules[0].min_severity, rules[0].targets.clone()), (NotifySeverity::Warning, vec![t[0].id]));
        assert_eq!(rules[1].categories, vec![NotifyCategory::Files]);
        assert_eq!((rules[1].fields[0].field.as_str(), rules[1].fields[0].value.as_str()), ("user", "alice"));
        assert_eq!((rules[1].min_severity, rules[1].targets.clone()), (NotifySeverity::Info, vec![t[0].id]));
        // the engine finds the subscription and the owner by the device id
        let dir = PushDir(Arc::downgrade(&r.svc));
        assert_eq!(dir.subscription(&d.id).unwrap().endpoint, "https://ntfy.example.org/upXYZ?up=1");
        assert_eq!(dir.owner(&d.id).as_deref(), Some("alice"));
        assert_eq!(dir.owner("d_00"), None);
        // a second registration keeps one target; a rename follows
        r.svc.set_push("alice", &d.id, sub(), None).unwrap();
        r.svc.rename("alice", &d.id, "Pixel").unwrap();
        let t = device_targets();
        assert_eq!(t.len(), 1);
        assert!(t[0].name.starts_with("Pixel · "));
        assert_eq!(engine.rules().unwrap().len(), 2);
        // unregistered: target and rules gone
        r.svc.clear_push("alice", &d.id).unwrap();
        assert!(device_targets().is_empty() && engine.rules().unwrap().is_empty());
        assert!(!r.svc.list("alice")[0].push);
        // gone at the push service, or the device revoked: the same
        r.svc.set_push("alice", &d.id, sub(), None).unwrap();
        r.svc.push_gone(&d.id);
        assert!(device_targets().is_empty() && dir.subscription(&d.id).is_none());
        r.svc.set_push("alice", &d.id, sub(), None).unwrap();
        r.svc.revoke("alice", &d.id, "user").unwrap();
        assert!(device_targets().is_empty() && engine.rules().unwrap().is_empty());
    }

    #[tokio::test]
    async fn push_registrations_set_the_target_language() {
        let r = rig();
        let engine = rynas_notify::engine::Engine::open(rynas_notify::engine::EngineOptions::new(
            r._dir.path().join("notify"),
            "nas",
        ))
        .unwrap();
        r.svc.bind_engine(engine.clone());
        let lang = || {
            engine
                .targets()
                .unwrap()
                .into_iter()
                .find(|t| matches!(t.config, TargetConfig::Device { .. }))
                .map(|t| t.language.as_str().to_string())
        };
        // created in the language asked for
        let d = register(&r, "alice", &Key::new(), "A1");
        r.svc.set_push("alice", &d.id, sub(), Some("it")).unwrap();
        assert_eq!(lang().as_deref(), Some("it"));
        // without lang: kept; another lang: changed; an unknown one: English
        r.svc.set_push("alice", &d.id, sub(), None).unwrap();
        assert_eq!(lang().as_deref(), Some("it"));
        r.svc.set_push("alice", &d.id, sub(), Some("RU")).unwrap();
        assert_eq!(lang().as_deref(), Some("ru"));
        r.svc.set_push("alice", &d.id, sub(), Some("de")).unwrap();
        assert_eq!(lang().as_deref(), Some("en"));
        let devices = engine.targets().unwrap().into_iter().filter(|t| matches!(t.config, TargetConfig::Device { .. })).count();
        assert_eq!(devices, 1, "still one target");
        // the request body: the subscription's fields and an optional lang
        let reg: PushRegistration = serde_json::from_value(serde_json::json!({
            "endpoint": "https://ntfy.example.org/up", "p256dh": "x", "auth": "y", "lang": "it"
        }))
        .unwrap();
        assert_eq!((reg.subscription.endpoint.as_str(), reg.lang.as_deref()), ("https://ntfy.example.org/up", Some("it")));
        let reg: PushRegistration =
            serde_json::from_value(serde_json::json!({"endpoint": "e", "p256dh": "x", "auth": "y"})).unwrap();
        assert_eq!(reg.lang, None);
    }

    #[test]
    fn lan_urls_skip_tailscale_and_loopback() {
        let a: Vec<std::net::IpAddr> = ["127.0.0.1", "192.168.1.10", "100.101.2.3", "169.254.1.1", "2001:db8::1", "192.168.1.10"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(lan_urls(&a, 8443), vec!["https://192.168.1.10:8443".to_string()]);
    }

    #[test]
    fn revoked_devices_hear_why_for_30_days() {
        let r = rig();
        let reason = |e: Error| {
            assert_eq!((e.0, e.1), (StatusCode::UNAUTHORIZED, error_codes::DEVICE_REVOKED));
            assert_eq!(e.3, None, "not a guess");
            e.4
        };
        // revoked in the web interface (or by the app)
        let ku = Key::new();
        let du = register(&r, "alice", &ku, "U1");
        r.svc.revoke("alice", &du.id, revoke_reason::USER).unwrap();
        assert_eq!(reason(check(&r, &ku, &du.id, "U1", "U2").unwrap_err()).as_deref(), Some("user"));
        // a reuse: that answer, and the same on every later try
        let k = Key::new();
        let d = register(&r, "alice", &k, "R1");
        check(&r, &k, &d.id, "R1", "R2").unwrap();
        check(&r, &k, &d.id, "R2", "R3").unwrap();
        r.clock.advance_ms(61_000);
        assert_eq!(reason(check(&r, &k, &d.id, "R1", "R9").unwrap_err()).as_deref(), Some("refresh_reuse"));
        assert_eq!(reason(check(&r, &k, &d.id, "R3", "R4").unwrap_err()).as_deref(), Some("refresh_reuse"));
        // removed and disabled accounts
        let kc = Key::new();
        let dc = register(&r, "carol", &kc, "C1");
        assert_eq!(reason(check(&r, &kc, &dc.id, "C1", "C2").unwrap_err()).as_deref(), Some("account_removed"));
        assert_eq!(reason(check(&r, &kc, &dc.id, "C1", "C2").unwrap_err()).as_deref(), Some("account_removed"));
        let kl = Key::new();
        let dl = register(&r, "locked", &kl, "L1");
        assert_eq!(reason(check(&r, &kl, &dl.id, "L1", "L2").unwrap_err()).as_deref(), Some("account_disabled"));
        // a device's own routes answer the same (its access token outlived the revocation)
        let e = r.svc.clear_push("alice", &d.id).unwrap_err();
        assert_eq!((e.1, e.4.as_deref()), (error_codes::DEVICE_REVOKED, Some("refresh_reuse")));
        let e = r.svc.set_push("alice", &du.id, sub(), None).unwrap_err();
        assert_eq!((e.1, e.4.as_deref()), (error_codes::DEVICE_REVOKED, Some("user")));
        // the tombstones hold no key or token hash
        let text = std::fs::read_to_string(r._dir.path().join("devices/devices.json")).unwrap();
        assert!(!text.contains(&k.spki()) && !text.contains(&sha_b64u("R3")));
        assert!(text.contains("\"refresh_reuse\""));
        // never paired: no reason, and a guess
        let e = check(&r, &k, "d_never", "X1", "X2").unwrap_err();
        assert_eq!((e.1, e.3.as_deref(), e.4), (error_codes::DEVICE_REVOKED, Some(GUESS_DEVICE_ID), None));
        // 30 days later the tombstones are gone: no reason any more
        r.clock.advance_ms(REVOKED_KEPT_DAYS * DAY_MS);
        r.svc.check();
        assert!(r.svc.lock().revoked.is_empty());
        let e = check(&r, &ku, &du.id, "U1", "U2").unwrap_err();
        assert_eq!((e.1, e.4), (error_codes::DEVICE_REVOKED, None));
    }

    #[test]
    fn never_refreshed_devices_are_revoked_as_unclaimed() {
        let r = rig();
        let key = Key::new();
        let picked = register(&r, "alice", &key, "P1");
        let lost = register(&r, "alice", &Key::new(), "L1");
        check(&r, &key, &picked.id, "P1", "P2").unwrap();
        r.clock.advance_ms(UNCLAIMED_AFTER_MS - 2_000);
        assert!(r.svc.check().is_empty(), "not yet");
        r.clock.advance_ms(2_000);
        r.events.take();
        assert_eq!(r.svc.check(), vec![lost.id.clone()]);
        assert_eq!(r.svc.ids(), vec![picked.id.clone()]);
        let ev = r.events.take();
        assert_eq!(ev[0].code, "device.revoked");
        assert_eq!(ev[0].fields["reason"], "unclaimed");
    }

    #[test]
    fn a_replaced_account_revokes_its_devices() {
        let r = rig();
        let key = Key::new();
        let d = register(&r, "alice", &key, "R1");
        assert_eq!(r.svc.lock().devices[0].uid, Some(1000), "kept at the pairing");
        // a record from before the uid was kept: filled at the next refresh
        r.svc.lock().devices[0].uid = None;
        check(&r, &key, &d.id, "R1", "R2").unwrap();
        assert_eq!(r.svc.lock().devices[0].uid, Some(1000));
        // alice removed and made again (another uid): the refresh revokes
        ALICE_REPLACED.with(|f| f.set(true));
        r.events.take();
        assert_eq!(check(&r, &key, &d.id, "R2", "R3").unwrap_err().1, error_codes::DEVICE_REVOKED);
        assert!(r.svc.ids().is_empty());
        assert_eq!(r.events.take()[0].fields["reason"], "account_replaced");
        // the periodic check too; a device of the new account stays
        ALICE_REPLACED.with(|f| f.set(false));
        let old = register(&r, "alice", &Key::new(), "O1");
        ALICE_REPLACED.with(|f| f.set(true));
        let new = register(&r, "alice", &Key::new(), "N1");
        assert_eq!(r.svc.check(), vec![old.id.clone()]);
        assert_eq!(r.svc.ids(), vec![new.id]);
    }

    #[tokio::test]
    async fn targets_older_than_the_files_rule_get_it_once() {
        let r = rig();
        let engine = rynas_notify::engine::Engine::open(rynas_notify::engine::EngineOptions::new(
            r._dir.path().join("notify"),
            "nas",
        ))
        .unwrap();
        // two devices with push from before the flag: the first target has only the warnings
        // rule (made before the files rule existed), the second has both already
        let old = register(&r, "alice", &Key::new(), "A1");
        let both = register(&r, "alice", &Key::new(), "B1");
        let mut ids = Vec::new();
        for d in [&old, &both] {
            let t = engine
                .create_target(&TargetRequest {
                    name: target_name(&d.id, &d.name),
                    enabled: true,
                    language: NotifyLanguage::EN,
                    config: TargetConfig::Device { device_id: d.id.clone() },
                    secrets: Default::default(),
                })
                .unwrap();
            engine
                .create_rule(&NotifyRule {
                    name: target_name(&d.id, &d.name),
                    enabled: true,
                    min_severity: NotifySeverity::Warning,
                    targets: vec![t.id],
                    ..Default::default()
                })
                .unwrap();
            ids.push(t.id);
        }
        engine.create_rule(&files_rule(&both.id, &both.name, "alice", ids[1])).unwrap();
        {
            let mut reg = r.svc.lock();
            for d in reg.devices.iter_mut() {
                d.push = Some(sub());
            }
            r.svc.save(&reg).unwrap();
        }
        let files = |t: u64| {
            engine
                .rules()
                .unwrap()
                .into_iter()
                .filter(|x| x.targets.contains(&t) && x.categories.contains(&NotifyCategory::Files))
                .collect::<Vec<_>>()
        };
        r.svc.bind_engine(engine.clone());
        assert_eq!(files(ids[0]).len(), 1, "added to the old target");
        assert_eq!(files(ids[0])[0].fields[0].value, "alice");
        assert_eq!(files(ids[1]).len(), 1, "not doubled on the other");
        assert!(r.svc.lock().devices.iter().all(|d| d.files_rule_added));
        // idempotent
        r.svc.add_missing_files_rules();
        assert_eq!((files(ids[0]).len(), files(ids[1]).len()), (1, 1));
        // the administrator deletes it: a restart does not bring it back
        engine.delete_rule(files(ids[0])[0].id).unwrap();
        let again = DevicesSvc::new(
            r._dir.path(),
            Arc::new(rynas_notify::NullNotifier),
            AdminPolicy::new(vec!["rynas-admin".into()], groups),
            lookup,
            uid,
            Arc::new(|_: &str| Account::Ok),
            r.svc.trust.clone(),
            r.clock.clone(),
        )
        .unwrap();
        again.bind_engine(engine.clone());
        assert!(files(ids[0]).is_empty());
        // a target made now comes with both rules and is marked
        let d = register_in(&again, "alice", &Key::new(), "C1");
        again.set_push("alice", &d.id, sub(), None).unwrap();
        let t = engine
            .targets()
            .unwrap()
            .into_iter()
            .find(|t| matches!(&t.config, TargetConfig::Device { device_id } if *device_id == d.id))
            .unwrap();
        assert_eq!(files(t.id).len(), 1);
        assert!(again.lock().devices.iter().find(|x| x.id == d.id).unwrap().files_rule_added);
    }
}
