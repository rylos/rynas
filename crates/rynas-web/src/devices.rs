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
//! `POST /devices/pairing/poll` (long poll, at most 25 s), `POST /auth/device/refresh`; they
//! have their own limiter ([`rynas_auth::AuthService::device_limiter`]: per client address,
//! the sign-in's delays) and their own fail2ban line, `device authentication failure from
//! IP` (filter `packaging/fail2ban/filter.d/rynas-devices.conf`). Only guesses count: an
//! unknown claim code, an unknown or used claim token, a bad signature, an unknown device.
//! A `ts` not newer, a role or account refusal, 503 and `clock_skew` count nothing.

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
use rynas_api_types::devices::internal::{
    GUESS_DEVICE_ID, GUESS_SIGNATURE, Live, RefreshCheck, RefreshOk, Register, Sid, Trust,
};
use rynas_api_types::devices::{
    ACCESS_TTL_S, AndroidApp, ClaimRequest, Device, DeviceRename, DeviceSelf, NasRef, PROTOCOL_VERSION,
    PairingApprove, PairingCreated, PairingState, PairingStatus, Ping, PollApproved, PollPending,
    PollRequest, REAUTH_WINDOW_S, REVOKED_KEPT_DAYS, RefreshRequest, RefreshResponse, clean_name,
    clock_skew_reason, revoke_reason,
};
use rynas_api_types::{UserInfo, error_codes};
use rynas_auth::device::{ClaimError, PairingError, PollOutcome, b64u, sha_b64u};
use rynas_auth::session::{DeviceRef, SessionData, SessionStore};
use rynas_auth::{Identity, random_token};

use crate::app::AppState;
use crate::auth::{CurrentSession, client_ip};
use crate::conn::Client;
use crate::error::Error;
use crate::rynasd_client::UpstreamError;

/// How often finished pairings are dropped and access tokens of devices revoked by rynasd
/// itself (expired, removed accounts, reuse) or of users who lost the role are ended. rynasd
/// is asked only while some device has a session ([`sweep`]); revocations made through
/// rynas-web end the sessions at once, not at the next sweep.
pub const SWEEP_EVERY: Duration = Duration::from_secs(60);
/// Revoked device ids remembered (the newest; older ones are long past any refresh in flight).
const MAX_REVOKED: usize = 4096;

/// rynas-web's memory of the devices: the NAS id (asked once from rynasd), when and from
/// where each device was last seen (merged into the lists; the store records it at each
/// refresh), and the poll's longest wait.
pub struct DeviceHub {
    sid: Mutex<Option<String>>,
    seen: Mutex<HashMap<String, (u64, String)>>,
    poll_wait: Duration,
    /// Devices revoked through rynas-web since it started (the web interface, the app's
    /// unpairing, unclaimed pairings): an access token issued while such a revocation was on
    /// its way is dropped right after it is made ([`DeviceHub::issue`]). rynasd's own
    /// revocations only end sessions ([`apply_live`]). With the reason and when: the reason
    /// goes with a `device_revoked` answer for [`REVOKED_KEPT_DAYS`], like rynasd's tombstones.
    revoked: Mutex<std::collections::VecDeque<Revoked>>,
}

struct Revoked {
    id: String,
    reason: String,
    at: std::time::Instant,
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
            revoked: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// The device `id` was revoked through rynas-web for `reason` (one of
    /// [`revoke_reason`]): its sessions end, and any made later are dropped at once.
    pub fn revoked(&self, sessions: &SessionStore, id: &str, reason: &str) {
        {
            let mut r = self.revoked.lock().unwrap_or_else(|e| e.into_inner());
            if !r.iter().any(|x| x.id == id) {
                r.push_back(Revoked {
                    id: id.to_string(),
                    reason: reason.to_string(),
                    at: std::time::Instant::now(),
                });
                if r.len() > MAX_REVOKED {
                    r.pop_front();
                }
            }
        }
        sessions.remove_device(id);
        self.forget(id);
    }

