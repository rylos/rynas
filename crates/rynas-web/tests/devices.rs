//! Mobile devices end to end: rynas-web (pairing, tokens, bearer sessions) → a real rynasd
//! router on a Unix socket (device store, refresh checks), with a software P-256 key as the
//! phone. A second rig puts a recording fake rynasd behind rynas-web for the routes whose
//! real backends need a machine (restart, files).

mod common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use common::{Creds, FakeAuth, Resp, TestApp, build_with_rynasd};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
use rynas_api_types::devices::internal::Register;
use rynas_api_types::devices::{
    ClaimRequest, ClaimResponse, Device, DeviceInfo, DeviceSelf, PairingCreated, PairingState,
    PairingStatus, Ping, PollApproved, PollPending, RefreshResponse,
};
use rynas_api_types::error_codes;
use rynas_auth::ManualClock;
use rynas_auth::device::{
    b64u, compare_code, key_hash, pair_message, refresh_message, sha_b64u, spki_of_point,
};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_web::hardware::NoProvider;
use rynasd::devices::{Account, AccountCheck, DevicesSvc, TrustProvider};
use rynasd::roles::AdminPolicy;
use rynasd::server::{DaemonState, PeerCheckedListener, router};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;

/// The phone: a P-256 key that never leaves it.
struct Phone {
    key: EcdsaKeyPair,
    rng: SystemRandom,
}

impl Phone {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let p = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, p.as_ref(), &rng).unwrap();
        Self { key, rng }
    }
    fn spki(&self) -> Vec<u8> {
        spki_of_point(self.key.public_key().as_ref())
    }
    fn sign(&self, m: &str) -> String {
        b64u(self.key.sign(&self.rng, m.as_bytes()).unwrap().as_ref())
    }
    fn claim(&self, sid: &str, code: &str, ts: i64) -> ClaimRequest {
        let spki = self.spki();
        ClaimRequest {
            v: 1,
            sid: sid.into(),
            code: code.into(),
            public_key: b64u(&spki),
            device: DeviceInfo {
                name: "Galaxy S25".into(),
                model: "SM-S931B".into(),
                os: "Android 16".into(),
                app_version: "0.1.0".into(),
            },
            ts,
            sig: self.sign(&pair_message(sid, code, &key_hash(&spki), ts)),
        }
    }
    fn refresh(&self, sid: &str, device: &str, token: &str, ts: i64) -> serde_json::Value {
        let sha = sha_b64u(token);
        json!({
            "device_id": device,
            "refresh_token": token,
            "ts": ts,
            "sig": self.sign(&refresh_message(sid, device, ts, &sha)),
        })
    }
}

fn groups(u: &str) -> Option<Vec<String>> {
    match u {
        "admin" => Some(vec!["rynas-admin".into()]),
        "guest" => Some(vec!["users".into()]),
        _ => None,
    }
}

struct Rig {
    app: TestApp,
    svc: Arc<DevicesSvc>,
    _dir: TempDir,
}

impl Rig {
    fn ts(&self) -> i64 {
        (self.app.clock.now_ms() / 1000) as i64
    }
}

use rynas_auth::Clock;

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("rynasd.sock");
    let app = build_with_rynasd(Arc::new(NoProvider), Arc::new(FakeAuth), socket.clone());
    let clock: Arc<ManualClock> = app.clock.clone();
    let totp = Arc::new(TotpManager::new(MemoryStorage::default(), clock.clone(), "rynas"));
    let mut st = DaemonState::new(Arc::new(FakeAuth), totp, clock.clone());
    let account: AccountCheck = Arc::new(|u: &str| match groups(u) {
        Some(_) => Account::Ok,
        None => Account::Gone,
    });
    let trust: TrustProvider = Arc::new(|| {
        Box::pin(async {
            (
                vec!["https://192.0.2.5:8443".to_string()],
                vec!["Q0EtcGlu".to_string(), "bGVhZi1waW4".to_string()],
            )
        })
    });
    let svc = DevicesSvc::new(
        &dir.path().join("state"),
        Arc::new(rynas_notify::NullNotifier),
        AdminPolicy::new(vec!["rynas-admin".into()], groups),
        groups,
        account,
        trust,
        clock,
    )
    .unwrap();
    st.devices = Some(svc.clone());
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    // SAFETY: getuid cannot fail and has no preconditions.
    let me = unsafe { libc::getuid() };
    let allowed: HashSet<u32> = [me].into();
    tokio::spawn(async move {
        axum::serve(PeerCheckedListener::new(listener, allowed), router(st))
            .await
            .unwrap();
    });
    Rig {
        app,
        svc,
        _dir: dir,
    }
}

