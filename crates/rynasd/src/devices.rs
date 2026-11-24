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
//! - Every 10 minutes: devices without a refresh for 180 days expire; devices of removed or
//!   disabled accounts are revoked.
//!
//! Internal routes (rynas-web only, never public; the user in `x-rynas-user`):
//!
//! | Method | Path | Body | Answer |
//! |---|---|---|---|
//! | GET | `/v1/devices/sid` | | `internal::Sid` |
//! | GET | `/v1/devices/trust` | | `internal::Trust` (sid, base URLs, TLS pins) |
//! | GET | `/v1/devices/ids` | | `Vec<String>`: the live devices (rynas-web drops the access tokens of the others) |
//! | POST | `/v1/devices/register` | `internal::Register` | `Device` (the approving user owns it) |
//! | POST | `/v1/devices/refresh` | `internal::RefreshCheck` | `internal::RefreshOk`; 401 `device_revoked` / `refresh_invalid` |
//! | GET | `/v1/devices` | | `Vec<Device>`: the user's; administrators everyone's |
//! | PATCH | `/v1/devices/{id}` | `DeviceRename` | `Device` (owner or administrator) |
//! | DELETE | `/v1/devices/{id}?reason=user\|self` | | 204 (owner or administrator) |
//! | GET | `/v1/devices/{id}/self` | | `DeviceSelf` (the owner: `devices/self` of the app) |

use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use rynas_api_types::devices::internal::{RefreshCheck, RefreshOk, Register, Sid, Trust};
use rynas_api_types::devices::{
    Device, DeviceRename, DeviceSelf, INACTIVE_EXPIRY_DAYS, REFRESH_GRACE_S, clean_name, clean_text,
};
use rynas_api_types::error_codes;
use rynas_auth::Clock;
use rynas_auth::backend::BoxFuture;
use rynas_auth::device::{
    b64u, b64u_decode, ct_eq_bytes, key_hash, new_id, p256_point, refresh_message, ts_ok, verify,
};
use rynas_notify::Notifier;
use rynas_notify::checks::{device_paired, device_refresh_reuse, device_renamed, device_revoked};
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

/// What the account behind a device is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Account {
    Ok,
    /// No such user any more.
    Gone,
    /// Locked, expired, or refused by PAM account management.
    Disabled,
}

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
    /// The token the last rotation replaced, accepted with a valid signature until
    /// `prev_until_ms` (a lost answer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prev_sha: Option<String>,
    #[serde(default)]
    prev_until_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Registry {
    #[serde(default)]
    devices: Vec<Record>,
}

pub struct DevicesSvc {
    dir: PathBuf,
    sid_path: PathBuf,
    reg: Mutex<Registry>,
    notifier: Arc<dyn Notifier>,
    admins: AdminPolicy,
    groups: fn(&str) -> Option<Vec<String>>,
    account: AccountCheck,
    trust: TrustProvider,
    clock: Arc<dyn Clock>,
}

pub type DevState = Option<Arc<DevicesSvc>>;

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

