//! The file manager's pass-through: session and CSRF like every route, request bodies and
//! answers streamed (no 1 MiB cap as for the JSON proxy), the headers downloads and
//! previews need, no gzip over file contents. rynasd is a stub on a Unix socket here; the
//! real routes are tested in rynasd's `tests/files.rs`.

mod common;

use std::collections::HashSet;

use axum::body::Body;
use axum::extract::Request as AxumRequest;
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::any;
use common::{FakeAuth, build_with_rynasd};
use futures_util::StreamExt;
use rynas_api_types::error_codes;
use rynas_web::hardware::NoProvider;
use rynasd::server::PeerCheckedListener;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

/// What the stub rynasd answers: uploads are counted chunk by chunk, downloads are big.
async fn stub(req: AxumRequest) -> axum::response::Response {
    let user = req.headers().get("x-rynas-user").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let range = req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()).map(String::from);
    let path = req.uri().path().to_string();
    if path == "/v1/files/download" {
        let body = vec![b'x'; 3 << 20];
        return axum::response::Response::builder()
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(header::CONTENT_DISPOSITION, "attachment; filename=\"a.txt\"")
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, body.len())
            .body(Body::from(body))
            .unwrap();
    }
    let mut stream = req.into_body().into_data_stream();
    let mut bytes = 0usize;
    while let Some(chunk) = stream.next().await {
        bytes += chunk.map(|c| c.len()).unwrap_or(0);
    }
    axum::Json(json!({ "path": path, "user": user, "bytes": bytes, "range": range })).into_response()
}

use axum::response::IntoResponse;

#[tokio::test]
async fn files_stream_through_with_session_and_csrf() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("rynasd.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    // SAFETY: getuid cannot fail
    let me = unsafe { libc::getuid() };
    let allowed: HashSet<u32> = [me].into();
    tokio::spawn(async move {
        let app = axum::Router::new().route("/v1/files/{*r}", any(stub));
        axum::serve(PeerCheckedListener::new(listener, allowed), app).await.unwrap();
    });
    let app = build_with_rynasd(Arc::new(NoProvider), Arc::new(FakeAuth), socket);

    // no session: 401, like every route
    let r = app.get("/api/v1/files/list?path=/docs", &Default::default()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    let creds = app.login().await;
    let r = app.get("/api/v1/files/list?path=/docs", &creds).await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    let v: Value = r.json();
    assert_eq!((v["path"].as_str(), v["user"].as_str()), (Some("/v1/files/list"), Some("admin")));
    let upload = |csrf: bool, len: usize| {
        let mut b = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/files/upload?dir=/docs&name=a&id=abcdefgh&total=1")
            .header(header::COOKIE, format!("rynas_session={}", creds.cookie.clone().unwrap()))
            .header(header::CONTENT_LENGTH, len);
        if csrf {
            b = b.header("x-csrf-token", creds.csrf.clone().unwrap());
        }
        b.body(Body::from(vec![7u8; len])).unwrap()
    };
    // no CSRF header: refused before anything reaches rynasd
    let r = app.router.clone().oneshot(upload(false, 10)).await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    // 5 MiB: over the JSON proxy's 1 MiB, streamed whole
    let r = app.router.clone().oneshot(upload(true, 5 << 20)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["bytes"], 5 << 20);
    assert_eq!(v["user"], "admin");
    // over the upload limit: 413 at once
    let mut req = upload(true, 0);
    req.headers_mut().insert(header::CONTENT_LENGTH, (rynas_web::files::MAX_UPLOAD + 1).into());
    let r = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["code"], error_codes::PAYLOAD_TOO_LARGE);

    // a download: the range goes to rynasd, the headers come back, no gzip over it
    let req = Request::builder()
        .uri("/api/v1/files/download?path=/docs/a.txt")
        .header(header::COOKIE, format!("rynas_session={}", creds.cookie.clone().unwrap()))
        .header(header::ACCEPT_ENCODING, "gzip")
        .header(header::RANGE, "bytes=0-9")
        .body(Body::empty())
        .unwrap();
    let r = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.headers().get(header::CONTENT_ENCODING).is_none());
    assert_eq!(r.headers()[header::ACCEPT_RANGES], "bytes");
    assert!(r.headers()[header::CONTENT_DISPOSITION].to_str().unwrap().contains("a.txt"));
    let body = axum::body::to_bytes(r.into_body(), 8 << 20).await.unwrap();
    assert_eq!(body.len(), 3 << 20);
    let r = app.get("/api/v1/files/stat?path=/docs/a.txt", &creds).await;
    let req = Request::builder()
        .uri("/api/v1/files/view?path=/docs/a.txt")
        .header(header::COOKIE, format!("rynas_session={}", creds.cookie.clone().unwrap()))
        .header(header::RANGE, "bytes=0-9")
        .body(Body::empty())
        .unwrap();
    let rr = app.router.clone().oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(rr.into_body(), 1 << 20).await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["range"], "bytes=0-9");
    assert_eq!(r.status, StatusCode::OK);
}
