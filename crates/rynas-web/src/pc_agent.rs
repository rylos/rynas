//! The PC agents' routes (`/api/v1/pcbackup/agent/…` and the restic REST protocol under
//! `/api/v1/pcbackup/repo/<group>/…`, D-PCBACKUP): no session and no CSRF; rynasd checks the
//! pairing code, device token or restore session itself. Both ways stream (a repository
//! upload goes on to rynasd as it arrives; a download comes back as rynasd reads it).
//!
//! Only the headers the protocol needs pass, and the client's address for rynasd's log; never
//! a cookie or a session user. The console's routes (`/api/v1/pcbackup/…` for
//! administrators) are in [`crate::proxy`].

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Extension, Router};
use http_body_util::{BodyExt, Limited};
use rynas_api_types::error_codes;
use rynas_auth::ipc::CLIENT_ADDR_HEADER;

use crate::app::AppState;
use crate::conn::Client;
use crate::error::Error;

/// Largest repository file an agent uploads (rynasd allows 1 GiB too).
pub const MAX_REPO_BODY: usize = 1 << 30;
/// Largest JSON body of the agent's calls (a report with its manifest).
pub const MAX_AGENT_BODY: usize = 2 << 20;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/pcbackup/agent/pair", post(forward))
        .route("/pcbackup/agent/plan", get(forward))
        .route("/pcbackup/agent/status", post(forward))
        .route("/pcbackup/agent/report", post(forward))
        .route("/pcbackup/agent/restore/login", post(forward))
        .route("/pcbackup/agent/restore/report", post(forward))
        .route("/pcbackup/repo/{group}", post(forward))
        .route("/pcbackup/repo/{group}/", post(forward))
        .route("/pcbackup/repo/{group}/{*rest}", any(forward))
}

const REQUEST_HEADERS: [HeaderName; 5] = [
    header::AUTHORIZATION,
    header::CONTENT_TYPE,
    header::CONTENT_LENGTH,
    header::RANGE,
    header::ACCEPT,
];

const ANSWER_HEADERS: [HeaderName; 5] = [
    header::CONTENT_TYPE,
    header::CONTENT_LENGTH,
    header::CONTENT_RANGE,
    header::ACCEPT_RANGES,
    header::RETRY_AFTER,
];

async fn forward(State(st): State<AppState>, client: Option<Extension<Client>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let repo = parts.uri.path().starts_with("/pcbackup/repo/") || parts.uri.path().contains("/pcbackup/repo/");
    let max = if repo { MAX_REPO_BODY } else { MAX_AGENT_BODY };
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > max as u64) {
        return Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            error_codes::PAYLOAD_TOO_LARGE,
            format!("request body over {max} bytes"),
        )
        .into_response();
    }
    let target = match parts.uri.query() {
        Some(q) => format!("/v1{}?{q}", parts.uri.path()),
        None => format!("/v1{}", parts.uri.path()),
    };
    let mut headers: Vec<(HeaderName, HeaderValue)> = REQUEST_HEADERS
        .iter()
        .filter_map(|n| parts.headers.get(n).map(|v| (n.clone(), v.clone())))
        .collect();
    if let Some(Extension(c)) = client
        && let Ok(v) = HeaderValue::from_str(&c.ip.to_string())
    {
        headers.push((HeaderName::from_static(CLIENT_ADDR_HEADER), v));
    }
    let body = Body::new(Limited::new(body, max).map_err(axum::Error::new));
    let resp = match st
        .rynasd
        .send_stream_anonymous(parts.method, &target, &headers, body)
        .await
    {
        Ok(r) => r,
        Err(e) => return Error::from(e).into_response(),
    };
    let (rparts, incoming) = resp.into_parts();
    let mut out = Response::new(Body::new(incoming));
    *out.status_mut() = rparts.status;
    for name in ANSWER_HEADERS {
        if let Some(v) = rparts.headers.get(&name) {
            out.headers_mut().insert(name, v.clone());
        }
    }
    out
}
