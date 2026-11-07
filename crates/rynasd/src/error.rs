//! The error type of the internal API. Rejected JSON bodies and every other non-2xx answer
//! (axum rejections: query string, path, 405, 413...) become [`ApiError`] bodies through
//! [`rynas_http`], shared with rynas-web, so that rynas-web can pass rynasd's errors on as
//! they are (guidelines A8, A10, A11, A13).

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rynas_api_types::{ApiError, error_codes};

/// An error answer: status, code from [`error_codes`], English message, and the form
/// field it is about when known ([`ApiError::field`], guidelines A10).
#[derive(Debug)]
pub struct Error(
    pub(crate) StatusCode,
    pub(crate) &'static str,
    pub(crate) String,
    pub(crate) Option<String>,
);

impl Error {
    pub fn new(status: StatusCode, code: &'static str, msg: String) -> Self {
        Error(status, code, msg, None)
    }

    /// 400 `invalid_value` about the request field `field` (its JSON name).
    pub fn invalid(field: impl Into<String>, msg: impl Into<String>) -> Self {
        Error::new(
            StatusCode::BAD_REQUEST,
            error_codes::INVALID_VALUE,
            msg.into(),
        )
        .field(field)
    }

    /// The error code (one of [`error_codes`]).
    pub fn code(&self) -> &'static str {
        self.1
    }

    /// Names the request field the error is about.
    pub fn field(mut self, field: impl Into<String>) -> Self {
        self.3 = Some(field.into());
        self
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Error::new(
            StatusCode::BAD_REQUEST,
            error_codes::BAD_REQUEST,
            msg.into(),
        )
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, msg.into())
    }

    /// 500 `internal`, also logged.
    pub fn internal(e: impl std::fmt::Display) -> Self {
        let msg = e.to_string();
        tracing::error!("internal error: {msg}");
        Error::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            error_codes::INTERNAL,
            msg,
        )
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(ApiError {
                code: self.1.into(),
                message: self.2,
                field: self.3,
            }),
        )
            .into_response()
    }
}

impl From<JsonRejection> for Error {
    fn from(e: JsonRejection) -> Self {
        rynas_http::json_rejection(e).into()
    }
}

impl From<rynas_http::Rejected> for Error {
    fn from((status, e): rynas_http::Rejected) -> Self {
        let code = error_codes::ALL
            .iter()
            .copied()
            .find(|c| *c == e.code)
            .unwrap_or(error_codes::BAD_REQUEST);
        Error(status, code, e.message, e.field)
    }
}

/// `201 Created` with `body` and a `Location` header naming the new object under the
/// public API (guidelines A6 table), e.g. `created("/api/v1/users", &name, v)`. The id is
/// percent-encoded as a path segment.
pub fn created<T: serde::Serialize>(collection: &str, id: &str, body: T) -> Response {
    let mut r = (StatusCode::CREATED, Json(body)).into_response();
    let location = format!("{collection}/{}", path_segment(id));
    if let Ok(v) = HeaderValue::from_str(&location) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r
}

/// RFC 3986 percent-encoding of everything but the unreserved characters.
fn path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Middleware: framework rejections (query string, path, 405, 413...) as [`ApiError`]
/// bodies with generic messages, the same as rynas-web's ([`rynas_http::json_errors`]).
pub use rynas_http::json_errors;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_sets_location() {
        let r = created("/api/v1/users", "alice", ());
        assert_eq!(r.status(), StatusCode::CREATED);
        assert_eq!(r.headers()[header::LOCATION], "/api/v1/users/alice");
        let r = created("/api/v1/shares", "a b/ü", ());
        assert_eq!(
            r.headers()[header::LOCATION],
            "/api/v1/shares/a%20b%2F%C3%BC"
        );
    }
}
