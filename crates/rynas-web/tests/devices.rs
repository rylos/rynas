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
        |u: &str| Ok(groups(u)),
        |u: &str| Ok(groups(u).map(|_| 1000)),
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
            Some(json!({"name": "  Ann's phone "})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    let dev: Device = r.json();
    assert_eq!((dev.name.as_str(), dev.user.as_str()), ("Ann's phone", "admin"));
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
    // a phone never shows an encrypted folder's recovery key...
    for p in ["/api/v1/crypt/folders/vol1/secret/reveal-recovery-key", "/api/v1/crypt/folders/vol1/secret/recovery-key"] {
        let r = bearer(app, Method::POST, p, &ok.access_token, Some(json!({"password": "x"}))).await;
        assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, error_codes::NOT_FOR_DEVICES.to_string()), "{p}");
    }
    // ...but keeps everything a bare-metal PC restore needs (past the device check: whatever
    // rynasd answers, it is not the device refusal)
    let r = bearer(app, Method::POST, "/api/v1/pcbackup/devices/laptop/restore-code", &ok.access_token, Some(json!({}))).await;
    assert!(r.status != StatusCode::FORBIDDEN || r.code() != error_codes::NOT_FOR_DEVICES, "{}", String::from_utf8_lossy(&r.body));
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
    // after the window, the token replaced most recently (r2) is refused, the device stays
    rig.app.clock.advance_ms(61_000);
    let r = public(app, "/api/v1/auth/device/refresh", phone.refresh(&sid, &dev.id, &r2.refresh_token, rig.ts())).await;
    assert_eq!((r.status, r.code()), (StatusCode::UNAUTHORIZED, error_codes::REFRESH_INVALID.to_string()));
    assert_eq!(rig.svc.ids(), vec![dev.id.clone()]);
    assert_eq!(bearer(app, Method::GET, "/api/v1/system", &r3.access_token, None).await.status, StatusCode::OK);
    // an older one (the first token) is a reuse: the device is revoked
    rig.app.clock.advance_ms(1_000);
    let r = public(app, "/api/v1/auth/device/refresh", phone.refresh(&sid, &dev.id, &ok.refresh_token, rig.ts())).await;
    assert_eq!((r.status, r.code()), (StatusCode::UNAUTHORIZED, error_codes::DEVICE_REVOKED.to_string()));
    assert_eq!(r.json::<rynas_api_types::ApiError>().reason.as_deref(), Some("refresh_reuse"));
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
    let e: rynas_api_types::ApiError = r.json();
    assert_eq!((e.code.as_str(), e.reason.as_deref(), e.field), ("device_revoked", Some("user"), None));

    // unpaired from the app itself
    let self_phone = Phone::new();
    let ok = pair(&rig, &creds, &self_phone).await;
    let r = bearer(app, Method::DELETE, "/api/v1/devices/self", &ok.access_token, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(rig.svc.ids().is_empty());
    let r = public(app, "/api/v1/auth/device/refresh", self_phone.refresh(&sid, &ok.device_id, &ok.refresh_token, rig.ts())).await;
    assert_eq!(r.json::<rynas_api_types::ApiError>().reason.as_deref(), Some("self"));

    // revoked by rynasd on its own while the access token lives: a bearer call rynasd answers
    // for the device carries the reason through rynas-web unchanged
    let ok = pair(&rig, &creds, &Phone::new()).await;
    rig.svc.revoke("admin", &ok.device_id, "inactive").unwrap();
    let r = bearer(app, Method::GET, "/api/v1/devices/self", &ok.access_token, None).await;
    let e: rynas_api_types::ApiError = r.json();
    assert_eq!((r.status, e.code.as_str(), e.reason.as_deref()), (StatusCode::UNAUTHORIZED, "device_revoked", Some("inactive")));
    // a device never paired: no reason
    let r = public(app, "/api/v1/auth/device/refresh", self_phone.refresh(&sid, "d_never", "x", rig.ts())).await;
    let e: rynas_api_types::ApiError = r.json();
    assert_eq!((e.code.as_str(), e.reason, e.field), ("device_revoked", None, None));

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
async fn two_refreshes_at_once_keep_the_device() {
    let rig = rig().await;
    let app = &rig.app;
    let creds = app.login().await;
    let phone = Phone::new();
    let ok = pair(&rig, &creds, &phone).await;
    let sid = rig.svc.sid().unwrap();
    let ts = rig.ts();
    // the app sends two refreshes of the same token at once (a retry signs anew)
    let a = public(app, "/api/v1/auth/device/refresh", phone.refresh(&sid, &ok.device_id, &ok.refresh_token, ts + 1));
    let b = public(app, "/api/v1/auth/device/refresh", phone.refresh(&sid, &ok.device_id, &ok.refresh_token, ts + 2));
    let (a, b) = tokio::join!(a, b);
    assert_eq!((a.status, b.status), (StatusCode::OK, StatusCode::OK), "{}", String::from_utf8_lossy(&b.body));
    let (a, b): (RefreshResponse, RefreshResponse) = (a.json(), b.json());
    // whichever answer the app keeps goes on working, and so does the other one
    let r = public(app, "/api/v1/auth/device/refresh", phone.refresh(&sid, &ok.device_id, &a.refresh_token, ts + 3)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    let r = public(app, "/api/v1/auth/device/refresh", phone.refresh(&sid, &ok.device_id, &b.refresh_token, ts + 4)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    // the same request replayed: refused, the device stays
    let replay = phone.refresh(&sid, &ok.device_id, &b.refresh_token, ts + 4);
    let r = public(app, "/api/v1/auth/device/refresh", replay).await;
    assert_eq!(r.code(), error_codes::REFRESH_INVALID);
    assert_eq!(rig.svc.ids(), vec![ok.device_id.clone()]);
}

const REFRESH: &str = "/api/v1/auth/device/refresh";
const CLAIM: &str = "/api/v1/devices/pairing/claim";

#[tokio::test]
async fn device_routes_have_their_own_limiter_and_count_only_guesses() {
    // the test limiter: 3 free failures per name, 10 per address, then 4 s and more
    let rig = rig().await;
    let app = &rig.app;
    let creds = app.login().await;
    let phone = Phone::new();
    let ok = pair(&rig, &creds, &phone).await;
    let sid = rig.svc.sid().unwrap();
    let ts = rig.ts();
    let r = public(app, REFRESH, phone.refresh(&sid, &ok.device_id, &ok.refresh_token, ts)).await;
    assert_eq!(r.status, StatusCode::OK);
    let mut token = r.json::<RefreshResponse>().refresh_token;

    // refusals that are not guesses, many times over: none counts
    let guest_phone = Phone::new();
    let guest = rig
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
    for i in 0..6 {
        // a `ts` not newer than the last accepted one
        let r = public(app, REFRESH, phone.refresh(&sid, &ok.device_id, &token, ts)).await;
        assert_eq!((r.status, r.code()), (StatusCode::UNAUTHORIZED, error_codes::REFRESH_INVALID.to_string()));
        assert!(r.json::<serde_json::Value>().get("field").is_none(), "no detail for the phone");
        // the phone's clock 10 minutes behind: told so
        let r = public(app, REFRESH, phone.refresh(&sid, &ok.device_id, &token, ts - 600)).await;
        assert_eq!((r.status, r.code()), (StatusCode::BAD_REQUEST, error_codes::CLOCK_SKEW.to_string()));
        // an account that may not sign in
        let r = public(app, REFRESH, guest_phone.refresh(&sid, &guest.id, "G1", ts + i)).await;
        assert_eq!(r.code(), error_codes::REFRESH_INVALID);
        // a body that does not parse tried nothing
        let r = public(app, CLAIM, json!({"v": 1})).await;
        assert_eq!(r.code(), error_codes::PAIRING_INVALID);
    }
    // a valid code claimed with a skewed clock: told so, and the code still works
    let p: PairingCreated = app.send(Method::POST, "/api/v1/devices/pairing", &creds, Some(json!({}))).await.json();
    let code = qr_param(&p.qr, "c");
    let other = Phone::new();
    for _ in 0..5 {
        let r = public(app, CLAIM, serde_json::to_value(other.claim(&sid, &code, rig.ts() + 300)).unwrap()).await;
        assert_eq!((r.status, r.code()), (StatusCode::BAD_REQUEST, error_codes::CLOCK_SKEW.to_string()));
    }
    let r = public(app, CLAIM, serde_json::to_value(other.claim(&sid, &code, rig.ts())).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    // nothing was counted: the device refreshes
    let r = public(app, REFRESH, phone.refresh(&sid, &ok.device_id, &token, ts + 10)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    token = r.json::<RefreshResponse>().refresh_token;

    // failed sign-ins lock the address for the sign-in only
    for i in 0..11 {
        let r = app
            .try_login(&rynas_api_types::LoginRequest {
                username: format!("nobody{i}"),
                password: "x".into(),
                totp: None,
            })
            .await;
        assert_ne!(r.status, StatusCode::OK);
    }
    let r = app.try_login(&rynas_api_types::LoginRequest { username: "admin".into(), password: "admin-pw".into(), totp: None }).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "the sign-in is locked");
    let r = public(app, REFRESH, phone.refresh(&sid, &ok.device_id, &token, ts + 11)).await;
    assert_eq!(r.status, StatusCode::OK, "the device routes are not");
    token = r.json::<RefreshResponse>().refresh_token;
    rig.app.clock.advance_ms(5_000);

    // guesses count, in the device limiter only: a signature of another key, an unknown
    // device, an unknown claim token, an unknown code
    let thief = Phone::new();
    let r = public(app, REFRESH, thief.refresh(&sid, &ok.device_id, &token, rig.ts())).await;
    assert_eq!(r.code(), error_codes::REFRESH_INVALID);
    assert!(r.json::<serde_json::Value>().get("field").is_none(), "the field is rynasd's, not the phone's");
    let r = public(app, REFRESH, thief.refresh(&sid, "d_nope", "x", rig.ts())).await;
    assert_eq!(r.code(), error_codes::DEVICE_REVOKED);
    let r = public(app, "/api/v1/devices/pairing/poll", json!({"claim_token": "nope"})).await;
    assert_eq!(r.code(), error_codes::PAIRING_INVALID);
    let r = public(app, CLAIM, serde_json::to_value(thief.claim(&sid, "nope", rig.ts())).unwrap()).await;
    assert_eq!(r.code(), error_codes::PAIRING_INVALID);
    // the fifth attempt is refused before it is checked
    let r = public(app, REFRESH, phone.refresh(&sid, &ok.device_id, &token, rig.ts() + 1)).await;
    assert_eq!((r.status, r.code()), (StatusCode::TOO_MANY_REQUESTS, error_codes::RATE_LIMITED.to_string()));
    // the web sign-in from the same address is not affected
    app.login().await;
    // the lock passes; the device was never touched
    rig.app.clock.advance_ms(5_000);
    let r = public(app, REFRESH, phone.refresh(&sid, &ok.device_id, &token, ts + 30)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
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