    fn is_revoked(&self, id: &str) -> bool {
        self.revoked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|x| x.id == id)
    }

    /// The reason recorded for the device `id` revoked through rynas-web, while it is kept
    /// ([`REVOKED_KEPT_DAYS`]).
    pub fn revoked_reason(&self, id: &str) -> Option<String> {
        let keep = Duration::from_secs(REVOKED_KEPT_DAYS * 86_400);
        self.revoked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|x| x.id == id && x.at.elapsed() < keep)
            .map(|x| x.reason.clone())
    }

    /// A device session for `device` of `user`: the access token handed to the phone, or
    /// `None` when the device was revoked meanwhile (the session made is dropped again: a
    /// revocation running at the same time removes it either before or after this check).
    pub fn issue(&self, sessions: &SessionStore, user: UserInfo, device: &Device) -> Option<String> {
        let (token, _) = sessions.create_device(
            user,
            DeviceRef {
                id: device.id.clone(),
                name: device.name.clone(),
            },
        );
        if self.is_revoked(&device.id) {
            sessions.remove_device(&device.id);
            return None;
        }
        Some(token)
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

/// The device limiter's name for a client: one per address, so a client guessing codes
/// cannot lock out the others (the device limiter's tables are its own: the web sign-in from
/// the same address is not affected).
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
        .route("/devices/android-app", get(android_app))
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
    let limiter = st.auth.device_limiter();
    let _attempt = match limiter.begin(ip, &key) {
        Ok(a) => a,
        Err(d) => return rate_limited(d),
    };
    // a body that does not parse tried no code: refused, not counted
    let Ok(Json(req)) = body else {
        return pairing_invalid();
    };
    let sid = match sid(&st).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match st.auth.pairings().claim(&sid, &req, &ip.to_string()) {
        Ok(r) => {
            limiter.record_success(ip, &key);
            tracing::info!(target: "rynas_auth", "pairing claimed from {ip}");
            Json(r).into_response()
        }
        Err(ClaimError::Invalid) => {
            limiter.record_failure(ip, &key);
            pairing_invalid()
        }
        Err(ClaimError::Malformed) => pairing_invalid(),
        Err(ClaimError::ClockSkew) => clock_skew(),
    }
}

/// 400 `clock_skew`, with the side likely wrong as `reason` (as rynasd answers a refresh).
fn clock_skew() -> Response {
    let mut e = Error::new(
        StatusCode::BAD_REQUEST,
        error_codes::CLOCK_SKEW,
        "the device's clock is more than 120 s away from the NAS'",
    );
    e.1.reason = Some(
        if host_clock_synced() { clock_skew_reason::DEVICE } else { clock_skew_reason::NAS }.to_string(),
    );
    e.into_response()
}

/// The kernel's view of the NAS clock (`adjtimex`, read only), as `timedatectl` reports it.
fn host_clock_synced() -> bool {
    // SAFETY: modes 0 only reads; the struct is plain data
    let mut t: libc::timex = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::adjtimex(&mut t) };
    r >= 0 && r != libc::TIME_ERROR && t.status & libc::STA_UNSYNC == 0
}

