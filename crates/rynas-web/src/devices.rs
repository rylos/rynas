//! Mobile devices, rynas-web's side (`docs/phase2/mobile-devices.md`).
//!
//! The split with rynasd: rynas-web holds what lives in memory and is checked on every
//! request — the pairings (codes, claims, the tokens waiting for the phone's poll:
//! [`rynas_auth::device::Pairings`]) and the access tokens, which are device sessions in the
//! same store as the web sessions ([`rynas_auth::session::SessionStore::create_device`]), so
//! [`crate::auth::require_session`] accepts a bearer with one map lookup and SSE streams,
//! downloads, uploads and thumbnails work unchanged. rynasd keeps the device store (public
//! keys, refresh token hashes) and checks every refresh: it has the keys on disk and the
//! system accounts (PAM). No request of a paired device goes to rynasd for its token.
//!
//! Public routes (no session): `GET /ping`, `POST /devices/pairing/claim`,
//! `POST /devices/pairing/poll` (long poll, at most 25 s), `POST /auth/device/refresh`; their
//! failures count in the login limiter and are logged for fail2ban like failed logins.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use rynas_api_types::devices::internal::{RefreshCheck, RefreshOk, Register, Sid, Trust};
use rynas_api_types::devices::{
    ACCESS_TTL_S, ClaimRequest, Device, DeviceRename, DeviceSelf, NasRef, PROTOCOL_VERSION,
    PairingApprove, PairingCreated, PairingState, PairingStatus, Ping, PollApproved, PollPending,
    PollRequest, REAUTH_WINDOW_S, RefreshRequest, RefreshResponse, clean_name,
};
use rynas_api_types::{UserInfo, error_codes};
use rynas_auth::device::{PairingError, PollOutcome, b64u, sha_b64u};
use rynas_auth::session::{DeviceRef, SessionData};
use rynas_auth::{Identity, random_token};

use crate::app::AppState;
use crate::auth::{CurrentSession, client_ip};
use crate::conn::Client;
use crate::error::Error;
use crate::rynasd_client::UpstreamError;

/// How often finished pairings are dropped and access tokens of devices revoked elsewhere
/// (expired, removed accounts) are ended.
pub const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// rynas-web's memory of the devices: the NAS id (asked once from rynasd), when and from
/// where each device was last seen (merged into the lists; the store records it at each
/// refresh), and the poll's longest wait.
pub struct DeviceHub {
    sid: Mutex<Option<String>>,
    seen: Mutex<HashMap<String, (u64, String)>>,
    poll_wait: Duration,
}

impl Default for DeviceHub {
    fn default() -> Self {
        Self::with_poll_wait(Duration::from_secs(rynas_api_types::devices::POLL_WAIT_S))
    }
}

impl DeviceHub {
    pub fn with_poll_wait(poll_wait: Duration) -> Self {
        Self {
            sid: Mutex::new(None),
            seen: Mutex::new(HashMap::new()),
            poll_wait,
        }
    }

    /// A request of the device `id` from `ip`.
    pub fn seen(&self, id: &str, ip: IpAddr, now_ms: u64) {
        let mut m = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        m.insert(id.to_string(), (now_ms, ip.to_string()));
    }

    fn merge(&self, d: &mut Device) {
        let m = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((ms, ip)) = m.get(&d.id)
            && *ms > d.last_seen_ms
        {
            d.last_seen_ms = *ms;
            d.last_ip = ip.clone();
        }
    }

    fn forget(&self, id: &str) {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }
}

fn err(status: StatusCode, code: &str, msg: &str) -> Response {
    Error::new(status, code, msg).into_response()
}

fn pairing_invalid() -> Response {
    err(StatusCode::BAD_REQUEST, error_codes::PAIRING_INVALID, "invalid pairing request")
}

fn rate_limited(d: Duration) -> Response {
    let secs = d.as_secs().max(1);
    let mut r = err(
        StatusCode::TOO_MANY_REQUESTS,
        error_codes::RATE_LIMITED,
        "too many failed attempts",
    );
    if let Ok(v) = axum::http::HeaderValue::from_str(&secs.to_string()) {
        r.headers_mut().insert(axum::http::header::RETRY_AFTER, v);
    }
    r
}

/// The limiter's name for the device routes of a client: one per address, so a client
/// guessing codes cannot lock out the others.
fn limiter_key(ip: IpAddr) -> String {
    format!("device@{}", rynas_auth::ratelimit::ip_key(ip))
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "nas".into())
}