/// A request with a device's access token (no cookie, no CSRF).
async fn bearer(app: &TestApp, method: Method, uri: &str, token: &str, body: Option<serde_json::Value>) -> Resp {
    let b = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    let req = match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string())),
        None => b.body(Body::empty()),
    }
    .unwrap();
    let r = app.router.clone().oneshot(req).await.unwrap();
    let (parts, body) = r.into_parts();
    let body = if parts
        .headers
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"))
    {
        Vec::new()
    } else {
        to_bytes(body, 1 << 24).await.unwrap().to_vec()
    };
    Resp {
        status: parts.status,
        headers: parts.headers,
        body,
    }
}

async fn public(app: &TestApp, uri: &str, body: serde_json::Value) -> Resp {
    app.send(Method::POST, uri, &Creds::default(), Some(body)).await
}

/// The `c` and `sid` parameters of a `rynas://pair?…` link.
fn qr_param(qr: &str, key: &str) -> String {
    qr.split_once('?')
        .unwrap()
        .1
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .unwrap()
        .to_string()
}

/// Pairs `phone` for the signed-in `creds`: QR, claim, approval, poll. Returns the poll's
/// answer.
async fn pair(rig: &Rig, creds: &Creds, phone: &Phone) -> PollApproved {
    let app = &rig.app;
    let r = app.send(Method::POST, "/api/v1/devices/pairing", creds, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    let p: PairingCreated = r.json();
    let (code, sid) = (qr_param(&p.qr, "c"), qr_param(&p.qr, "sid"));
    let c = public(app, "/api/v1/devices/pairing/claim", serde_json::to_value(phone.claim(&sid, &code, rig.ts())).unwrap()).await;
    assert_eq!(c.status, StatusCode::OK, "{}", String::from_utf8_lossy(&c.body));
    let c: ClaimResponse = c.json();
    let a = app
        .send(Method::POST, &format!("/api/v1/devices/pairing/{}/approve", p.pairing_id), creds, Some(json!({})))
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", String::from_utf8_lossy(&a.body));
    let r = public(app, "/api/v1/devices/pairing/poll", json!({"claim_token": c.claim_token})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    r.json()
}

#[tokio::test]
async fn pairing_tokens_rotation_reuse() {
    let rig = rig().await;
    let app = &rig.app;
    // discovery
    let ping: Ping = app.get("/api/v1/ping", &Creds::default()).await.json();
    assert_eq!((ping.api, ping.sid.len()), (1, 22));

    let creds = app.login().await;
    let r = app.send(Method::POST, "/api/v1/devices/pairing", &creds, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    let p: PairingCreated = r.json();
    assert!(p.qr.starts_with("rynas://pair?v=1&sid="));
    assert!(p.qr.contains("&u=https%3A%2F%2F192.0.2.5%3A8443&p=Q0EtcGlu&p=bGVhZi1waW4&exp="));
    let (code, sid) = (qr_param(&p.qr, "c"), qr_param(&p.qr, "sid"));
    assert_eq!(sid, ping.sid);
    // the pairing needs CSRF like every web write
    let mut no_csrf = creds.clone();
    no_csrf.csrf = None;
    let r = app.send(Method::POST, "/api/v1/devices/pairing", &no_csrf, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // a claim signed by another key: the generic error
    let phone = Phone::new();
    let mut forged = phone.claim(&sid, &code, rig.ts());
    forged.public_key = b64u(&Phone::new().spki());
    let r = public(app, "/api/v1/devices/pairing/claim", serde_json::to_value(&forged).unwrap()).await;
    assert_eq!((r.status, r.code()), (StatusCode::BAD_REQUEST, error_codes::PAIRING_INVALID.to_string()));

    let r = public(app, "/api/v1/devices/pairing/claim", serde_json::to_value(phone.claim(&sid, &code, rig.ts())).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    let claim: ClaimResponse = r.json();
    assert_eq!(claim.compare_code, compare_code(&code, &phone.spki()));
    // the code is used up
    let r = public(app, "/api/v1/devices/pairing/claim", serde_json::to_value(phone.claim(&sid, &code, rig.ts())).unwrap()).await;
    assert_eq!(r.code(), error_codes::PAIRING_INVALID);

    // the web user sees the claim and its compare code
    let st: PairingStatus = app.get(&format!("/api/v1/devices/pairing/{}", p.pairing_id), &creds).await.json();
    assert_eq!(st.state, PairingState::Claimed);
    let cl = st.claim.unwrap();
    assert_eq!((cl.compare_code, cl.device.model.as_str()), (claim.compare_code.clone(), "SM-S931B"));
    // the phone waits (the test harness waits 1.5 s, not 25 s)
    let r = public(app, "/api/v1/devices/pairing/poll", json!({"claim_token": claim.claim_token})).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert_eq!(r.json::<PollPending>().state, PairingState::Claimed);

    // a poll waiting while the user approves gets the answer at once
    let token = claim.claim_token.clone();
    let router = app.router.clone();
    let waiting = tokio::spawn(async move {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/devices/pairing/poll")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"claim_token": token}).to_string()))
            .unwrap();
        let r = router.oneshot(req).await.unwrap();
        let status = r.status();
        (status, to_bytes(r.into_body(), 1 << 20).await.unwrap())
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = app
        .send(
            Method::POST,
            &format!("/api/v1/devices/pairing/{}/approve", p.pairing_id),
            &creds,
            Some(json!({"name": "  Telefono Marco "})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    let dev: Device = r.json();
    assert_eq!((dev.name.as_str(), dev.user.as_str()), ("Telefono Marco", "admin"));
    let (status, body) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let ok: PollApproved = serde_json::from_slice(&body).unwrap();
    assert_eq!((ok.device_id.as_str(), ok.nas.sid.as_str(), ok.access_expires_in), (dev.id.as_str(), sid.as_str(), 900));
    // the tokens are given once
    let r = public(app, "/api/v1/devices/pairing/poll", json!({"claim_token": claim.claim_token})).await;
    assert_eq!(r.code(), error_codes::PAIRING_INVALID);

    // the bearer works everywhere a session does, without CSRF
    let me: DeviceSelf = bearer(app, Method::GET, "/api/v1/devices/self", &ok.access_token, None).await.json();
    assert!(me.device.current);
    assert_eq!(me.tls_pins, vec!["Q0EtcGlu".to_string(), "bGVhZi1waW4".to_string()]);
    assert_eq!(me.endpoints, vec!["https://192.0.2.5:8443".to_string()]);
    let r = bearer(app, Method::GET, "/api/v1/system", &ok.access_token, None).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = bearer(app, Method::GET, "/api/v1/events", &ok.access_token, None).await;
    assert_eq!(r.status, StatusCode::OK, "the event stream accepts the bearer");
    let r = bearer(app, Method::PATCH, "/api/v1/devices/self", &ok.access_token, Some(json!({"name": "Pixel"}))).await;
    assert_eq!(r.json::<Device>().name, "Pixel");
    let list: Vec<Device> = app.get("/api/v1/devices", &creds).await.json();
    assert_eq!((list.len(), list[0].name.as_str(), list[0].current), (1, "Pixel", false));
    // a bearer is not a cookie, a cookie is not a bearer
    let as_cookie = Creds {
        cookie: Some(ok.access_token.clone()),
        csrf: None,
    };
    assert_eq!(app.get("/api/v1/system", &as_cookie).await.status, StatusCode::UNAUTHORIZED);
    let cookie = creds.cookie.clone().unwrap();
    assert_eq!(bearer(app, Method::GET, "/api/v1/system", &cookie, None).await.status, StatusCode::UNAUTHORIZED);
    // the web session's own devices/self does not exist
    assert_eq!(app.get("/api/v1/devices/self", &creds).await.status, StatusCode::FORBIDDEN);

    // refresh: rotation
    let r = public(app, "/api/v1/auth/device/refresh", phone.refresh(&sid, &dev.id, &ok.refresh_token, rig.ts())).await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    let r2: RefreshResponse = r.json();
    assert_ne!(r2.refresh_token, ok.refresh_token);
    assert_eq!(bearer(app, Method::GET, "/api/v1/system", &r2.access_token, None).await.status, StatusCode::OK);
    // a signature of another key is refused
    let thief = Phone::new();
    let r = public(app, "/api/v1/auth/device/refresh", thief.refresh(&sid, &dev.id, &r2.refresh_token, rig.ts())).await;
    assert_eq!((r.status, r.code()), (StatusCode::UNAUTHORIZED, error_codes::REFRESH_INVALID.to_string()));
    // the answer was lost: the old token within 60 s still works, with a fresh pair
    rig.app.clock.advance_ms(20_000);
    let r = public(app, "/api/v1/auth/device/refresh", phone.refresh(&sid, &dev.id, &ok.refresh_token, rig.ts())).await;
    assert_eq!(r.status, StatusCode::OK, "grace window: {}", String::from_utf8_lossy(&r.body));
    let r3: RefreshResponse = r.json();
    // after the window, the replaced token is a reuse: the device is revoked
    rig.app.clock.advance_ms(61_000);
    let r = public(app, "/api/v1/auth/device/refresh", phone.refresh(&sid, &dev.id, &r2.refresh_token, rig.ts())).await;
    assert_eq!((r.status, r.code()), (StatusCode::UNAUTHORIZED, error_codes::DEVICE_REVOKED.to_string()));
    assert_eq!(bearer(app, Method::GET, "/api/v1/system", &r3.access_token, None).await.status, StatusCode::UNAUTHORIZED);
    let r = public(app, "/api/v1/auth/device/refresh", phone.refresh(&sid, &dev.id, &r3.refresh_token, rig.ts())).await;
    assert_eq!(r.code(), error_codes::DEVICE_REVOKED);
    assert!(rig.svc.ids().is_empty());
}

#[tokio::test]
async fn revoke_rename_and_expiry() {
    let rig = rig().await;
    let app = &rig.app;
    let creds = app.login().await;
    let phone = Phone::new();
    let ok = pair(&rig, &creds, &phone).await;
    // renamed from the web interface: the app sees it
    let r = app
        .send(Method::PATCH, &format!("/api/v1/devices/{}", ok.device_id), &creds, Some(json!({"name": "Tablet"})))
        .await;
    assert_eq!(r.json::<Device>().name, "Tablet");
    let me: DeviceSelf = bearer(app, Method::GET, "/api/v1/devices/self", &ok.access_token, None).await.json();
    assert_eq!(me.device.name, "Tablet");
    // revoked: the access token stops at once, the refresh too
    let r = app.send(Method::DELETE, &format!("/api/v1/devices/{}", ok.device_id), &creds, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(bearer(app, Method::GET, "/api/v1/devices/self", &ok.access_token, None).await.status, StatusCode::UNAUTHORIZED);
    let sid = rig.svc.sid().unwrap();
    let r = public(app, "/api/v1/auth/device/refresh", phone.refresh(&sid, &ok.device_id, &ok.refresh_token, rig.ts())).await;
    assert_eq!(r.code(), error_codes::DEVICE_REVOKED);

    // unpaired from the app itself
    let ok = pair(&rig, &creds, &Phone::new()).await;
    let r = bearer(app, Method::DELETE, "/api/v1/devices/self", &ok.access_token, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(rig.svc.ids().is_empty());

    // the access token lasts 15 minutes
    let ok = pair(&rig, &creds, &Phone::new()).await;
    rig.app.clock.advance_ms(900_000);
    assert_eq!(bearer(app, Method::GET, "/api/v1/system", &ok.access_token, None).await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn expired_codes_reauth_and_refused_accounts() {
    let rig = rig().await;
    let app = &rig.app;
    let creds = app.login().await;
    let p: PairingCreated = app.send(Method::POST, "/api/v1/devices/pairing", &creds, Some(json!({}))).await.json();
    let (code, sid) = (qr_param(&p.qr, "c"), qr_param(&p.qr, "sid"));
    rig.app.clock.advance_ms(301_000);
    let phone = Phone::new();
    let r = public(app, "/api/v1/devices/pairing/claim", serde_json::to_value(phone.claim(&sid, &code, rig.ts())).unwrap()).await;
    assert_eq!((r.status, r.code()), (StatusCode::BAD_REQUEST, error_codes::PAIRING_INVALID.to_string()));
    let st: PairingStatus = app.get(&format!("/api/v1/devices/pairing/{}", p.pairing_id), &creds).await.json();
    assert_eq!(st.state, PairingState::Expired);

    // more than 10 minutes after the sign-in: the password again
    rig.app.clock.advance_ms(300_000);
    let r = app.send(Method::POST, "/api/v1/devices/pairing", &creds, Some(json!({}))).await;
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, error_codes::REAUTH_REQUIRED.to_string()));
    let r = app.send(Method::POST, "/api/v1/auth/reauth", &creds, Some(json!({"password": "nope"}))).await;
    assert_eq!(r.code(), error_codes::INVALID_CREDENTIALS);
    let r = app.send(Method::POST, "/api/v1/auth/reauth", &creds, Some(json!({"password": "admin-pw"}))).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.send(Method::POST, "/api/v1/devices/pairing", &creds, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::OK);
    // a waiting pairing cancelled: its code is dead
    let p: PairingCreated = r.json();
    let r = app.send(Method::DELETE, &format!("/api/v1/devices/pairing/{}", p.pairing_id), &creds, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = public(app, "/api/v1/devices/pairing/claim", serde_json::to_value(phone.claim(&sid, &qr_param(&p.qr, "c"), rig.ts())).unwrap()).await;
    assert_eq!(r.code(), error_codes::PAIRING_INVALID);

    // a device of a user who may not sign in (not an administrator) gets no access token
    let guest_phone = Phone::new();
    let d = rig
        .svc
        .register(
            "guest",
            &Register {
                name: "Guest phone".into(),
                device: DeviceInfo::default(),
                public_key: b64u(&guest_phone.spki()),
                ip: String::new(),
                refresh_sha: sha_b64u("G1"),
            },
        )
        .unwrap();
    let r = public(app, "/api/v1/auth/device/refresh", guest_phone.refresh(&sid, &d.id, "G1", rig.ts())).await;
    assert_eq!((r.status, r.code()), (StatusCode::UNAUTHORIZED, error_codes::REFRESH_INVALID.to_string()));
    // an administrator sees every user's devices
    let list: Vec<Device> = app.get("/api/v1/devices", &creds).await.json();
    assert!(list.iter().any(|x| x.user == "guest"));
}

#[tokio::test]
async fn push_registration() {
    let rig = rig().await;
    let app = &rig.app;
    let creds = app.login().await;
    let ok = pair(&rig, &creds, &Phone::new()).await;
    let sub = json!({
        "endpoint": "https://ntfy.example.org/upXYZ?up=1",
        "p256dh": "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
        "auth": "BTBZMqHH6r4Tts7J_aSIgg",
    });
    let r = bearer(app, Method::PUT, "/api/v1/devices/self/push", &ok.access_token, Some(sub.clone())).await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    assert!(r.json::<Device>().push);
    let list: Vec<Device> = app.get("/api/v1/devices", &creds).await.json();
    assert!(list[0].push);
    // plain http is refused, with the field
    let mut http = sub.clone();
    http["endpoint"] = json!("http://ntfy.example.org/up");
    let r = bearer(app, Method::PUT, "/api/v1/devices/self/push", &ok.access_token, Some(http)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let e: rynas_api_types::ApiError = r.json();
    assert_eq!((e.code.as_str(), e.field.as_deref()), (error_codes::INVALID_VALUE, Some("endpoint")));
    // a web session has no push subscription of its own
    let r = app.send(Method::PUT, "/api/v1/devices/self/push", &creds, Some(sub)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = bearer(app, Method::DELETE, "/api/v1/devices/self/push", &ok.access_token, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let list: Vec<Device> = app.get("/api/v1/devices", &creds).await.json();
    assert!(!list[0].push);
}

/// rynas-web with a fake rynasd that records each request (path and body) and answers `{}`.
async fn recording_rig() -> (TestApp, Arc<Mutex<Vec<(Method, String, String)>>>, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("rynasd.sock");
    let seen: Arc<Mutex<Vec<(Method, String, String)>>> = Arc::default();
    let rec = seen.clone();
    let fake = axum::Router::new().fallback(move |req: Request| {
        let rec = rec.clone();
        async move {
            let (m, p) = (req.method().clone(), req.uri().path().to_string());
            let body = to_bytes(req.into_body(), 1 << 20).await.unwrap();
            rec.lock().unwrap().push((m, p, String::from_utf8_lossy(&body).into_owned()));
            axum::Json(json!({}))
        }
    });
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
    let app = build_with_rynasd(Arc::new(NoProvider), Arc::new(FakeAuth), socket);
    (app, seen, dir)
}

fn device_session(app: &TestApp) -> String {
    let user = rynas_api_types::UserInfo {
        username: "admin".into(),
        realm: "pam".into(),
        role: rynas_api_types::Role::Admin,
    };
    let (token, _) = app.auth.sessions().create_device(
        user,
        rynas_auth::session::DeviceRef {
            id: "d_0123".into(),
            name: "Pixel".into(),
        },
    );
    token
}

#[tokio::test]
async fn a_device_restarts_the_nas_and_downloads_files() {
    let (app, seen, _dir) = recording_rig().await;
    let token = device_session(&app);
    // restart: forwarded as for a web session of the same user, and a security notification
    let r = bearer(&app, Method::POST, "/api/v1/system/power", &token, Some(json!({"action": "restart", "delay_min": 0}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    let r = bearer(&app, Method::POST, "/api/v1/system/power/cancel", &token, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::OK);
    let mut notified = Vec::new();
    for _ in 0..100 {
        notified = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p, _)| p == "/v1/notifications/events")
            .map(|(_, _, b)| b.clone())
            .collect();
        if notified.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(notified.len(), 2, "{notified:?}");
    let ev: rynas_api_types::notify::NewEvent = serde_json::from_str(&notified[0]).unwrap();
    assert_eq!(ev.code, "device.power");
    assert_eq!(ev.fields["device"], "Pixel");
    assert_eq!(ev.fields["action"], "restart");
    let seen_now = seen.lock().unwrap().clone();
    assert!(seen_now.iter().any(|(m, p, b)| m == Method::POST && p == "/v1/system/power" && b.contains("restart")));
    // downloads and the file manager go through the same session check
    seen.lock().unwrap().clear();
    let r = bearer(&app, Method::GET, "/api/v1/files/download?path=/volume1/a.txt", &token, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(seen.lock().unwrap().iter().any(|(_, p, _)| p == "/v1/files/download"));
    // a device's token is not accepted once its sessions end (revoked elsewhere)
    app.auth.sessions().retain_devices(|_| false);
    assert_eq!(bearer(&app, Method::GET, "/api/v1/system", &token, None).await.status, StatusCode::UNAUTHORIZED);
}
