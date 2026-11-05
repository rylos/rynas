//! The internal API with a fake password backend, and the peer-credential check.

use std::collections::HashSet;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::{ApiError, TotpConfirmed, TotpEnrollment, error_codes};
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager, code_at};
use rynas_auth::{Clock, ManualClock, TotpStatus};
use rynasd::server::{DaemonState, PeerCheckedListener, router};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

struct Fake;

impl Authenticator for Fake {
    fn realm(&self) -> &str {
        "pam"
    }
    fn authenticate<'a>(
        &'a self,
        u: &'a str,
        p: &'a str,
    ) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async move {
            if u == "alice" && p == "pw" {
                Ok(Identity {
                    username: u.into(),
                    groups: vec!["sudo".into()],
                })
            } else {
                Err(AuthError::InvalidCredentials)
            }
        })
    }
}

fn state(clock: Arc<ManualClock>) -> DaemonState {
    let totp = Arc::new(TotpManager::new(
        MemoryStorage::default(),
        clock.clone(),
        "rynas",
    ));
    let mut st = DaemonState::new(Arc::new(Fake), totp, clock);
    st.groups = |u| (u == "alice").then(|| vec!["alice".into(), "sudo".into()]);
    st
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec())
}

fn code(b: &[u8]) -> String {
    serde_json::from_slice::<ApiError>(b).unwrap().code
}

