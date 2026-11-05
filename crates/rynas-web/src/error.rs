//! The error answer of the API. Rejected JSON bodies and every other non-2xx answer under
//! `/api` (axum rejections: query string, path, 405, 413...) become [`ApiError`] bodies
//! through [`rynas_http`], shared with rynasd (guidelines A8, A10, A11, A13). Codes come
//! from [`error_codes`] only.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rynas_api_types::{ApiError, error_codes};

/// An error response with an [`ApiError`] body.
#[derive(Debug)]
pub struct Error(pub StatusCode, pub ApiError);

impl Error {
    /// `code` must be one of [`error_codes`].
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self(status, ApiError::new(code, message))
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, error_codes::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, message)
    }

    /// 500 `internal`, also logged.
    pub fn internal(message: impl Into<String>) -> Self {
        let message = message.into();
        tracing::error!("internal error: {message}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            error_codes::INTERNAL,
            message,
        )
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl From<JsonRejection> for Error {
    fn from(e: JsonRejection) -> Self {
        rynas_http::json_rejection(e).into()
    }
}

impl From<rynas_http::Rejected> for Error {
    fn from((status, e): rynas_http::Rejected) -> Self {
        Self(status, e)
    }
}

/// Middleware for `/api`: framework rejections (query string, path, 405, 413...) as
/// [`ApiError`] bodies with generic messages, the same as rynasd's.
pub use rynas_http::json_errors;
