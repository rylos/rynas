//! Router-level tests: status codes, JSON bodies, headers, SPA fallback.

mod common;

use std::sync::Arc;

use axum::http::{StatusCode, header};
use common::{Creds, app, fixture};
use rynas_api_types::{HardwareStatus, MetricsSample};
use rynas_web::hardware::NoProvider;

#[tokio::test]
async fn history_window_and_errors() {
    let app = app(Arc::new(NoProvider));
    let c = app.login().await;
    let r = app.get("/api/v1/metrics/history", &c).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers[header::CACHE_CONTROL], "no-store");
    let v: Vec<MetricsSample> = r.json();
    assert_eq!(v.len(), 600); // default 10 minutes
    let v: Vec<MetricsSample> = app
        .get("/api/v1/metrics/history?minutes=60", &c)
        .await
        .json();
    assert_eq!(v.len(), 600); // 1200 samples averaged in pairs
    assert_eq!(v.last().unwrap().ts_ms, 1_199_000);
    let r = app.get("/api/v1/metrics/history?minutes=0", &c).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.code(), "bad_request");
}

#[tokio::test]
async fn fonts_cached_and_gzip_only() {
    use axum::body::Body;
    use tower::ServiceExt;
    let app = app(Arc::new(NoProvider));
    let dir = common::ui_dir();
    std::fs::create_dir_all(dir.join("fonts")).unwrap();
    std::fs::write(dir.join("fonts/inter.woff2"), [0u8; 64]).unwrap();
    let r = app.get("/fonts/inter.woff2", &Creds::default()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers[header::CACHE_CONTROL], "public, max-age=86400");
    // a big API answer is gzipped even when the client prefers brotli
    let c = app.login().await;
    let req = axum::http::Request::get("/api/v1/metrics/history?minutes=60")
        .header(
            header::COOKIE,
            format!("rynas_session={}", c.cookie.unwrap()),
        )
        .header(header::ACCEPT_ENCODING, "br, gzip")
        .body(Body::empty())
        .unwrap();
    let r = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(r.headers()[header::CONTENT_ENCODING], "gzip");
}

#[tokio::test]
async fn rejections_are_api_errors() {
    use axum::http::Method;
    let app = app(Arc::new(NoProvider));
    // outside /api/v1 an unknown path says 401 without a session, like inside (A23)
    for p in ["/api/v2/x", "/api", "/api/v1/nothing"] {
        let r = app.get(p, &Creds::default()).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{p}");
        assert_eq!(r.code(), "unauthenticated", "{p}");
    }
    let c = app.login().await;
    let r = app.get("/api/v2/x", &c).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.code(), "not_found");
    // 405 with a JSON body and the Allow header kept
    let r = app.send(Method::DELETE, "/api/v1/system", &c, None).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(r.headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(r.code(), "method_not_allowed");
    assert!(r.headers.contains_key(header::ALLOW));
    // login without JSON
    let r = app
        .send(Method::POST, "/api/v1/auth/login", &Creds::default(), None)
        .await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(r.code(), "unsupported_media_type");
}

#[tokio::test]
async fn hardware_with_and_without_provider() {
    let a = app(Arc::new(NoProvider));
    let r = a.get("/api/v1/hardware", &a.login().await).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json::<HardwareStatus>(), HardwareStatus::default());

    let a = app(Arc::new(rynas_hw_synology::Synology::status_reader(fixture("synohwd-status.json"))));
    let r = a.get("/api/v1/hardware", &a.login().await).await;
    assert_eq!(r.status, StatusCode::OK);
    let h: HardwareStatus = r.json();
    assert_eq!(h.provider.as_deref(), Some("synology"));
    assert_eq!(h.bays.len(), 4);
    assert_eq!(h.fans.len(), 2);
}

#[tokio::test]
async fn unknown_api_is_json_404() {
    let app = app(Arc::new(NoProvider));
    let c = app.login().await;
    for uri in ["/api/v1/nope", "/api/v2/system", "/api"] {
        let r = app.get(uri, &c).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(r.code(), "not_found");
    }
}

#[tokio::test]
async fn spa_and_assets() {
    let app = app(Arc::new(NoProvider));
    let anon = Creds::default();
    for uri in ["/", "/storage/disks", "/index.html"] {
        let r = app.get(uri, &anon).await;
        assert_eq!(r.status, StatusCode::OK, "{uri}");
        assert!(String::from_utf8_lossy(&r.body).contains("shell"));
        let h = &r.headers;
        assert_eq!(h[header::CACHE_CONTROL], "no-cache");
        let csp = h[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.contains("'sha256-"), "{csp}");
        let script_src = csp
            .split(';')
            .find(|d| d.trim().starts_with("script-src"))
            .unwrap();
        assert!(!script_src.contains("unsafe-inline"), "{script_src}");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    }
    let r = app.get("/app-0123456789abcdef.js", &anon).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.headers[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        app.get("/missing-asset.js", &anon).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn system_and_storage_answer() {
    let app = app(Arc::new(NoProvider));
    let c = app.login().await;
    let r = app.get("/api/v1/system", &c).await;
    assert_eq!(r.status, StatusCode::OK);
    let v: rynas_api_types::SystemInfo = r.json();
    assert!(v.cpu_threads > 0);
    let r = app.get("/api/v1/storage", &c).await;
    assert_eq!(r.status, StatusCode::OK);
    let v: rynas_api_types::StorageOverview = r.json();
    assert!(v.filesystems.iter().any(|f| f.mountpoint == "/"));
}