async fn poll(
    State(st): State<AppState>,
    client: Option<Extension<Client>>,
    body: Result<Json<PollRequest>, JsonRejection>,
) -> Response {
    let ip = client_ip(client);
    let key = limiter_key(ip);
    if let Err(d) = st.auth.device_limiter().check(ip, &key) {
        return rate_limited(d);
    }
    let Ok(Json(req)) = body else {
        return pairing_invalid();
    };
    let deadline = tokio::time::Instant::now() + st.devices.poll_wait;
    loop {
        let changed = st.auth.pairings().changed().notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        match st.auth.pairings().poll(&req.claim_token) {
            PollOutcome::Unknown => {
                st.auth.device_limiter().record_failure(ip, &key);
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

/// 401 `device_revoked`, with the reason when one is known.
fn revoked_answer(reason: Option<String>) -> Response {
    let mut e = Error::new(StatusCode::UNAUTHORIZED, error_codes::DEVICE_REVOKED, "device unknown or revoked");
    e.1.reason = reason;
    e.into_response()
}

async fn refresh(
    State(st): State<AppState>,
    client: Option<Extension<Client>>,
    body: Result<Json<RefreshRequest>, JsonRejection>,
) -> Response {
    let ip = client_ip(client);
    let key = limiter_key(ip);
    let limiter = st.auth.device_limiter();
    let _attempt = match limiter.begin(ip, &key) {
        Ok(a) => a,
        Err(d) => return rate_limited(d),
    };
    let invalid = || {
        err(StatusCode::UNAUTHORIZED, error_codes::REFRESH_INVALID, "refresh refused")
    };
    // malformed: nothing was tried, nothing counts
    let Ok(Json(req)) = body else {
        return invalid();
    };
    if req.refresh_token.is_empty() || req.refresh_token.len() > 128 || req.device_id.len() > 64 {
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
        admin_groups: st.auth.admin_groups().to_vec(),
    };
    let ok: RefreshOk = match st
        .rynasd
        .call(Method::POST, "/v1/devices/refresh", None, Some(&check))
        .await
    {
        Ok(ok) => ok,
        Err(UpstreamError::Api(status, mut e))
            if e.code == error_codes::DEVICE_REVOKED || e.code == error_codes::REFRESH_INVALID =>
        {
            // only an unknown device or a signature of another key is a guess (rynasd names
            // the field); a `ts` not newer, a role, the account, a reuse count nothing
            if matches!(e.field.as_deref(), Some(GUESS_DEVICE_ID | GUESS_SIGNATURE)) {
                limiter.record_failure(ip, &key);
            }
            if e.code == error_codes::DEVICE_REVOKED {
                // rynasd revoked it (or never had it): nothing can refresh it again, so its
                // sessions end without a permanent entry. rynasd's reason goes on unchanged;
                // without one, the reason of a revocation made here, if any
                st.auth.sessions().remove_device(&req.device_id);
                st.devices.forget(&req.device_id);
                if e.reason.is_none() {
                    e.reason = st.devices.revoked_reason(&req.device_id);
                }
            }
            e.field = None;
            return Error(status, e).into_response();
        }
        Err(e) => return Error::from(e).into_response(),
    };
    let identity = Identity {
        username: ok.device.user.clone(),
        groups: ok.groups.clone(),
    };
    // rynasd rotated only for a member of our administrator groups (`admin_groups`): the
    // role is there; the new refresh token is never thrown away
    let role = st.auth.role_of(&identity).unwrap_or_else(|| {
        tracing::error!("devices: refresh of {} rotated without a role here", ok.device.id);
        rynas_api_types::Role::Admin
    });
    limiter.record_success(ip, &key);
    let user = UserInfo {
        username: ok.device.user.clone(),
        realm: "pam".into(),
        role,
    };
    let Some(access) = st.devices.issue(st.auth.sessions(), user, &ok.device) else {
        return revoked_answer(st.devices.revoked_reason(&ok.device.id));
    };
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
    let Some(access) = st.devices.issue(st.auth.sessions(), s.user.clone(), &device) else {
        st.auth.pairings().approve_failed(&id);
        return revoked_answer(st.devices.revoked_reason(&device.id));
    };
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

/// The Android app published on GitHub, as rynasd last read it (`rynasd::android_app`); an
/// unreachable rynasd is "unavailable" too: the page links the APK anyway.
async fn android_app(State(st): State<AppState>) -> Json<AndroidApp> {
    Json(
        st.rynasd
            .call(Method::GET, "/v1/devices/android-app", None, None::<&()>)
            .await
            .unwrap_or_default(),
    )
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
    st.devices.revoked(st.auth.sessions(), id, reason);
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
    revoke_device(&st, &s, &id, revoke_reason::USER).await
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
    revoke_device(&st, &s, &id, revoke_reason::SELF).await
}

/// `PUT /devices/self/push`: the device's Web Push subscription (rynasd checks it and makes
/// the device's notification target).
async fn push_put(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
    body: Result<Json<rynas_api_types::devices::PushRegistration>, JsonRejection>,
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
        match r {
            // revoked here: the access token made at the approval ends too
            Ok(()) => st.devices.revoked(st.auth.sessions(), &id, revoke_reason::UNCLAIMED),
            Err(e) => tracing::warn!("devices: unclaimed device {id} not revoked: {e:?}"),
        }
    }
    // no device session: nothing rynasd could have revoked here (no call, no wakeup there)
    if st.auth.sessions().device_ids().is_empty() {
        return;
    }
    // sessions made from now on may belong to devices this list does not know yet
    let asked_ms = st.auth.pairings().now_ms();
    let live: Result<Vec<Live>, _> = st
        .rynasd
        .call(Method::GET, "/v1/devices/live", None, None::<&()>)
        .await;
    if let Ok(live) = live {
        apply_live(&st.auth, &st.devices, &live, asked_ms);
    }
}

/// rynasd's list of live devices, asked at `asked_ms`: the sessions of devices not in it end
/// (rynasd revoked them), and those of devices whose user lost the role (they refresh, and
/// rynasd refuses). Only sessions made before `asked_ms` are judged: a device paired or
/// refreshed while the list was on its way is not in it yet. Nothing is remembered: the
/// permanent revoked list ([`DeviceHub::revoked`]) is for revocations made through rynas-web.
/// A user whose groups could not be read keeps their sessions until the next sweep.
pub fn apply_live(auth: &rynas_auth::AuthService, hub: &DeviceHub, live: &[Live], asked_ms: u64) {
    for id in auth.sessions().device_ids() {
        match live.iter().find(|l| l.id == id) {
            None => {
                if auth.sessions().remove_device_before(&id, asked_ms) > 0 {
                    hub.forget(&id);
                }
            }
            Some(Live { groups: Some(g), .. }) => {
                let who = Identity {
                    username: String::new(),
                    groups: g.clone(),
                };
                if auth.role_of(&who).is_none() {
                    auth.sessions().remove_device_before(&id, asked_ms);
                }
            }
            Some(Live { groups: None, .. }) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_auth::Clock;

    fn admin() -> UserInfo {
        UserInfo {
            username: "admin".into(),
            realm: "pam".into(),
            role: rynas_api_types::Role::Admin,
        }
    }

    fn device(id: &str) -> Device {
        Device {
            id: id.into(),
            name: "Pixel".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_token_issued_while_a_revocation_runs_is_dropped() {
        let auth = rynas_auth::AuthService::with_clock(
            Vec::new(),
            rynas_auth::AuthConfig::default(),
            std::sync::Arc::new(rynas_auth::ManualClock::new(1_000)),
        );
        let hub = DeviceHub::default();
        let s = auth.sessions();
        let t = hub.issue(s, admin(), &device("d_1")).unwrap();
        assert!(s.get_bearer(&t).is_some());
        // the revocation lands between rynasd's answer and the session: no token survives
        hub.revoked(s, "d_1", revoke_reason::USER);
        assert_eq!(hub.revoked_reason("d_1").as_deref(), Some("user"));
        assert_eq!(hub.revoked_reason("d_2"), None);
        assert!(s.get_bearer(&t).is_none());
        assert!(hub.issue(s, admin(), &device("d_1")).is_none());
        assert!(s.device_ids().is_empty(), "the session made is gone again");
        assert!(hub.issue(s, admin(), &device("d_2")).is_some());
    }

    #[test]
    fn the_sweep_ends_revoked_devices_and_lost_roles() {
        let clock = std::sync::Arc::new(rynas_auth::ManualClock::new(1_000));
        let auth = rynas_auth::AuthService::with_clock(
            Vec::new(),
            rynas_auth::AuthConfig::default(),
            clock.clone(),
        );
        let hub = DeviceHub::default();
        let s = auth.sessions();
        let gone = hub.issue(s, admin(), &device("d_gone")).unwrap();
        let demoted = hub.issue(s, admin(), &device("d_demoted")).unwrap();
        let unknown = hub.issue(s, admin(), &device("d_unknown")).unwrap();
        let fine = hub.issue(s, admin(), &device("d_fine")).unwrap();
        clock.advance_ms(10);
        let live = [
            Live { id: "d_demoted".into(), groups: Some(vec!["users".into()]) },
            Live { id: "d_unknown".into(), groups: None },
            Live { id: "d_fine".into(), groups: Some(vec!["sudo".into()]) },
        ];
        apply_live(&auth, &hub, &live, clock.now_ms());
        assert!(s.get_bearer(&gone).is_none());
        assert!(s.get_bearer(&demoted).is_none());
        assert!(s.get_bearer(&unknown).is_some(), "a failed lookup decides nothing");
        assert!(s.get_bearer(&fine).is_some());
        // rynasd's own revocations are not remembered: only rynas-web's are permanent
        assert!(hub.issue(s, admin(), &device("d_gone")).is_some());
    }

    #[test]
    fn a_device_paired_while_the_live_list_travels_keeps_its_session() {
        let clock = std::sync::Arc::new(rynas_auth::ManualClock::new(1_000));
        let auth = rynas_auth::AuthService::with_clock(
            Vec::new(),
            rynas_auth::AuthConfig::default(),
            clock.clone(),
        );
        let hub = DeviceHub::default();
        let s = auth.sessions();
        let old = hub.issue(s, admin(), &device("d_old")).unwrap();
        clock.advance_ms(5);
        // the sweep asks rynasd for the list...
        let asked = clock.now_ms();
        clock.advance_ms(5);
        // ...a phone is approved (or refreshes) meanwhile: not in the list yet
        let new = hub.issue(s, admin(), &device("d_new")).unwrap();
        let newer_of_old = hub.issue(s, admin(), &device("d_old")).unwrap();
        apply_live(&auth, &hub, &[], asked);
        assert!(s.get_bearer(&new).is_some(), "made after the list was asked for");
        assert!(s.get_bearer(&newer_of_old).is_some());
        assert!(s.get_bearer(&old).is_none(), "made before: judged");
        // and nothing is remembered: the next sweep, with the device listed, keeps it
        let later = clock.now_ms() + 1;
        apply_live(&auth, &hub, &[Live { id: "d_new".into(), groups: Some(vec!["sudo".into()]) }], later);
        assert!(s.get_bearer(&new).is_some());
        assert!(hub.issue(s, admin(), &device("d_new")).is_some());
    }

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