fn not_found() -> Error {
    Error::not_found("no such device")
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
        groups: fn(&str) -> Option<Vec<String>>,
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
        let svc = Arc::new(Self {
            dir,
            sid_path: state_dir.join("nas-sid"),
            reg: Mutex::new(reg),
            notifier,
            admins,
            groups,
            account,
            trust,
            clock,
        });
        svc.sid()?;
        Ok(svc)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.reg.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, reg: &Registry) -> Res<()> {
        let json = serde_json::to_vec_pretty(reg).map_err(Error::internal)?;
        write_private(&self.dir.join("devices.json"), &json).map_err(Error::internal)
    }

    /// This NAS' identity for the app, made on first use.
    pub fn sid(&self) -> std::io::Result<String> {
        match std::fs::read_to_string(&self.sid_path) {
            Ok(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
            Ok(_) | Err(_) => {
                let mut b = [0u8; 16];
                getrandom::fill(&mut b).map_err(std::io::Error::other)?;
                let sid = b64u(&b);
                write_private(&self.sid_path, format!("{sid}\n").as_bytes())?;
                Ok(sid)
            }
        }
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
            push: false,
            current: false,
        }
    }

    /// The live device ids.
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
            prev_sha: None,
            prev_until_ms: 0,
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
        }
        Ok(view)
    }

    /// Revokes the device `id`: it is gone from the store (its refresh token with it).
    pub fn revoke(&self, who: &str, id: &str, reason: &str) -> Res<()> {
        let rec = {
            let mut reg = self.lock();
            let i = reg
                .devices
                .iter()
                .position(|d| d.id == id && self.may_manage(who, d))
                .ok_or_else(not_found)?;
            let rec = reg.devices.remove(i);
            if let Err(e) = self.save(&reg) {
                reg.devices.insert(i, rec);
                return Err(e);
            }
            rec
        };
        audit(format_args!("{who} revoked {} ({}) of {}: {reason}", rec.id, rec.name, rec.user));
        self.notifier
            .emit(device_revoked(&rec.user, &rec.id, &rec.name, reason));
        Ok(())
    }

    /// Removes the devices `drop` picks, with one notification each.
    fn revoke_where(&self, reason: &str, pick: impl Fn(&Record) -> bool) -> Vec<String> {
        let gone: Vec<Record> = {
            let mut reg = self.lock();
            let (gone, keep): (Vec<Record>, Vec<Record>) =
                std::mem::take(&mut reg.devices).into_iter().partition(|d| pick(d));
            reg.devices = keep;
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
        }
        gone.into_iter().map(|r| r.id).collect()
    }

    /// The refresh of a device: signature, account, then the token with rotation, the grace
    /// window and reuse detection.
    pub fn refresh(&self, c: &RefreshCheck) -> Res<RefreshOk> {
        let now = self.clock.now_ms();
        let sid = self.sid().map_err(Error::internal)?;
        if !valid_sha(&c.refresh_sha) || !valid_sha(&c.new_refresh_sha) {
            return Err(refresh_invalid());
        }
        let (user, key) = {
            let reg = self.lock();
            let r = reg
                .devices
                .iter()
                .find(|d| d.id == c.device_id)
                .ok_or_else(revoked)?;
            (r.user.clone(), b64u_decode(&r.public_key).unwrap_or_default())
        };
        let msg = refresh_message(&sid, &c.device_id, c.ts, &c.refresh_sha);
        if !ts_ok(c.ts, now) || !verify(&key, &msg, &c.sig) {
            return Err(refresh_invalid());
        }
        match (self.account)(&user) {
            Account::Ok => {}
            Account::Gone | Account::Disabled => {
                self.revoke_where("account", |d| d.user == user);
                return Err(revoked());
            }
        }
        if !self.admins.is_admin(&user) {
            // the account may not sign in to the web interface (today: administrators)
            return Err(refresh_invalid());
        }
        let groups = (self.groups)(&user).unwrap_or_default();
        let mut reg = self.lock();
        let Some(i) = reg.devices.iter().position(|d| d.id == c.device_id) else {
            return Err(revoked());
        };
        let r = &mut reg.devices[i];
        let current = ct_eq_bytes(r.refresh_sha.as_bytes(), c.refresh_sha.as_bytes());
        let previous = !current
            && now < r.prev_until_ms
            && r
                .prev_sha
                .as_ref()
                .is_some_and(|p| ct_eq_bytes(p.as_bytes(), c.refresh_sha.as_bytes()));
        if !current && !previous {
            let rec = reg.devices.remove(i);
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
            return Err(revoked());
        }
        if current {
            r.prev_sha = Some(std::mem::take(&mut r.refresh_sha));
            r.prev_until_ms = now + REFRESH_GRACE_S * 1000;
        }
        // in the grace window the lost token is replaced, the previous one keeps its window
        r.refresh_sha = c.new_refresh_sha.clone();
        r.last_refresh_ms = now;
        r.last_seen_ms = now;
        r.last_ip = clean_text(&c.ip);
        let view = Self::view(r);
        self.save(&reg)?;
        Ok(RefreshOk {
            device: view,
            groups,
            grace: previous,
        })
    }

    /// The device `id` as its own app sees it, with the TLS pins and base URLs.
    pub async fn self_view(&self, who: &str, id: &str) -> Res<DeviceSelf> {
        let mut device = {
            let reg = self.lock();
            let r = reg
                .devices
                .iter()
                .find(|d| d.id == id && d.user == who)
                .ok_or_else(not_found)?;
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
        let sid = self.sid().map_err(Error::internal)?;
        let (endpoints, tls_pins) = (self.trust)().await;
        Ok(Trust {
            sid,
            endpoints,
            tls_pins,
        })
    }

    /// Expires devices without a refresh for [`INACTIVE_EXPIRY_DAYS`] and revokes the devices
    /// of removed or disabled accounts (blocking: PAM). Returns the revoked ids.
    pub fn check(&self) -> Vec<String> {
        let now = self.clock.now_ms();
        let limit = INACTIVE_EXPIRY_DAYS * DAY_MS;
        let mut out = self.revoke_where("inactive", |d| now.saturating_sub(d.last_refresh_ms) >= limit);
        let users: BTreeSet<String> = self.lock().devices.iter().map(|d| d.user.clone()).collect();
        for u in users {
            if (self.account)(&u) != Account::Ok {
                out.extend(self.revoke_where("account", |d| d.user == u));
            }
        }
        out
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
        .route("/v1/devices/ids", get(ids_get))
        .route("/v1/devices/register", post(register_post))
        .route("/v1/devices/refresh", post(refresh_post))
        .route("/v1/devices", get(list_get))
        .route("/v1/devices/{id}", patch(rename_patch).delete(revoke_delete))
        .route("/v1/devices/{id}/self", get(self_get))
        .with_state(st)
}

async fn sid_get(State(st): State<DevState>) -> Res<Json<Sid>> {
    let s = svc(&st)?;
    Ok(Json(Sid {
        sid: s.sid().map_err(Error::internal)?,
    }))
}

async fn trust_get(State(st): State<DevState>) -> Res<Json<Trust>> {
    Ok(Json(svc(&st)?.trust().await?))
}

async fn ids_get(State(st): State<DevState>) -> Res<Json<Vec<String>>> {
    Ok(Json(svc(&st)?.ids()))
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
        None | Some("user") => "user",
        Some("self") => "self",
        Some("unclaimed") => "unclaimed",
        Some(_) => return Err(Error::bad_request("reason: user, self or unclaimed")),
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

    struct Rig {
        svc: Arc<DevicesSvc>,
        clock: Arc<ManualClock>,
        events: Arc<MemoryNotifier>,
        gone: Arc<Mutex<Vec<String>>>,
        _dir: tempfile::TempDir,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::new(T0));
        let events = Arc::new(MemoryNotifier::default());
        let gone: Arc<Mutex<Vec<String>>> = Arc::default();
        let g = gone.clone();
        let account: AccountCheck = Arc::new(move |u: &str| {
            if g.lock().unwrap().iter().any(|x| x == u) || groups(u).is_none() {
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
            groups,
            account,
            trust,
            clock.clone(),
        )
        .unwrap();
        Rig { svc, clock, events, gone, _dir: dir }
    }

    fn register(r: &Rig, user: &str, key: &Key, token: &str) -> Device {
        r.svc
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

    fn check(r: &Rig, key: &Key, id: &str, old: &str, new: &str) -> Res<RefreshOk> {
        let sid = r.svc.sid().unwrap();
        let ts = (r.clock.now_ms() / 1000) as i64;
        let sha = sha_b64u(old);
        r.svc.refresh(&RefreshCheck {
            device_id: id.into(),
            ts,
            sig: key.sign(&refresh_message(&sid, id, ts, &sha)),
            refresh_sha: sha,
            new_refresh_sha: sha_b64u(new),
            ip: "192.0.2.10".into(),
        })
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
            groups,
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
        // bad signature: refused, nothing changes
        let other = Key::new();
        let e = check(&r, &other, &d.id, "R4", "R5").unwrap_err();
        assert_eq!(e.1, error_codes::REFRESH_INVALID);
        // the grace window is over: R3 is a reuse and the device goes
        r.clock.advance_ms(61_000);
        r.events.take();
        let e = check(&r, &key, &d.id, "R3", "R9").unwrap_err();
        assert_eq!((e.0, e.1), (StatusCode::UNAUTHORIZED, error_codes::DEVICE_REVOKED));
        assert!(r.svc.ids().is_empty());
        assert_eq!(r.events.take()[0].code, "device.refresh_reuse");
        // unknown device
        let e = check(&r, &key, &d.id, "R4", "R5").unwrap_err();
        assert_eq!(e.1, error_codes::DEVICE_REVOKED);
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
            })
            .unwrap_err();
        assert_eq!(e.1, error_codes::REFRESH_INVALID);
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
        let other = register(&r, "alice", &Key::new(), "O1");
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

    #[test]
    fn lan_urls_skip_tailscale_and_loopback() {
        let a: Vec<std::net::IpAddr> = ["127.0.0.1", "192.168.1.10", "100.101.2.3", "169.254.1.1", "2001:db8::1", "192.168.1.10"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(lan_urls(&a, 8443), vec!["https://192.168.1.10:8443".to_string()]);
    }
}
