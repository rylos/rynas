//! The interface's optional sections in the session answer (login and `GET
//! /api/v1/auth/session`): copied from rynasd's `GET /v1/ui/sections`, never waiting long for
//! it, the last answer standing in for a slow rynasd, and left out when unknown.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use common::{FakeAuth, app, build_with_rynasd};
use rynas_api_types::Session;
use rynas_web::hardware::NoProvider;
use serde_json::json;

const ANSWER: u8 = 0;
const SLOW: u8 = 1;

/// A rynasd that answers the sections (Docker installed) or hangs for 5 s.
async fn fake_rynasd(dir: &tempfile::TempDir, mode: Arc<AtomicU8>) -> std::path::PathBuf {
    let socket = dir.path().join("rynasd.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let fake = axum::Router::new().route(
        "/v1/ui/sections",
        axum::routing::get(move || {
            let mode = mode.clone();
            async move {
                if mode.load(Ordering::SeqCst) == SLOW {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                axum::Json(json!({ "sections": ["docker", "syncthing"] }))
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
    socket
}

#[tokio::test]
async fn sections_come_with_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let mode = Arc::new(AtomicU8::new(ANSWER));
    let socket = fake_rynasd(&dir, mode.clone()).await;
    let app = build_with_rynasd(Arc::new(NoProvider), Arc::new(FakeAuth), socket);
    let want = Some(vec!["docker".to_string(), "syncthing".to_string()]);

    let r = app
        .try_login(&rynas_api_types::LoginRequest {
            username: "admin".into(),
            password: "admin-pw".into(),
            totp: None,
        })
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json::<Session>().sections, want, "the login answer carries them");
    let c = app.login().await;
    let r = app.get("/api/v1/auth/session", &c).await;
    assert_eq!(r.json::<Session>().sections, want);

    // rynasd slow: the session answers within the short deadline, with the last sections
    mode.store(SLOW, Ordering::SeqCst);
    let t = Instant::now();
    let r = app.get("/api/v1/auth/session", &c).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(r.json::<Session>().sections, want);
}

#[tokio::test]
async fn unknown_sections_are_left_out() {
    // rynasd slow from the start: nothing known, nothing sent, no long wait
    let dir = tempfile::tempdir().unwrap();
    let socket = fake_rynasd(&dir, Arc::new(AtomicU8::new(SLOW))).await;
    let slow = build_with_rynasd(Arc::new(NoProvider), Arc::new(FakeAuth), socket);
    let c = slow.login().await;
    let t = Instant::now();
    let r = slow.get("/api/v1/auth/session", &c).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert!(!String::from_utf8_lossy(&r.body).contains("sections"));
    assert_eq!(r.json::<Session>().sections, None);

    // no rynasd at all: the same
    let bare = app(Arc::new(NoProvider));
    let c = bare.login().await;
    let r = bare.get("/api/v1/auth/session", &c).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json::<Session>().sections, None);
}