/// This NAS' id, asked from rynasd once.
async fn sid(st: &AppState) -> Result<String, Error> {
    if let Some(s) = st.devices.sid.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Ok(s);
    }
    let s: Sid = st
        .rynasd
        .call(Method::GET, "/v1/devices/sid", None, None::<&()>)
        .await?;
    *st.devices.sid.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.sid.clone());
    Ok(s.sid)
}

/// Percent-encoding of a QR code parameter (RFC 3986 unreserved characters kept).
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The `rynas://pair?…` link of a pairing.
pub fn qr_link(sid: &str, name: &str, code: &str, trust: &Trust, expires_ms: u64) -> String {
    let mut q = format!(
        "rynas://pair?v={PROTOCOL_VERSION}&sid={}&n={}&c={}",
        enc(sid),
        enc(name),
        enc(code)
    );
    for u in &trust.endpoints {
        q.push_str(&format!("&u={}", enc(u)));
    }
    for p in &trust.tls_pins {
        q.push_str(&format!("&p={}", enc(p)));
    }
    q.push_str(&format!("&exp={}", expires_ms / 1000));
    q
}

/// The public routes (no session).
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/ping", get(ping))
        .route("/devices/pairing/claim", post(claim))
        .route("/devices/pairing/poll", post(poll))
        .route("/auth/device/refresh", post(refresh))
}

/// The routes of a signed-in user (web session or device), for their own devices; rynasd
/// lets an administrator manage everyone's.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/devices/pairing", post(pairing_create))
        .route(
            "/devices/pairing/{id}",
            get(pairing_get).delete(pairing_cancel),
        )
        .route("/devices/pairing/{id}/approve", post(pairing_approve))
        .route("/devices/pairing/{id}/reject", post(pairing_reject))
        .route("/devices", get(list))
        .route("/devices/self", get(self_get).patch(self_patch).delete(self_delete))
        .route("/devices/self/push", axum::routing::put(push_put).delete(push_delete))
        .route("/devices/{id}", axum::routing::patch(rename).delete(revoke))
}

async fn ping(State(st): State<AppState>) -> Response {
    match sid(&st).await {
        Ok(sid) => Json(Ping {
            sid,
            name: hostname(),
            version: env!("CARGO_PKG_VERSION").into(),
            api: PROTOCOL_VERSION,
        })
        .into_response(),
        Err(e) => e.into_response(),
    }
}

async fn claim(
    State(st): State<AppState>,
    client: Option<Extension<Client>>,
    body: Result<Json<ClaimRequest>, JsonRejection>,
) -> Response {
    let ip = client_ip(client);
    let key = limiter_key(ip);
    let limiter = st.auth.limiter();
    let _attempt = match limiter.begin(ip, &key) {
        Ok(a) => a,
        Err(d) => return rate_limited(d),
    };
    let Ok(Json(req)) = body else {
        limiter.record_failure(ip, &key);
        return pairing_invalid();
    };
    let sid = match sid(&st).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match st.auth.pairings().claim(&sid, &req, &ip.to_string()) {
        Some(r) => {
            limiter.record_success(ip, &key);
            tracing::info!(target: "rynas_auth", "pairing claimed from {ip}");
            Json(r).into_response()
        }
        None => {
            limiter.record_failure(ip, &key);
            pairing_invalid()
        }
    }
}

async fn poll(
    State(st): State<AppState>,
    client: Option<Extension<Client>>,
    body: Result<Json<PollRequest>, JsonRejection>,
) -> Response {
    let ip = client_ip(client);
    let key = limiter_key(ip);
    if let Err(d) = st.auth.limiter().check(ip, &key) {
        return rate_limited(d);
    }
    let Ok(Json(req)) = body else {
        st.auth.limiter().record_failure(ip, &key);
        return pairing_invalid();
    };
    let deadline = tokio::time::Instant::now() + st.devices.poll_wait;
    loop {
        let changed = st.auth.pairings().changed().notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        match st.auth.pairings().poll(&req.claim_token) {
            PollOutcome::Unknown => {
                st.auth.limiter().record_failure(ip, &key);
                return pairing_invalid();
            }
            PollOutcome::Rejected => {
                return err(StatusCode::FORBIDDEN, error_codes::PAIRING_REJECTED, "the pairing was rejected");
            }
            PollOutcome::Expired => {
                return err(StatusCode::GONE, error_codes::PAIRING_EXPIRED, "the pairing expired");
            }
            PollOutcome::Approved(a) => return Json(*a).into_response(),
            PollOutcome::Pending => {}
        }
        if tokio::time::timeout_at(deadline, changed).await.is_err() {
            return (
                StatusCode::ACCEPTED,
                Json(PollPending {
                    state: PairingState::Claimed,
                }),
            )
                .into_response();
        }
    }
}

