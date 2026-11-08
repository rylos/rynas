//! Snapshot replication routes reach rynasd: every route rynasd exposes under
//! `/v1/snapshots/replication` is forwarded with its method (the list must not fall into
//! `/snapshots/{id}`). rynasd is a stub on a Unix socket echoing method and path.

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::Request as AxumRequest;
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use common::{FakeAuth, build_with_rynasd};
use rynas_web::hardware::NoProvider;
use rynasd::server::PeerCheckedListener;
use serde_json::{Value, json};

async fn stub(req: AxumRequest) -> axum::response::Response {
    axum::Json(json!({ "method": req.method().as_str(), "path": req.uri().path() })).into_response()
}

#[tokio::test]
async fn every_replication_route_is_forwarded() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("rynasd.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    // SAFETY: getuid cannot fail
    let me = unsafe { libc::getuid() };
    let allowed: HashSet<u32> = [me].into();
    tokio::spawn(async move {
        let app = axum::Router::new().route("/v1/snapshots/{*r}", any(stub));
        axum::serve(PeerCheckedListener::new(listener, allowed), app).await.unwrap();
    });
    let app = build_with_rynasd(Arc::new(NoProvider), Arc::new(FakeAuth), socket);

    // no session: 401 like every route
    let r = app.get("/api/v1/snapshots/replication", &Default::default()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    let c = app.login().await;
    let base = "/snapshots/replication";
    let cases = [
        (Method::GET, base.to_string()),
        (Method::POST, base.to_string()),
        (Method::GET, format!("{base}/ssh-key")),
        (Method::POST, format!("{base}/ssh-key")),
        (Method::POST, format!("{base}/host-keys")),
        (Method::GET, format!("{base}/daily/nas2")),
        (Method::PUT, format!("{base}/daily/nas2")),
        (Method::DELETE, format!("{base}/daily/nas2")),
        (Method::POST, format!("{base}/daily/nas2/run")),
        (Method::GET, format!("{base}/daily/nas2/snapshots")),
        (Method::GET, format!("{base}/daily/nas2/snapshots/docs.20261108T0300/browse")),
        (Method::POST, format!("{base}/daily/nas2/snapshots/docs.20261108T0300/restore")),
        (Method::POST, format!("{base}/daily/nas2/snapshots/docs.20261108T0300/promote")),
    ];
    for (m, p) in cases {
        let body = (m == Method::POST || m == Method::PUT).then(|| json!({}));
        let r = app.send(m.clone(), &format!("/api/v1{p}"), &c, body).await;
        assert_eq!(r.status, StatusCode::OK, "{m} {p}: {}", String::from_utf8_lossy(&r.body));
        let v: Value = r.json();
        assert_eq!(v["method"], m.as_str(), "{p}");
        assert_eq!(v["path"].as_str(), Some(format!("/v1{p}").as_str()));
    }
    // the plain snapshot routes still answer
    let r = app.send(Method::DELETE, "/api/v1/snapshots/abc", &c, None).await;
    assert_eq!(r.status, StatusCode::OK);
}
