//! Notifications end to end: rynas-web (session, CSRF, proxy, /events, login lockouts) → a
//! real rynasd router with the engine on a Unix socket.

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::{Creds, FakeAuth, TestApp, build_with_rynasd};
use futures_util::StreamExt;
use rynas_api_types::notify::{Notification, NotifyCategory, NotifyTarget};
use rynas_api_types::{LoginRequest, error_codes};
use rynas_auth::ManualClock;
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_notify::engine::{Engine, EngineOptions, ListQuery};
use rynas_web::hardware::NoProvider;
use rynasd::server::{DaemonState, PeerCheckedListener, router};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;

struct Rig {
    app: TestApp,
    engine: Engine,
    _dir: TempDir,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path().join("notify"), "testnas")).unwrap();
    let clock = Arc::new(ManualClock::new(common::T0_MS));
    let totp = Arc::new(TotpManager::new(
        MemoryStorage::default(),
        clock.clone(),
        "rynas",
    ));
    let mut st = DaemonState::new(Arc::new(FakeAuth), totp, clock);
    st.notifications = Some(engine.clone());
    let socket = dir.path().join("rynasd.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    // SAFETY: getuid cannot fail and has no preconditions.
    let me = unsafe { libc::getuid() };
    let allowed: HashSet<u32> = [me].into();
    tokio::spawn(async move {
        axum::serve(PeerCheckedListener::new(listener, allowed), router(st))
            .await
            .unwrap();
    });
    let app = build_with_rynasd(Arc::new(NoProvider), Arc::new(FakeAuth), socket);
    Rig {
        app,
        engine,
        _dir: dir,
    }
}

fn firing(key: &str) -> rynas_api_types::notify::NewEvent {
    rynas_notify::event::firing(
        NotifyCategory::Storage,
        rynas_notify::NotifySeverity::Critical,
        "storage",
        key,
        format!("{key} broke"),
    )
}

#[tokio::test]
async fn proxied_endpoints() {
    let rig = rig().await;
    let app = &rig.app;
    // no session: 401
    let r = app.get("/api/v1/notifications", &Creds::default()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let c = app.login().await;
    rig.engine
        .ingest(firing("storage:md:md2:health"), false)
        .unwrap();
    let r = app.get("/api/v1/notifications?severity=error", &c).await;
    assert_eq!(r.status, StatusCode::OK);
    let list: Vec<Notification> = r.json();
    assert_eq!(list.len(), 1);
    // CSRF on writes
    let no_csrf = Creds {
        csrf: None,
        ..c.clone()
    };
    let r = app
        .send(
            Method::POST,
            "/api/v1/notifications/ack-all",
            &no_csrf,
            None,
        )
        .await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::FORBIDDEN, error_codes::CSRF.to_string())
    );
    // ack records the session user
    let r = app
        .send(
            Method::POST,
            &format!("/api/v1/notifications/{}/ack", list[0].id),
            &c,
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json::<Notification>().acked_by.as_deref(), Some("admin"));
    // create: 201 with Location passed through, secrets never echoed
    let r = app
        .send(
            Method::POST,
            "/api/v1/notifications/targets",
            &c,
            Some(json!({"name": "phone", "config": {"kind": "ntfy", "server": "https://ntfy.sh", "topic": "x"},
                        "secrets": {"token": "tk_very_secret"}})),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let t: NotifyTarget = r.json();
    assert_eq!(
        r.headers[header::LOCATION],
        format!("/api/v1/notifications/targets/{}", t.id).as_str()
    );
    assert!(!String::from_utf8_lossy(&r.body).contains("tk_very_secret"));
    let r = app
        .send(
            Method::DELETE,
            &format!("/api/v1/notifications/targets/{}", t.id),
            &c,
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.get("/api/v1/notifications/rules", &c).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = app.get("/api/v1/notifications/silences", &c).await;
    assert_eq!(r.status, StatusCode::OK);
    // internal-only routes are not exposed
    let r = app.get("/api/v1/notifications/stream", &c).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn notification_events_on_the_multiplexed_stream() {
    let rig = rig().await;
    let app = &rig.app;
    let c = app.login().await;
    let req = Request::builder()
        .uri("/api/v1/events")
        .header(
            header::COOKIE,
            format!("rynas_session={}", c.cookie.clone().unwrap()),
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await.unwrap();
    let seen = Arc::new(std::sync::Mutex::new(String::new()));
    let s2 = seen.clone();
    tokio::spawn(async move {
        let mut body = resp.into_body().into_data_stream();
        while let Some(Ok(chunk)) = body.next().await {
            s2.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&chunk));
        }
    });
    // let the notification half reach rynasd
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.engine
        .ingest(firing("storage:disk:S9:health"), false)
        .unwrap();
    let mut text = String::new();
    for _ in 0..100 {
        text = seen.lock().unwrap().clone();
        if text.contains("event: notification") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        text.contains("event: notification\ndata: {\"kind\":\"created\""),
        "{text}"
    );
    assert!(text.contains("storage:disk:S9:health"), "{text}");
}

#[tokio::test]
async fn login_lockout_is_notified_once() {
    let rig = rig().await;
    let app = &rig.app;
    let bad = LoginRequest {
        username: "admin".into(),
        password: "wrong".into(),
        totp: None,
    };
    let mut limited = 0;
    for _ in 0..6 {
        if app.try_login(&bad).await.status == StatusCode::TOO_MANY_REQUESTS {
            limited += 1;
        }
    }
    assert!(limited >= 2);
    let q = ListQuery {
        limit: 10,
        category: Some(NotifyCategory::Security),
        ..Default::default()
    };
    let mut found = Vec::new();
    for _ in 0..100 {
        found = rig.engine.list(&q).unwrap();
        if !found.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, "auth:lockout:admin");
    assert_eq!(found[0].count, 1, "debounced in rynas-web");
}