/// A device session for `device` of `user`: the access token handed to the phone.
fn issue_access(st: &AppState, user: UserInfo, device: &Device) -> String {
    let (token, _) = st.auth.sessions().create_device(
        user,
        DeviceRef {
            id: device.id.clone(),
            name: device.name.clone(),
        },
    );
    token
}

async fn refresh(
    State(st): State<AppState>,
    client: Option<Extension<Client>>,
    body: Result<Json<RefreshRequest>, JsonRejection>,
) -> Response {
    let ip = client_ip(client);
    let key = limiter_key(ip);
    let limiter = st.auth.limiter();
    let _attempt = match limiter.begin(ip, &key) {
        Ok(a) => a,
        Err(d) => return rate_limited(d),
    };
    let invalid = || {
        err(StatusCode::UNAUTHORIZED, error_codes::REFRESH_INVALID, "refresh refused")
    };
    let Ok(Json(req)) = body else {
        limiter.record_failure(ip, &key);
        return invalid();
    };
    if req.refresh_token.is_empty() || req.refresh_token.len() > 128 || req.device_id.len() > 64 {
        limiter.record_failure(ip, &key);
        return invalid();
    }
    let new_token = random_token();
    let check = RefreshCheck {
        device_id: req.device_id.clone(),
        ts: req.ts,
        sig: req.sig.clone(),
        refresh_sha: sha_b64u(&req.refresh_token),
        new_refresh_sha: sha_b64u(&new_token),
        ip: ip.to_string(),
    };
    let ok: RefreshOk = match st
        .rynasd
        .call(Method::POST, "/v1/devices/refresh", None, Some(&check))
        .await
    {
        Ok(ok) => ok,
        Err(UpstreamError::Api(status, e))
            if e.code == error_codes::DEVICE_REVOKED || e.code == error_codes::REFRESH_INVALID =>
        {
            limiter.record_failure(ip, &key);
            if e.code == error_codes::DEVICE_REVOKED {
                st.auth.sessions().remove_device(&req.device_id);
                st.devices.forget(&req.device_id);
            }
            return Error(status, e).into_response();
        }
        Err(e) => return Error::from(e).into_response(),
    };
    let identity = Identity {
        username: ok.device.user.clone(),
        groups: ok.groups.clone(),
    };
    let Some(role) = st.auth.role_of(&identity) else {
        limiter.record_failure(ip, &key);
        return invalid();
    };
    limiter.record_success(ip, &key);
    let user = UserInfo {
        username: ok.device.user.clone(),
        realm: "pam".into(),
        role,
    };
    let access = issue_access(&st, user, &ok.device);
    st.devices.seen(&ok.device.id, ip, st.auth.pairings().now_ms());
    Json(RefreshResponse {
        access_token: access,
        access_expires_in: ACCESS_TTL_S,
        refresh_token: new_token,
    })
    .into_response()
}

fn pairing_error(e: PairingError) -> Response {
    match e {
        PairingError::TooMany => err(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            "at most 3 pairings may be open at once",
        ),
        PairingError::NotFound => err(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, "no such pairing"),
        PairingError::WrongState(s) => err(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            &format!("the pairing is {}", serde_json::to_string(&s).unwrap_or_default().trim_matches('"')),
        ),
    }
}

async fn pairing_create(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
) -> Response {
    let now = st.auth.pairings().now_ms();
    if now.saturating_sub(s.auth_ms) > REAUTH_WINDOW_S * 1000 {
        return err(
            StatusCode::FORBIDDEN,
            error_codes::REAUTH_REQUIRED,
            "sign in again (password, and TOTP code when enabled) to pair a device",
        );
    }
    let sid = match sid(&st).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let trust: Trust = match st
        .rynasd
        .call(Method::GET, "/v1/devices/trust", None, None::<&()>)
        .await
    {
        Ok(t) => t,
        Err(e) => return Error::from(e).into_response(),
    };
    let (id, code, expires_ms) = match st.auth.pairings().create(&s.user.username, &s.user.realm) {
        Ok(x) => x,
        Err(e) => return pairing_error(e),
    };
    tracing::info!(target: "rynas::audit", "devices: {} opened pairing {id}", s.user.username);
    let qr = qr_link(&sid, &hostname(), &code, &trust, expires_ms);
    Json(PairingCreated {
        pairing_id: id,
        expires_ms,
        qr,
    })
    .into_response()
}

