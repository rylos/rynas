//! `/api/v1/storage/...` through a fake rynasd on a Unix socket: session, CSRF, validation,
//! bay enrichment, the user header, error pass-through and mapping.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use common::{Creds, FakeAuth, TestApp, app, build_with_rynasd, fixture};
use rynas_api_types::storage::{
    DiskDetail, DiskSummary, PowerState, StoragePlan, VolumePlanRequest,
};
use rynas_api_types::{ApiError, error_codes};
use rynas_web::hardware::NoProvider;
use serde_json::{Value, json};

/// What the fake rynasd received: (path, x-rynas-user, body).
type Calls = Arc<Mutex<Vec<(String, Option<String>, Option<Value>)>>>;

fn disk(name: &str) -> Value {
    json!({"name": name, "path": format!("/dev/{name}"), "model": "M", "serial": "S",
           "wwn": null, "firmware": null, "size": 1, "kind": "hdd", "rotational": true,
           "transport": "sata", "bay": null, "power_state": "unknown",
           "rotation_rpm": null, "temperature_c": null, "power_on_hours": null,
           "health": "healthy", "health_reasons": [], "smart_age_s": 5,
           "used_by": [], "smr": false, "removable": false})
}

fn record(calls: &Calls, path: &str, h: &HeaderMap, body: Option<Value>) {
    let user = h
        .get("x-rynas-user")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    calls.lock().unwrap().push((path.to_string(), user, body));
}

async fn disks(State(c): State<Calls>, h: HeaderMap) -> Json<Value> {
    record(&c, "/v1/storage/disks", &h, None);
    Json(json!([disk("sda"), disk("sdc")]))
}

async fn one_disk(State(c): State<Calls>, h: HeaderMap, Path(name): Path<String>) -> Response {
    record(&c, &format!("/v1/storage/disks/{name}"), &h, None);
    match name.as_str() {
        "sdc" => Json(
            json!({"summary": disk("sdc"), "smart": null, "settings": [],
                             "partitions": []}),
        )
        .into_response(),
        "garbage" => Json(json!({"unexpected": true})).into_response(),
        _ => (
            StatusCode::NOT_FOUND,
            Json(ApiError::new(
                error_codes::NOT_FOUND,
                format!("{name}: not found"),
            )),
        )
            .into_response(),
    }
}

async fn list(State(c): State<Calls>, h: HeaderMap) -> Json<Value> {
    record(&c, "list", &h, None);
    Json(json!([]))
}

async fn plan_volume(
    State(c): State<Calls>,
    h: HeaderMap,
    body: Result<Json<VolumePlanRequest>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(error_codes::BAD_REQUEST, e.body_text())),
            )
                .into_response();
        }
    };
    record(
        &c,
        "/v1/storage/plan/volume",
        &h,
        Some(serde_json::to_value(&req).unwrap()),
    );
    Json(json!({"operation": "create_volume", "valid": true,
        "errors": [], "warnings": [], "raw_bytes": 0, "usable_bytes": 0,
        "fault_tolerance": 0, "wasted_bytes": 0, "summary": [], "steps": [
            {"description": "x", "type": "command", "argv": ["mdadm", "--create"]}]}))
    .into_response()
}