#[tokio::test]
async fn authenticate_and_groups() {
    let app = router(state(Arc::new(ManualClock::new(1_800_000_000_000))));
    let (s, b) = call(
        &app,
        "POST",
        "/v1/authenticate",
        Some(json!({"username": "alice", "password": "pw"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let id: Identity = serde_json::from_slice(&b).unwrap();
    assert_eq!(id.groups, ["sudo"]);
    for (u, p) in [("alice", "x"), ("ghost", "pw"), ("../x", "pw")] {
        let (s, b) = call(
            &app,
            "POST",
            "/v1/authenticate",
            Some(json!({"username": u, "password": p})),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{u}");
        assert_eq!(code(&b), error_codes::INVALID_CREDENTIALS);
    }
    let (s, _) = call(
        &app,
        "POST",
        "/v1/authenticate",
        Some(json!({"user": "alice"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, b) = call(&app, "GET", "/v1/users/alice/groups", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Vec<String>>(&b).unwrap(),
        ["alice", "sudo"]
    );
    assert_eq!(
        call(&app, "GET", "/v1/users/ghost/groups", None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "GET", "/v1/users/..%2Fx/groups", None).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&app, "GET", "/v1/health", None).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "GET", "/nope", None).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn throttles_password_guessing() {
    let app = router(state(Arc::new(ManualClock::new(0))));
    for _ in 0..10 {
        let (s, _) = call(
            &app,
            "POST",
            "/v1/authenticate",
            Some(json!({"username": "alice", "password": "x"})),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    call(
        &app,
        "POST",
        "/v1/authenticate",
        Some(json!({"username": "alice", "password": "x"})),
    )
    .await;
    let (s, b) = call(
        &app,
        "POST",
        "/v1/authenticate",
        Some(json!({"username": "alice", "password": "pw"})),
    )
    .await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(code(&b), error_codes::RATE_LIMITED);
}

#[tokio::test]
async fn totp_endpoints() {
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let app = router(state(clock.clone()));
    let status = |b: &[u8]| serde_json::from_slice::<TotpStatus>(b).unwrap();
    let (s, b) = call(&app, "GET", "/v1/totp/alice", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!status(&b).enabled);
    let (s, b) = call(
        &app,
        "POST",
        "/v1/totp/alice/enroll",
        Some(json!({"account": "alice@nas"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let e: TotpEnrollment = serde_json::from_slice(&b).unwrap();
    let now = || code_at(&e.secret, clock.now_ms() / 1000).unwrap();
    let (s, b) = call(
        &app,
        "POST",
        "/v1/totp/alice/confirm",
        Some(json!({"code": "000000"})),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::INVALID_TOTP)
    );
    let (s, b) = call(
        &app,
        "POST",
        "/v1/totp/alice/confirm",
        Some(json!({"code": now()})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let rc = serde_json::from_slice::<TotpConfirmed>(&b)
        .unwrap()
        .recovery_codes;
    assert!(status(&call(&app, "GET", "/v1/totp/alice", None).await.1).enabled);
    let (s, b) = call(
        &app,
        "POST",
        "/v1/totp/alice/enroll",
        Some(json!({"account": "x"})),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, error_codes::TOTP_ALREADY_ENABLED)
    );
    clock.advance_ms(30_000);
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/totp/alice/verify",
            Some(json!({"code": now()}))
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/totp/alice/verify",
            Some(json!({"code": now()}))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/totp/alice/disable",
            Some(json!({"code": rc[0]}))
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (s, b) = call(
        &app,
        "POST",
        "/v1/totp/alice/verify",
        Some(json!({"code": rc[1]})),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, error_codes::TOTP_NOT_ENROLLED)
    );
    assert_eq!(
        call(&app, "GET", "/v1/totp/.hidden", None).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn peer_credentials_are_checked() {
    let dir = std::env::temp_dir().join(format!("rynasd-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let me = nix::unistd::getuid().as_raw();
    for (allowed, expect_answer) in [
        (HashSet::from([me]), true),
        (HashSet::from([me + 1]), false),
    ] {
        let path = dir.join(format!("s{expect_answer}.sock"));
        let _ = std::fs::remove_file(&path);
        let l = tokio::net::UnixListener::bind(&path).unwrap();
        let app = router(state(Arc::new(ManualClock::new(0))));
        tokio::spawn(async move {
            axum::serve(PeerCheckedListener::new(l, allowed), app)
                .await
                .unwrap();
        });
        let mut s = tokio::net::UnixStream::connect(&path).await.unwrap();
        let _ = s
            .write_all(b"GET /v1/health HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .await;
        let mut out = Vec::new();
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(5), s.read_to_end(&mut out)).await;
        let text = String::from_utf8_lossy(&out);
        assert_eq!(text.starts_with("HTTP/1.1 200"), expect_answer, "{text}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Over a real socket served by `rynasd::server::serve`, the root-only routes see the
/// connection's uid (`SO_PEERCRED`): a non-root peer gets 403, root reaches the handler
/// (503 here: no scheduler in this state).
#[tokio::test]
async fn root_only_routes_check_the_peer_uid() {
    let dir = std::env::temp_dir().join(format!("rynasd-root-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let me = nix::unistd::getuid().as_raw();
    let path = dir.join("s.sock");
    let l = tokio::net::UnixListener::bind(&path).unwrap();
    let app = router(state(Arc::new(ManualClock::new(0))));
    tokio::spawn(rynasd::server::serve(
        PeerCheckedListener::new(l, HashSet::from([me])),
        app,
        std::future::pending(),
    ));
    for (path_q, public) in [
        ("/v1/scheduler/tasks/t1/finished", false),
        ("/v1/scheduler/tasks/t1/execute", false),
        ("/v1/health", true),
    ] {
        let mut s = tokio::net::UnixStream::connect(&path).await.unwrap();
        let method = if public { "GET" } else { "POST" };
        let req = format!(
            "{method} {path_q} HTTP/1.1\r\nhost: x\r\nx-rynas-user: root\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(5), s.read_to_end(&mut out)).await;
        let text = String::from_utf8_lossy(&out);
        let expect = match (public, me) {
            (true, _) => "HTTP/1.1 200",
            (false, 0) => "HTTP/1.1 503",
            (false, _) => "HTTP/1.1 403",
        };
        assert!(text.starts_with(expect), "{path_q}: {text}");
        if expect.ends_with("403") {
            assert!(text.contains(error_codes::FORBIDDEN), "{text}");
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

fn storage_state() -> DaemonState {
    use rynas_storage::host::FakeHost;
    let fx = |n: &str| {
        std::fs::read_to_string(format!(
            "{}/../rynas-storage/tests/fixtures/{n}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    };
    let mut h = FakeHost {
        now_ms: 1_800_000_000_000,
        ..Default::default()
    };
    h.cmd(
        &format!("lsblk {}", rynas_storage::lsblk::LSBLK_ARGS.join(" ")),
        0,
        &fx("lsblk-nas.json"),
    );
    h.cmd(
        "smartctl --json=c -n standby -a -g apm -g wcache /dev/sda",
        0,
        &fx("smartctl-sata-hdd.json"),
    );
    h.file("/proc/mdstat", &fx("proc-mdstat.txt"));
    let mut st = state(Arc::new(ManualClock::new(1_800_000_000_000)));
    st.storage = Arc::new(rynas_storage::Storage::new(Arc::new(h)));
    st
}

#[tokio::test]
async fn storage_endpoints() {
    use rynas_api_types::storage::{DiskDetail, DiskSummary, HealthVerdict, MdArray, StoragePlan};
    let app = router(storage_state());
    let (s, b) = call(&app, "GET", "/v1/storage/disks", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let disks: Vec<DiskSummary> = serde_json::from_slice(&b).unwrap();
    assert_eq!(disks.len(), 7);
    assert_eq!(disks[0].name, "sda");
    assert_eq!(disks[0].health, HealthVerdict::Healthy);

    let (s, b) = call(&app, "GET", "/v1/storage/disks/sda", None).await;
    assert_eq!(s, StatusCode::OK);
    let d: DiskDetail = serde_json::from_slice(&b).unwrap();
    assert!(d.smart.is_some());

    let (s, b) = call(&app, "GET", "/v1/storage/disks/sdz", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(code(&b), "not_found");
    let (s, _) = call(&app, "GET", "/v1/storage/disks/..%2Fetc", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, b) = call(&app, "GET", "/v1/storage/arrays", None).await;
    assert_eq!(s, StatusCode::OK);
    let a: Vec<MdArray> = serde_json::from_slice(&b).unwrap();
    assert_eq!(a.len(), 6);

    let (s, b) = call(
        &app,
        "POST",
        "/v1/storage/plan/volume",
        Some(json!({"disks": ["sdd"], "level": "single", "label": "vol2"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let p: StoragePlan = serde_json::from_slice(&b).unwrap();
    assert!(p.valid, "{:?}", p.errors);
    let (s, b) = call(
        &app,
        "POST",
        "/v1/storage/plan/volume",
        Some(json!({"level": 3})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // a number where a level name goes: a stable code and the field (rynas_http)
    assert_eq!(code(&b), "invalid_value");
    let e: rynas_api_types::ApiError = serde_json::from_slice(&b).unwrap();
    assert_eq!(e.field.as_deref(), Some("level"));
}
