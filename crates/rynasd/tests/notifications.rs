//! Notification endpoints of the internal API, the synodeb forward socket and rynasd's
//! own sources (TOTP changes, finished tasks).

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::notify::*;
use rynas_api_types::{ApiError, TotpEnrollment, error_codes};
use rynas_auth::ipc::USER_HEADER;
use rynas_auth::totp::{MemoryStorage, TotpManager, code_at};
use rynas_auth::{Clock, ManualClock};
use rynas_notify::MemoryNotifier;
use rynas_notify::engine::{Engine, EngineOptions};
use rynasd::server::{DaemonState, router};
use serde_json::{Value, json};
use tower::ServiceExt;

struct NoAuth;

impl rynas_auth::backend::Authenticator for NoAuth {
    fn realm(&self) -> &str {
        "pam"
    }
    fn authenticate<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> rynas_auth::backend::BoxFuture<
        'a,
        Result<rynas_auth::backend::Identity, rynas_auth::backend::AuthError>,
    > {
        Box::pin(async { Err(rynas_auth::backend::AuthError::InvalidCredentials) })
    }
}

const NOW: u64 = 1_800_000_000_000;

fn base_state(clock: Arc<ManualClock>) -> DaemonState {
    let totp = Arc::new(TotpManager::new(
        MemoryStorage::default(),
        clock.clone(),
        "rynas",
    ));
    DaemonState::new(Arc::new(NoAuth), totp, clock)
}

fn engine(dir: &std::path::Path) -> Engine {
    Engine::open(EngineOptions::new(dir.join("notify"), "nas")).unwrap()
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    user: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Option<String>, Vec<u8>) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(u) = user {
        req = req.header(USER_HEADER, u);
    }
    let req = req
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    let loc = r
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string());
    (
        s,
        loc,
        to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec(),
    )
}

fn code(b: &[u8]) -> String {
    serde_json::from_slice::<ApiError>(b).unwrap().code
}

fn json<T: serde::de::DeserializeOwned>(b: &[u8]) -> T {
    serde_json::from_slice(b).unwrap()
}

fn event(key: &str, sev: &str) -> Value {
    json!({"source": "storage", "category": "storage", "severity": sev, "key": key,
           "state": "firing", "title": format!("{key} broke")})
}

#[tokio::test]
async fn disabled_engine_is_503() {
    let app = router(base_state(Arc::new(ManualClock::new(NOW))));
    let (s, _, b) = call(&app, "GET", "/v1/notifications", None, None).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code(&b), error_codes::BACKEND_UNAVAILABLE);
}