/// A fake rynasd serving the storage routes on a Unix socket.
fn fake_rynasd(name: &str) -> (PathBuf, Calls) {
    let dir = std::env::temp_dir().join(format!("rynas-web-st-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s.sock");
    let _ = std::fs::remove_file(&socket);
    let calls: Calls = Arc::default();
    let router = Router::new()
        .route("/v1/storage/disks", get(disks))
        .route("/v1/storage/disks/{name}", get(one_disk))
        .route("/v1/storage/arrays", get(list))
        .route("/v1/storage/filesystems", get(list))
        .route("/v1/storage/places", get(list))
        .route("/v1/storage/plan/volume", post(plan_volume))
        .with_state(calls.clone());
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (socket, calls)
}

fn with_fake(name: &str, hw: Arc<dyn rynas_web::hardware::HardwareProvider>) -> (TestApp, Calls) {
    let (socket, calls) = fake_rynasd(name);
    (build_with_rynasd(hw, Arc::new(FakeAuth), socket), calls)
}

#[tokio::test]
async fn disks_get_bays_from_the_provider() {
    let (a, calls) = with_fake(
        "bays",
        Arc::new(rynas_hw_synology::Synology::status_reader(fixture("synohwd-status.json"))),
    );
    let c = a.login().await;
    let r = a.get("/api/v1/storage/disks", &c).await;
    assert_eq!(r.status, StatusCode::OK);
    let v: Vec<DiskSummary> = r.json();
    let sdc = v.iter().find(|d| d.name == "sdc").unwrap();
    assert_eq!(sdc.bay, Some(4));
    // the fixture is old: no sleeping flag is trusted, the bay is
    assert_eq!(sdc.power_state, PowerState::Unknown);

    let r = a.get("/api/v1/storage/disks/sdc", &c).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json::<DiskDetail>().summary.bay, Some(4));
    // every call carries the session user (audit, guidelines A6)
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .all(|(_, u, _)| u.as_deref() == Some("admin"))
    );
}

#[tokio::test]
async fn validation_and_errors() {
    let (a, calls) = with_fake("errors", Arc::new(NoProvider));
    let anon = Creds::default();
    assert_eq!(
        a.get("/api/v1/storage/disks", &anon).await.status,
        StatusCode::UNAUTHORIZED
    );
    let c = a.login().await;
    let r = a.get("/api/v1/storage/disks/sdz", &c).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.code(), error_codes::NOT_FOUND);
    let r = a.get("/api/v1/storage/disks/SDA", &c).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.code(), error_codes::BAD_REQUEST);
    // rynasd answered something that is not a DiskDetail
    let r = a.get("/api/v1/storage/disks/garbage", &c).await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY);
    assert_eq!(r.code(), error_codes::BAD_GATEWAY);
    for p in ["arrays", "filesystems", "places"] {
        let r = a.get(&format!("/api/v1/storage/{p}"), &c).await;
        assert_eq!(r.status, StatusCode::OK, "{p}");
    }
    // nothing invalid reached rynasd
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .all(|(p, _, _)| !p.contains("SDA"))
    );
}

#[tokio::test]
async fn plans_need_csrf_and_valid_bodies() {
    let (a, calls) = with_fake("plans", Arc::new(NoProvider));
    let c = a.login().await;
    let body = json!({"disks": ["sdd"], "level": "raid1", "label": "vol2"});
    let no_csrf = Creds {
        cookie: c.cookie.clone(),
        csrf: None,
    };
    let r = a
        .send(
            Method::POST,
            "/api/v1/storage/plan/volume",
            &no_csrf,
            Some(body.clone()),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = a
        .send(Method::POST, "/api/v1/storage/plan/volume", &c, Some(body))
        .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let p: StoragePlan = r.json();
    assert!(p.valid);
    let sent = calls.lock().unwrap().last().unwrap().2.clone().unwrap();
    assert_eq!(sent["compress"], json!(true));
    let r = a
        .send(
            Method::POST,
            "/api/v1/storage/plan/volume",
            &c,
            Some(json!({"level": "raid9"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.code(), error_codes::BAD_REQUEST);
}

#[tokio::test]
async fn rynasd_down_is_503() {
    let a = app(Arc::new(NoProvider));
    let c = a.login().await;
    for p in ["tree", "disks"] {
        let r = a.get(&format!("/api/v1/storage/{p}"), &c).await;
        assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{p}");
        // one code for "rynasd unreachable" in every area (A12)
        assert_eq!(r.code(), error_codes::BACKEND_UNAVAILABLE, "{p}");
    }
    // the unprivileged overview keeps working
    assert_eq!(a.get("/api/v1/storage", &c).await.status, StatusCode::OK);
}