async fn pairing_get(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
    Path(id): Path<String>,
) -> Result<Json<PairingStatus>, Error> {
    st.auth
        .pairings()
        .status(&id, &s.user.username, &s.user.realm)
        .map(Json)
        .ok_or_else(|| Error::not_found("no such pairing"))
}

async fn pairing_approve(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
    Path(id): Path<String>,
    body: Result<Json<PairingApprove>, JsonRejection>,
) -> Response {
    let req = match body {
        Ok(Json(b)) => b,
        Err(e) => return Error::from(e).into_response(),
    };
    let claim = match st.auth.pairings().begin_approve(&id, &s.user.username, &s.user.realm) {
        Ok(c) => c,
        Err(e) => return pairing_error(e),
    };
    let name = req
        .name
        .as_deref()
        .and_then(clean_name)
        .or_else(|| clean_name(&claim.device.name))
        .or_else(|| clean_name(&claim.device.model));
    let Some(name) = name else {
        st.auth.pairings().approve_failed(&id);
        let mut e = Error::new(StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE, "a name is needed");
        e.1.field = Some("name".into());
        return e.into_response();
    };
    let refresh_token = random_token();
    let reg = Register {
        name,
        device: claim.device.clone(),
        public_key: b64u(&claim.public_key),
        ip: claim.ip.clone(),
        refresh_sha: sha_b64u(&refresh_token),
    };
    let device: Device = match st
        .rynasd
        .call(Method::POST, "/v1/devices/register", Some(&s.user.username), Some(&reg))
        .await
    {
        Ok(d) => d,
        Err(e) => {
            st.auth.pairings().approve_failed(&id);
            return Error::from(e).into_response();
        }
    };
    let sid = sid(&st).await.unwrap_or_default();
    let access = issue_access(&st, s.user.clone(), &device);
    st.auth.pairings().approved(
        &id,
        PollApproved {
            device_id: device.id.clone(),
            name: device.name.clone(),
            user: device.user.clone(),
            nas: NasRef {
                sid,
                name: hostname(),
            },
            refresh_token,
            access_token: access,
            access_expires_in: ACCESS_TTL_S,
        },
    );
    Json(device).into_response()
}

async fn pairing_reject(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
    Path(id): Path<String>,
) -> Response {
    match st.auth.pairings().reject(&id, &s.user.username, &s.user.realm) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => pairing_error(e),
    }
}

async fn pairing_cancel(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
    Path(id): Path<String>,
) -> Response {
    match st.auth.pairings().cancel(&id, &s.user.username, &s.user.realm) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => pairing_error(e),
    }
}

fn current_device(s: &SessionData) -> Option<&str> {
    s.device.as_ref().map(|d| d.id.as_str())
}

async fn list(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
) -> Result<Json<Vec<Device>>, Error> {
    let mut v: Vec<Device> = st
        .rynasd
        .call(Method::GET, "/v1/devices", Some(&s.user.username), None::<&()>)
        .await?;
    for d in &mut v {
        st.devices.merge(d);
        d.current = current_device(&s) == Some(d.id.as_str());
    }
    Ok(Json(v))
}

async fn rename_device(st: &AppState, s: &SessionData, id: &str, body: DeviceRename) -> Result<Json<Device>, Error> {
    let mut d: Device = st
        .rynasd
        .call(
            Method::PATCH,
            &format!("/v1/devices/{}", enc(id)),
            Some(&s.user.username),
            Some(&body),
        )
        .await?;
    st.auth.sessions().rename_device(&d.id, &d.name);
    st.devices.merge(&mut d);
    d.current = current_device(s) == Some(d.id.as_str());
    Ok(Json(d))
}

async fn revoke_device(st: &AppState, s: &SessionData, id: &str, reason: &str) -> Result<StatusCode, Error> {
    let () = st
        .rynasd
        .call(
            Method::DELETE,
            &format!("/v1/devices/{}?reason={reason}", enc(id)),
            Some(&s.user.username),
            None::<&()>,
        )
        .await?;
    st.auth.sessions().remove_device(id);
    st.devices.forget(id);
    Ok(StatusCode::NO_CONTENT)
}