#[tokio::test]
async fn notifications_api() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = base_state(Arc::new(ManualClock::new(NOW)));
    st.notifications = Some(engine(dir.path()));
    let app = router(st);

    // emit (internal) and list
    let (s, _, b) = call(
        &app,
        "POST",
        "/v1/notifications/events",
        None,
        Some(event("storage:md:md2:health", "critical")),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let first: NotifyAccepted = json(&b);
    assert!(first.id.is_some());
    call(
        &app,
        "POST",
        "/v1/notifications/events",
        None,
        Some(event("storage:disk:S1:health", "warning")),
    )
    .await;
    let (s, _, b) = call(
        &app,
        "POST",
        "/v1/notifications/events",
        None,
        Some(json!({"source": "x", "category": "app", "severity": "info", "title": ""})),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::BAD_REQUEST,
            error_codes::INVALID_VALUE.to_string()
        )
    );

    let (s, _, b) = call(&app, "GET", "/v1/notifications?limit=10", None, None).await;
    assert_eq!(s, StatusCode::OK);
    let all: Vec<Notification> = json(&b);
    assert_eq!(all.len(), 2);
    let (_, _, b) = call(
        &app,
        "GET",
        "/v1/notifications?severity=critical&category=storage&acked=false",
        None,
        None,
    )
    .await;
    assert_eq!(json::<Vec<Notification>>(&b).len(), 1);
    let (_, _, b) = call(
        &app,
        "GET",
        &format!("/v1/notifications?before={}", all[0].id),
        None,
        None,
    )
    .await;
    assert_eq!(json::<Vec<Notification>>(&b).len(), 1);
    let (s, _, b) = call(&app, "GET", "/v1/notifications?severity=loud", None, None).await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::BAD_REQUEST,
            error_codes::BAD_REQUEST.to_string()
        )
    );

    // ack
    let id = first.id.unwrap();
    let (s, _, b) = call(
        &app,
        "POST",
        &format!("/v1/notifications/{id}/ack"),
        None,
        None,
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::BAD_REQUEST,
            error_codes::BAD_REQUEST.to_string()
        )
    );
    let (s, _, b) = call(
        &app,
        "POST",
        &format!("/v1/notifications/{id}/ack"),
        Some("alice"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json::<Notification>(&b).acked_by.as_deref(), Some("alice"));
    let (s, _, b) = call(
        &app,
        "POST",
        "/v1/notifications/999/ack",
        Some("alice"),
        None,
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, error_codes::NOT_FOUND.to_string())
    );
    let (s, _, _) = call(
        &app,
        "POST",
        "/v1/notifications/abc/ack",
        Some("alice"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, _, b) = call(&app, "POST", "/v1/notifications/ack-all", Some("bob"), None).await;
    assert_eq!(json::<AckAllResult>(&b).acked, 1);

    // silences
    let (s, loc, b) = call(
        &app,
        "POST",
        "/v1/notifications/silence",
        Some("alice"),
        Some(json!({"key": "storage:disk:*", "duration_s": 3600})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let sil: Silence = json(&b);
    assert_eq!(
        loc.unwrap(),
        format!("/api/v1/notifications/silences/{}", sil.id)
    );
    let (_, _, b) = call(&app, "GET", "/v1/notifications/silences", None, None).await;
    assert_eq!(json::<Vec<Silence>>(&b).len(), 1);
    let (s, _, b) = call(
        &app,
        "POST",
        "/v1/notifications/silence",
        Some("alice"),
        Some(json!({"key": "", "duration_s": null})),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::BAD_REQUEST,
            error_codes::INVALID_VALUE.to_string()
        )
    );
    let uri = format!("/v1/notifications/silences/{}", sil.id);
    assert_eq!(
        call(&app, "DELETE", &uri, None, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, "DELETE", &uri, None, None).await.0,
        StatusCode::NOT_FOUND
    );

    // targets: secrets are write-only
    let target = json!({"name": "hook", "config": {"kind": "webhook", "url": "http://127.0.0.1:9/x"},
                        "secrets": {"token": "very-secret"}});
    let (s, loc, b) = call(
        &app,
        "POST",
        "/v1/notifications/targets",
        None,
        Some(target.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert!(!String::from_utf8_lossy(&b).contains("very-secret"));
    let t: NotifyTarget = json(&b);
    assert_eq!(
        loc.unwrap(),
        format!("/api/v1/notifications/targets/{}", t.id)
    );
    assert_eq!(t.secrets, ["token"]);
    let (s, _, b) = call(
        &app,
        "POST",
        "/v1/notifications/targets",
        None,
        Some(target),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::CONFLICT,
            error_codes::ALREADY_EXISTS.to_string()
        )
    );
    let (s, _, b) = call(
        &app,
        "POST",
        "/v1/notifications/targets",
        None,
        Some(json!({"name": "x", "config": {"kind": "ntfy", "server": "nope", "topic": "t"}})),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::BAD_REQUEST,
            error_codes::INVALID_VALUE.to_string()
        )
    );
    let (s, _, b) = call(
        &app,
        "POST",
        "/v1/notifications/targets",
        None,
        Some(json!({"name": "x", "config": {"kind": "pager"}})),
    )
    .await;
    // an unknown kind of target: not one of the accepted values, about `config.kind` (rynas_http)
    let e: rynas_api_types::ApiError = serde_json::from_slice(&b).unwrap();
    assert_eq!(e.field.as_deref(), Some("config.kind"));
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::BAD_REQUEST,
            error_codes::INVALID_VALUE.to_string()
        )
    );
    let turi = format!("/v1/notifications/targets/{}", t.id);
    let (s, _, b) = call(
        &app,
        "PUT",
        &turi,
        None,
        Some(json!({"name": "hook2", "enabled": false, "language": "it",
        "config": {"kind": "webhook", "url": "http://127.0.0.1:9/y"}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let t2: NotifyTarget = json(&b);
    assert_eq!(
        (t2.name.as_str(), t2.enabled, t2.language),
        ("hook2", false, NotifyLanguage::It)
    );
    assert_eq!(t2.secrets, ["token"]);
    let (s, _, b) = call(&app, "POST", &format!("{turi}/test"), Some("alice"), None).await;
    assert_eq!(s, StatusCode::OK);
    let r: TargetTestResult = json(&b);
    assert!(!r.ok && r.error.is_some());
    let (_, _, b) = call(&app, "GET", "/v1/notifications/targets", None, None).await;
    assert_eq!(json::<Vec<NotifyTarget>>(&b).len(), 2);

    // rules
    let (s, loc, b) = call(
        &app,
        "POST",
        "/v1/notifications/rules",
        None,
        Some(json!({"name": "crit", "min_severity": "error", "targets": [t.id]})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let r: NotifyRule = json(&b);
    assert_eq!(
        loc.unwrap(),
        format!("/api/v1/notifications/rules/{}", r.id)
    );
    let (s, _, b) = call(
        &app,
        "POST",
        "/v1/notifications/rules",
        None,
        Some(json!({"name": "bad", "targets": [77]})),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::BAD_REQUEST,
            error_codes::INVALID_VALUE.to_string()
        )
    );
    let (s, _, b) = call(&app, "DELETE", &turi, None, None).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, error_codes::CONFLICT.to_string())
    );
    let ruri = format!("/v1/notifications/rules/{}", r.id);
    let (s, _, b) = call(&app, "PUT", &ruri, None, Some(json!({"name": "crit", "min_severity": "critical", "targets": [t.id], "repeat_interval_s": 3600}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json::<NotifyRule>(&b).repeat_interval_s, 3600);
    assert_eq!(call(&app, "GET", &ruri, None, None).await.0, StatusCode::OK);
    assert_eq!(
        call(&app, "DELETE", &ruri, None, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, "GET", &ruri, None, None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "DELETE", &turi, None, None).await.0,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn change_stream() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = base_state(Arc::new(ManualClock::new(NOW)));
    let e = engine(dir.path());
    st.notifications = Some(e.clone());
    let app = router(st);
    let req = Request::builder()
        .uri("/v1/notifications/stream")
        .body(Body::empty())
        .unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    assert_eq!(r.headers()[header::CONTENT_TYPE], "text/event-stream");
    let mut body = r.into_body().into_data_stream();
    call(
        &app,
        "POST",
        "/v1/notifications/events",
        None,
        Some(event("k1", "error")),
    )
    .await;
    use futures_util::StreamExt;
    let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&chunk);
    assert!(
        text.starts_with("event: notification\ndata: {\"kind\":\"created\""),
        "{text}"
    );
    assert!(text.contains("\"unacked\":1"));
}

#[tokio::test]
async fn synodeb_socket() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path());
    let app = rynasd::notifications::socket_router(e.clone());
    let post = |ct: &'static str, body: &'static str| {
        let app = app.clone();
        async move {
            let req = Request::builder()
                .method("POST")
                .uri("/v1/notify")
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(body))
                .unwrap();
            let r = app.oneshot(req).await.unwrap();
            let s = r.status();
            (s, to_bytes(r.into_body(), 1 << 16).await.unwrap().to_vec())
        }
    };
    let (s, b) = post(
        "application/json",
        r#"{"title":"Fan 1 failed","message":"rpm 0","priority":"urgent","source":"synohwd"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let a: NotifyAccepted = json(&b);
    assert!(a.id.is_some());
    // the same message again: deduplicated (synodeb's repeat suppression)
    let (s, b) = post(
        "application/x-www-form-urlencoded",
        "title=Fan+1+failed&message=rpm+0&priority=urgent&source=synohwd",
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert_eq!(json::<NotifyAccepted>(&b).id, a.id);
    // -f: a new one
    let (_, b) = post(
        "application/x-www-form-urlencoded",
        "title=Fan+1+failed&message=rpm+0&priority=urgent&source=synohwd&force=1",
    )
    .await;
    assert_ne!(json::<NotifyAccepted>(&b).id, a.id);
    let (s, b) = post("text/plain", "hello").await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::BAD_REQUEST,
            error_codes::BAD_REQUEST.to_string()
        )
    );
    let (s, b) = post(
        "application/json",
        &*Box::leak(format!("{{\"title\":\"{}\"}}", "x".repeat(70_000)).into_boxed_str()),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            error_codes::PAYLOAD_TOO_LARGE.to_string()
        )
    );
    let list = e
        .list(&rynas_notify::engine::ListQuery {
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].source, "synodeb:synohwd");
    assert_eq!(list[0].category, NotifyCategory::Hardware);
    assert_eq!(list[0].severity, NotifySeverity::Critical);
}

