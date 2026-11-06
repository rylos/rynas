//! HTTP routes a module adds to the internal API.
//!
//! rynasd nests the router of every detected module under `/v1/modules/<id>` (public:
//! `/api/v1/modules/<id>/…`, which rynas-web forwards by prefix without knowing the module).
//! A machine without the module has no such routes: the paths answer 404 like any unknown
//! endpoint. The pieces below are what the handlers of a module share with the core's: the
//! error body ([`rynas_api_types::ApiError`]), the session user rynas-web acts for, the
//! staged/current/applied documents of a configuration key, the blocking pool.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rynas_api_types::{ApiError, error_codes};
use rynas_config::{Config, ConfigError};
use rynas_hw::ControlError;

/// Acknowledges the open hardware notifications as the given user; how many.
pub type AckHardware = Arc<dyn Fn(&str) -> Result<u64, String> + Send + Sync>;

/// What a module's routes work on.
#[derive(Clone)]
pub struct Ctx {
    /// The configuration store (a module's keys are staged in the session user's changeset
    /// and applied with `/v1/config/apply` like every other key).
    pub config: Arc<Config>,
    /// Root of `/sys`, `/proc`, `/run` (tests: a fake tree).
    pub hw_root: PathBuf,
    /// rynasd's state directory.
    pub state_dir: PathBuf,
    /// Acknowledges the open hardware notifications as the given user; how many.
    pub ack_hardware: AckHardware,
}

/// An error answer of a module route: status, code from [`error_codes`], English message.
#[derive(Debug)]
pub struct Error(
    pub StatusCode,
    pub &'static str,
    pub String,
    pub Option<String>,
);

pub type Res<T> = Result<T, Error>;

impl Error {
    pub fn new(status: StatusCode, code: &'static str, msg: impl Into<String>) -> Self {
        Error(status, code, msg.into(), None)
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, msg)
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Error::new(StatusCode::BAD_REQUEST, error_codes::BAD_REQUEST, msg)
    }

    pub fn unavailable(msg: impl Into<String>) -> Self {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            msg,
        )
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

impl From<JsonRejection> for Error {
    fn from(e: JsonRejection) -> Self {
        rynas_http::json_rejection(e).into()
    }
}

impl From<ControlError> for Error {
    fn from(e: ControlError) -> Self {
        match e {
            ControlError::NotFound(m) => Error::not_found(m),
            ControlError::Invalid(m) => {
                Error::new(StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE, m)
            }
            ControlError::Unavailable(m) => Error::unavailable(m),
            ControlError::Busy(m) => Error::new(StatusCode::CONFLICT, error_codes::BUSY, m),
            ControlError::Failed(m) => Error::internal(m),
        }
    }
}

impl From<ConfigError> for Error {
    fn from(e: ConfigError) -> Self {
        if let ConfigError::Invalid(rynas_config::Invalid {
            field: Some(f),
            message,
        }) = e
        {
            return Error(
                StatusCode::BAD_REQUEST,
                error_codes::INVALID_VALUE,
                message,
                Some(f),
            );
        }
        let msg = e.to_string();
        match e {
            ConfigError::UnknownKey(_) | ConfigError::NotFound(_) => Error::not_found(msg),
            ConfigError::Invalid(_) => {
                Error::new(StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE, msg)
            }
            ConfigError::NothingPending => {
                Error::new(StatusCode::CONFLICT, error_codes::NOTHING_PENDING, msg)
            }
            ConfigError::Conflict(_) => {
                Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, msg)
            }
            ConfigError::GroupSplit(_) => {
                Error::new(StatusCode::CONFLICT, error_codes::APPLY_GROUP_SPLIT, msg)
            }
            ConfigError::Apply(_) | ConfigError::Cancelled | ConfigError::Storage(_) => {
                Error::internal(format!("configuration: {msg}"))
            }
        }
    }
}

/// The user rynas-web acts for.
pub fn user(h: &HeaderMap) -> Res<String> {
    let header = rynas_auth::ipc::USER_HEADER;
    h.get(header)
        .and_then(|v| v.to_str().ok())
        .filter(|u| rynas_auth::valid_username(u))
        .map(str::to_string)
        .ok_or_else(|| Error::bad_request(format!("missing or invalid {header} header")))
}

/// `(staged or current, current, applied)` of a configuration key for `owner`.
pub fn docs<T: serde::de::DeserializeOwned + Default + Clone>(
    c: &Config,
    owner: &str,
    key: &str,
) -> Res<(T, T, Option<T>)> {
    let de = |v: serde_json::Value| serde_json::from_value::<T>(v).map_err(Error::internal);
    let current: T = match c.current(key)? {
        Some(v) => de(v)?,
        None => T::default(),
    };
    let applied = c.applied(key)?.map(de).transpose()?;
    let effective = match c.pending_value(owner, key)? {
        Some(Some(v)) => de(v)?,
        Some(None) => T::default(),
        None => current.clone(),
    };
    Ok((effective, current, applied))
}

/// Stages `doc` as the value of `key` in `owner`'s changeset.
pub fn stage<T: serde::Serialize>(c: &Config, owner: &str, key: &str, doc: &T) -> Res<()> {
    let v = serde_json::to_value(doc).map_err(Error::internal)?;
    c.stage(owner, key, Some(v))?;
    Ok(())
}

/// Runs `f` on a blocking thread (files, FIFO, SQLite).
pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Res<T> + Send + 'static) -> Res<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(Error::internal)?
}
