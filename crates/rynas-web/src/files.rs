//! The file manager: `/api/v1/files/...` passed on to rynasd (`/v1/files/...`), streaming
//! both ways — an upload chunk goes on to rynasd and its worker as it arrives, a download
//! comes back as the worker writes it; neither sits whole in memory.
//!
//! Open to every signed-in user (not only administrators): rynasd runs each request as the
//! session's Linux user, so the kernel decides what that user may read and change. Session
//! and CSRF checks have already run ([`crate::auth::require_session`]).

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Extension, Router};
use http_body_util::{BodyExt, Limited};
use rynas_api_types::error_codes;

use crate::app::AppState;
use crate::auth::CurrentSession;
use crate::error::Error;

/// Largest request body: an upload chunk (the interface sends 16 MiB) with room to spare
/// (guidelines M3: documented in `docs/plugins/files.md`).
pub const MAX_UPLOAD: usize = 80 << 20;

pub fn routes() -> Router<AppState> {
    Router::new().route("/files/{*route}", any(forward))
}

/// Request headers that reach rynasd.
const REQUEST_HEADERS: [HeaderName; 3] = [header::CONTENT_TYPE, header::CONTENT_LENGTH, header::RANGE];

/// Headers of the answer that reach the client: what downloads, previews with ranges and
/// thumbnails need.
fn answer_headers() -> [HeaderName; 8] {
    [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_DISPOSITION,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::CONTENT_SECURITY_POLICY,
        header::CACHE_CONTROL,
        header::LAST_MODIFIED,
    ]
}

async fn forward(State(st): State<AppState>, Extension(CurrentSession(session)): Extension<CurrentSession>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let declared = parts.headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_UPLOAD as u64) {
        return Error::new(StatusCode::PAYLOAD_TOO_LARGE, error_codes::PAYLOAD_TOO_LARGE, format!("request body over {MAX_UPLOAD} bytes")).into_response();
    }
    let target = match parts.uri.query() {
        Some(q) => format!("/v1{}?{q}", parts.uri.path()),
        None => format!("/v1{}", parts.uri.path()),
    };
    let headers: Vec<_> = REQUEST_HEADERS.iter().filter_map(|n| parts.headers.get(n).map(|v| (n.clone(), v.clone()))).collect();
    // a body over the limit ends the stream with an error: rynasd's worker sees it cut
    let body = Body::new(Limited::new(body, MAX_UPLOAD).map_err(axum::Error::new));
    let resp = match st.rynasd.send_stream(parts.method, &target, &session.user.username, &headers, body).await {
        Ok(r) => r,
        Err(e) => return Error::from(e).into_response(),
    };
    let (rparts, incoming) = resp.into_parts();
    let mut out = Response::new(Body::new(incoming));
    *out.status_mut() = rparts.status;
    for name in answer_headers() {
        if let Some(v) = rparts.headers.get(&name) {
            out.headers_mut().insert(name, v.clone());
        }
    }
    out
}

/// Compression predicate: never the file manager's file contents — downloads, previews
/// with byte ranges, archives, media (they are compressed already or must keep their
/// length for `Range`). JSON answers of the file manager are still compressed.
#[derive(Clone, Copy)]
pub struct NotFileContent;

impl tower_http::compression::Predicate for NotFileContent {
    fn should_compress<B>(&self, response: &axum::http::Response<B>) -> bool
    where
        B: axum::body::HttpBody,
    {
        let h = response.headers();
        !(h.contains_key(header::CONTENT_DISPOSITION) || h.contains_key(header::ACCEPT_RANGES) || h.contains_key(header::CONTENT_RANGE))
    }
}