#[tokio::test]
async fn totp_changes_are_emitted() {
    let clock = Arc::new(ManualClock::new(NOW));
    let mut st = base_state(clock.clone());
    let sink = Arc::new(MemoryNotifier::default());
    st.notifier = sink.clone();
    let app = router(st);
    let (s, _, b) = call(
        &app,
        "POST",
        "/v1/totp/alice/enroll",
        None,
        Some(json!({"account": "alice"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let enr: TotpEnrollment = json(&b);
    let c = code_at(&enr.secret, clock.now_ms() / 1000);
    let (s, _, _) = call(
        &app,
        "POST",
        "/v1/totp/alice/confirm",
        None,
        Some(json!({"code": c})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let ev = sink.take();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].category, NotifyCategory::Security);
    assert_eq!(ev[0].fields["user"], "alice");
}

#[tokio::test]
async fn failed_tasks_are_emitted() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = rynas_tasks::TaskManager::new(dir.path(), "nas", Default::default()).unwrap();
    let sink = Arc::new(MemoryNotifier::default());
    rynasd::notifications::spawn_task_watch(&tasks, sink.clone());
    let id = tasks
        .spawn("config.apply", "alice", |_| async {
            Err("testparm failed".to_string())
        })
        .unwrap();
    tasks.wait(&id).await;
    for _ in 0..50 {
        if !sink.0.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let ev = sink.take();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].key.as_deref(), Some("config:apply"));
    assert_eq!(ev[0].state, EventState::Firing);
    assert_eq!(ev[0].body, "testparm failed");
}