async fn rename(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
    Path(id): Path<String>,
    body: Result<Json<DeviceRename>, JsonRejection>,
) -> Result<Json<Device>, Error> {
    let Json(b) = body?;
    rename_device(&st, &s, &id, b).await
}

async fn revoke(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
    Path(id): Path<String>,
) -> Result<StatusCode, Error> {
    revoke_device(&st, &s, &id, "user").await
}

fn device_only() -> Error {
    Error::new(
        StatusCode::FORBIDDEN,
        error_codes::FORBIDDEN,
        "only a paired device has a devices/self",
    )
}

async fn self_get(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
) -> Result<Json<DeviceSelf>, Error> {
    let id = current_device(&s).ok_or_else(device_only)?;
    let mut d: DeviceSelf = st
        .rynasd
        .call(
            Method::GET,
            &format!("/v1/devices/{}/self", enc(id)),
            Some(&s.user.username),
            None::<&()>,
        )
        .await?;
    st.devices.merge(&mut d.device);
    Ok(Json(d))
}

async fn self_patch(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
    body: Result<Json<DeviceRename>, JsonRejection>,
) -> Result<Json<Device>, Error> {
    let id = current_device(&s).ok_or_else(device_only)?.to_string();
    let Json(b) = body?;
    rename_device(&st, &s, &id, b).await
}

async fn self_delete(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
) -> Result<StatusCode, Error> {
    let id = current_device(&s).ok_or_else(device_only)?.to_string();
    revoke_device(&st, &s, &id, "self").await
}

/// `PUT /devices/self/push`: the device's Web Push subscription (rynasd checks it and makes
/// the device's notification target).
async fn push_put(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
    body: Result<Json<rynas_api_types::devices::PushSubscription>, JsonRejection>,
) -> Result<Json<Device>, Error> {
    let id = current_device(&s).ok_or_else(device_only)?.to_string();
    let Json(sub) = body?;
    let mut d: Device = st
        .rynasd
        .call(
            Method::PUT,
            &format!("/v1/devices/{}/push", enc(&id)),
            Some(&s.user.username),
            Some(&sub),
        )
        .await?;
    st.devices.merge(&mut d);
    d.current = true;
    Ok(Json(d))
}

async fn push_delete(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
) -> Result<StatusCode, Error> {
    let id = current_device(&s).ok_or_else(device_only)?.to_string();
    let () = st
        .rynasd
        .call(
            Method::DELETE,
            &format!("/v1/devices/{}/push", enc(&id)),
            Some(&s.user.username),
            None::<&()>,
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Every [`SWEEP_EVERY`]: finished pairings dropped (devices approved and never picked up
/// revoked), access tokens of devices rynasd no longer has ended.
pub fn spawn_sweeper(st: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        loop {
            tick.tick().await;
            sweep(&st).await;
        }
    });
}

pub async fn sweep(st: &AppState) {
    for (id, user) in st.auth.pairings().purge() {
        let r: Result<(), _> = st
            .rynasd
            .call(
                Method::DELETE,
                &format!("/v1/devices/{}?reason=unclaimed", enc(&id)),
                Some(&user),
                None::<&()>,
            )
            .await;
        if let Err(e) = r {
            tracing::warn!("devices: unclaimed device {id} not revoked: {e:?}");
        }
    }
    let ids: Result<Vec<String>, _> = st
        .rynasd
        .call(Method::GET, "/v1/devices/ids", None, None::<&()>)
        .await;
    if let Ok(ids) = ids {
        st.auth.sessions().retain_devices(|id| ids.iter().any(|x| x == id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_link_encodes_every_field() {
        let t = Trust {
            sid: "s".into(),
            endpoints: vec!["https://192.168.1.10:8443".into(), "https://nas.example.org".into()],
            tls_pins: vec!["a-b_c".into()],
        };
        let q = qr_link("SID_x", "nas casa", "c0de", &t, 1_795_000_000_999);
        assert_eq!(
            q,
            "rynas://pair?v=1&sid=SID_x&n=nas%20casa&c=c0de\
             &u=https%3A%2F%2F192.168.1.10%3A8443&u=https%3A%2F%2Fnas.example.org&p=a-b_c&exp=1795000000"
        );
    }
}
