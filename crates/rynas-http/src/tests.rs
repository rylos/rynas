use axum::Json;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::Query;
use axum::http::Request;
use axum::routing::{get, post};
use serde::Deserialize;
use tower::ServiceExt;

use super::*;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Smb {
    workgroup: String,
    veto_files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Access {
    Ro,
    Rw,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Body1 {
    name: String,
    port: u16,
    smb: Option<Smb>,
    access: Option<Access>,
    members: Option<Vec<u32>>,
}

fn err(body: &str) -> Rejected {
    from_slice::<Body1>(body.as_bytes()).unwrap_err()
}

fn code_field(r: &Rejected) -> (StatusCode, &str, Option<&str>) {
    (r.0, r.1.code.as_str(), r.1.field.as_deref())
}

#[test]
fn json_errors_name_the_field_with_a_stable_code() {
    let r = err(r#"{"port": 1}"#);
    assert_eq!(
        code_field(&r),
        (
            StatusCode::BAD_REQUEST,
            error_codes::MISSING_FIELD,
            Some("name")
        )
    );
    assert_eq!(r.1.message, "name is required");

    let r = err(r#"{"name": "a", "port": "x"}"#);
    assert_eq!(
        code_field(&r),
        (
            StatusCode::BAD_REQUEST,
            error_codes::INVALID_TYPE,
            Some("port")
        )
    );
    let r = err(r#"{"name": "a", "port": 70000}"#);
    assert_eq!(r.1.code, error_codes::INVALID_VALUE);
    assert_eq!(r.1.field.as_deref(), Some("port"));

    let r = err(r#"{"name": "a", "port": 1, "smb": {"workgroup": "W", "veto_files": "x"}}"#);
    assert_eq!(r.1.code, error_codes::INVALID_TYPE);
    assert_eq!(r.1.field.as_deref(), Some("smb.veto_files"));
    let r = err(r#"{"name": "a", "port": 1, "smb": {"veto_files": []}}"#);
    assert_eq!(r.1.code, error_codes::MISSING_FIELD);
    assert_eq!(r.1.field.as_deref(), Some("smb.workgroup"));
    let r = err(r#"{"name": "a", "port": 1, "smb": {"workgroup": "W", "veto_files": [], "x": 1}}"#);
    assert_eq!(r.1.code, error_codes::UNKNOWN_FIELD);
    assert_eq!(r.1.field.as_deref(), Some("smb.x"));
    let r = err(r#"{"name": "a", "port": 1, "nope": 1}"#);
    assert_eq!(r.1.code, error_codes::UNKNOWN_FIELD);
    assert_eq!(r.1.field.as_deref(), Some("nope"));

    // serde_json calls a number for an enum a syntax error: still the field
    let r = err(r#"{"name": "a", "port": 1, "access": 3}"#);
    assert_eq!(r.1.code, error_codes::INVALID_VALUE);
    assert_eq!(r.1.field.as_deref(), Some("access"));

    let r = err(r#"{"name": "a", "port": 1, "access": "admin"}"#);
    assert_eq!(r.1.code, error_codes::INVALID_VALUE);
    assert_eq!(r.1.field.as_deref(), Some("access"));
    let r = err(r#"{"name": "a", "port": 1, "members": [1, "x"]}"#);
    assert_eq!(r.1.code, error_codes::INVALID_TYPE);
    assert_eq!(r.1.field.as_deref(), Some("members[1]"));

    // the whole body of the wrong type: no field
    let r = err("1");
    assert_eq!(
        (r.1.code.as_str(), r.1.field.as_deref()),
        (error_codes::INVALID_TYPE, None)
    );
}

#[test]
fn messages_never_carry_serde_text() {
    for body in [
        r#"{"port": 1}"#,
        r#"{"name": "a", "port": "x"}"#,
        r#"{"name": "a", "port": 1, "access": "admin"}"#,
        r#"{"name": "a", "port": 1, "nope": 1}"#,
        r#"{"name": "a""#,
        "not json",
        r#"{"name": "a", "port": 1} trailing"#,
    ] {
        let m = err(body).1.message;
        for leak in [
            "line",
            "column",
            "expected",
            "`",
            "invalid type",
            "deserialize",
        ] {
            assert!(!m.contains(leak), "{body}: {m}");
        }
    }
}

#[test]
fn syntax_errors_are_invalid_json() {
    for body in [
        r#"{"name": "a""#,
        "not json",
        "",
        r#"{"name":"a","port":1} x"#,
    ] {
        let r = err(body);
        assert_eq!(
            code_field(&r),
            (StatusCode::BAD_REQUEST, error_codes::INVALID_JSON, None),
            "{body}"
        );
    }
}

#[test]
fn values_parsed_first_report_the_same_way() {
    let v = serde_json::json!({"name": "a", "port": -1});
    let r = from_value::<Body1>(v).unwrap_err();
    assert_eq!(r.1.code, error_codes::INVALID_VALUE);
    assert_eq!(r.1.field.as_deref(), Some("port"));
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Limit {
    limit: Option<u32>,
}

async fn take_json(body: Result<Json<Body1>, JsonRejection>) -> Response {
    use axum::response::IntoResponse;
    match body {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            let (s, a) = json_rejection(e);
            (s, Json(a)).into_response()
        }
    }
}

fn app() -> Router {
    Router::new()
        .route("/q", get(|Query(_): Query<Limit>| async { "ok" }))
        .route("/j", post(take_json))
        .layer(axum::middleware::from_fn(json_errors))
}

async fn call(req: Request<Body>) -> (StatusCode, ApiError) {
    let r = app().oneshot(req).await.unwrap();
    let s = r.status();
    assert!(is_json(&r), "{s}");
    let b = to_bytes(r.into_body(), 1 << 16).await.unwrap();
    (s, serde_json::from_slice(&b).unwrap())
}

#[tokio::test]
async fn framework_rejections_are_clean_api_errors() {
    let (s, e) = call(Request::get("/q?limit=abc").body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(e.code, error_codes::BAD_REQUEST);
    assert_eq!(e.field.as_deref(), Some("limit"));
    assert_eq!(e.message, "invalid query parameter limit");

    let (s, e) = call(Request::post("/q").body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(e.code, error_codes::METHOD_NOT_ALLOWED);

    let (s, e) = call(
        Request::post("/j")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"name":"a","port":"1"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(e.code, error_codes::INVALID_TYPE);
    assert_eq!(e.field.as_deref(), Some("port"));

    let (s, e) = call(Request::post("/j").body(Body::from("{}")).unwrap()).await;
    assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(e.code, error_codes::UNSUPPORTED_MEDIA_TYPE);

    let (s, e) = call(
        Request::post("/j")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{"))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(e.code, error_codes::INVALID_JSON);
}
