//! The one client of the root daemon `rynasd` (HTTP/1.1 over its Unix socket, one
//! connection per call: logins, configuration changes and storage views are rare).
//!
//! Every call from rynas-web to rynasd goes through [`RynasdClient::send`]: one connection
//! setup with the `SO_PEERCRED` check, one table of deadlines ([`deadline`]), the session
//! user in `x-rynas-user` whenever there is one (guidelines A6), and one mapping of failures
//! to API errors ([`UpstreamError`]: unreachable → 503 `backend_unavailable`, no answer in
//! time → 504 `timeout`, garbage → 502 `bad_gateway`, rynasd's own `ApiError` as it is).
//! On top of it:
//! - [`RynasdClient::call`]: typed request/answer with a capped body, for what rynas-web
//!   must read (authentication, TOTP, storage disks enriched with bay numbers);
//! - [`RynasdClient::send`] itself for the pass-through proxy ([`crate::proxy`]), whose
//!   answers (JSON, task logs, SSE) stream back unchanged.
//!
//! The server end must run as root or as our own uid, so a process that managed to put a
//! socket in place cannot collect passwords.

use std::path::PathBuf;
use std::time::Duration;

use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use rynas_api_types::{ApiError, TotpCode, TotpConfirmed, TotpEnrollment, error_codes};
use rynas_auth::TotpStatus;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity, TotpBackend, TotpError};
use rynas_auth::ipc::{AuthenticateRequest, EnrollRequest, USER_HEADER};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;

use crate::error::Error;

/// PAM may sleep a few seconds on failure.
pub const AUTH_DEADLINE: Duration = Duration::from_secs(15);
/// A first storage view may read SMART from many disks (smartctl has 30 s per disk, in
/// parallel); rynasd serves cached inventories after that.
pub const STORAGE_DEADLINE: Duration = Duration::from_secs(90);
/// Everything else: configuration, tasks, users and shares (long work runs as a task).
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);
/// A test message to a notification target (SMTP may take 40 s to fail).
pub const NOTIFY_TEST_DEADLINE: Duration = Duration::from_secs(60);
/// A network diagnostic (traceroute runs up to 45 s in rynasd).
pub const DIAGNOSTICS_DEADLINE: Duration = Duration::from_secs(60);
/// The first read of the update list after apt's state changed runs two apt simulations;
/// the kernel card may run `synodeb-dom status` (mounts the DOM).
pub const UPDATES_DEADLINE: Duration = Duration::from_secs(90);
/// Largest answer read into memory by [`RynasdClient::call`] (pass-through answers stream).
pub const MAX_ANSWER: usize = 8 << 20;

/// Deadline of a call to `path` (rynasd's `/v1/...`): until the whole answer for typed calls,
/// until the response head for pass-through ones (a task log or an SSE stream may last).
pub fn deadline(path: &str) -> Duration {
    if path.starts_with("/v1/authenticate") || path.starts_with("/v1/totp/") {
        AUTH_DEADLINE
    } else if path.starts_with("/v1/storage/") {
        STORAGE_DEADLINE
    } else if path.starts_with("/v1/notifications/targets/") && path.ends_with("/test") {
        NOTIFY_TEST_DEADLINE
    } else if path.starts_with("/v1/network/diagnostics/") {
        DIAGNOSTICS_DEADLINE
    } else if path.starts_with("/v1/updates") {
        UPDATES_DEADLINE
    } else {
        DEFAULT_DEADLINE
    }
}

/// Why a call to rynasd failed.
#[derive(Debug)]
pub enum UpstreamError {
    /// Could not connect or the connection broke (503 `backend_unavailable`).
    Unavailable(String),
    /// No answer within the deadline (504 `timeout`).
    Timeout,
    /// An answer rynas-web cannot read (502 `bad_gateway`).
    BadGateway(String),
    /// rynasd answered with an error of its own.
    Api(StatusCode, ApiError),
}

impl From<UpstreamError> for Error {
    fn from(e: UpstreamError) -> Self {
        match e {
            UpstreamError::Unavailable(m) => {
                tracing::warn!("rynasd: {m}");
                Error::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    error_codes::BACKEND_UNAVAILABLE,
                    "the rynas daemon (rynasd) is not reachable",
                )
            }
            UpstreamError::Timeout => Error::new(
                StatusCode::GATEWAY_TIMEOUT,
                error_codes::TIMEOUT,
                "the rynas daemon (rynasd) did not answer in time",
            ),
            UpstreamError::BadGateway(m) => {
                tracing::error!("rynasd: unexpected answer: {m}");
                Error::new(
                    StatusCode::BAD_GATEWAY,
                    error_codes::BAD_GATEWAY,
                    "unexpected answer from the rynas daemon (rynasd)",
                )
            }
            UpstreamError::Api(s, a) => Error(s, a),
        }
    }
}

pub struct RynasdClient {
    socket: PathBuf,
    /// Upper bound on every deadline (tests).
    cap: Option<Duration>,
}

impl RynasdClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            cap: None,
        }
    }

    /// Never wait longer than `max`, whatever the route (tests of the timeout path).
    pub fn with_deadline_cap(mut self, max: Duration) -> Self {
        self.cap = Some(max);
        self
    }

    fn deadline(&self, path: &str) -> Duration {
        let d = deadline(path);
        self.cap.map_or(d, |c| d.min(c))
    }

    /// Opens a connection, refusing a server that is neither root nor our own uid.
    async fn connect(&self) -> Result<SendRequest<Full<Bytes>>, String> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| e.to_string())?;
        let peer = stream.peer_cred().map_err(|e| e.to_string())?;
        let me = current_uid();
        if peer.uid() != 0 && peer.uid() != me {
            return Err(format!("socket owned by uid {}, refusing", peer.uid()));
        }
        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(sender)
    }

    /// Sends a request to rynasd as `user` and returns its answer with the body still
    /// streaming. The deadline ([`deadline`]) covers the connection and the response head.
    pub async fn send(
        &self,
        method: Method,
        path_and_query: &str,
        user: Option<&str>,
        content_type: Option<HeaderValue>,
        body: Bytes,
    ) -> Result<hyper::Response<Incoming>, UpstreamError> {
        self.send_within(
            self.deadline(path_and_query),
            method,
            path_and_query,
            user,
            content_type,
            body,
            &[],
        )
        .await
    }

    /// [`RynasdClient::send`] with extra internal headers (the session's local address for
    /// the network endpoints).
    pub async fn send_with_headers(
        &self,
        method: Method,
        path_and_query: &str,
        user: Option<&str>,
        content_type: Option<HeaderValue>,
        body: Bytes,
        extra: &[(&'static str, String)],
    ) -> Result<hyper::Response<Incoming>, UpstreamError> {
        self.send_within(
            self.deadline(path_and_query),
            method,
            path_and_query,
            user,
            content_type,
            body,
            extra,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_within(
        &self,
        limit: Duration,
        method: Method,
        path_and_query: &str,
        user: Option<&str>,
        content_type: Option<HeaderValue>,
        body: Bytes,
        extra: &[(&'static str, String)],
    ) -> Result<hyper::Response<Incoming>, UpstreamError> {
        let fut = async {
            let mut sender = self.connect().await.map_err(UpstreamError::Unavailable)?;
            let mut req = Request::builder()
                .method(method)
                .uri(path_and_query)
                .header(header::HOST, "rynasd");
            if let Some(u) = user {
                req = req.header(USER_HEADER, u);
            }
            if let Some(ct) = content_type {
                req = req.header(header::CONTENT_TYPE, ct);
            }
            for (k, v) in extra {
                req = req.header(*k, v.as_str());
            }
            let req = req
                .body(Full::new(body))
                .map_err(|e| UpstreamError::BadGateway(e.to_string()))?;
            sender
                .send_request(req)
                .await
                .map_err(|e| UpstreamError::Unavailable(e.to_string()))
        };
        tokio::time::timeout(limit, fut)
            .await
            .unwrap_or(Err(UpstreamError::Timeout))
    }

    /// A typed call: JSON body (if any) in, JSON answer out, read with a cap of
    /// [`MAX_ANSWER`] and within the route's deadline. An empty 2xx answer decodes as `null`
    /// (so `T = ()` fits 204).
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        user: Option<&str>,
        body: Option<&impl Serialize>,
    ) -> Result<T, UpstreamError> {
        let limit = self.deadline(path);
        let fut = async {
            let bytes = match body {
                Some(b) => {
                    serde_json::to_vec(b).map_err(|e| UpstreamError::BadGateway(e.to_string()))?
                }
                None => Vec::new(),
            };
            let resp = self
                .send_within(
                    limit,
                    method,
                    path,
                    user,
                    Some(HeaderValue::from_static("application/json")),
                    Bytes::from(bytes),
                    &[],
                )
                .await?;
            let status = resp.status();
            let body = Limited::new(resp.into_body(), MAX_ANSWER)
                .collect()
                .await
                .map_err(|e| UpstreamError::BadGateway(format!("reading the answer: {e}")))?
                .to_bytes();
            if status.is_success() {
                let body: &[u8] = if body.is_empty() { b"null" } else { &body };
                serde_json::from_slice(body).map_err(|e| UpstreamError::BadGateway(e.to_string()))
            } else {
                match serde_json::from_slice::<ApiError>(&body) {
                    Ok(err) => Err(UpstreamError::Api(status, err)),
                    Err(_) => Err(UpstreamError::BadGateway(format!(
                        "{status}: {}",
                        String::from_utf8_lossy(&body[..body.len().min(200)])
                    ))),
                }
            }
        };
        tokio::time::timeout(limit, fut)
            .await
            .unwrap_or(Err(UpstreamError::Timeout))
    }
}

fn current_uid() -> u32 {
    // SAFETY: getuid cannot fail and has no preconditions.
    unsafe { libc::getuid() }
}

fn totp_err(e: UpstreamError) -> TotpError {
    match e {
        UpstreamError::Api(_, a) => match a.code.as_str() {
            error_codes::TOTP_ALREADY_ENABLED => TotpError::AlreadyEnabled,
            error_codes::TOTP_NOT_ENROLLED => TotpError::NotEnrolled,
            error_codes::INVALID_TOTP => TotpError::InvalidCode,
            error_codes::BAD_REQUEST => TotpError::InvalidUser,
            _ => TotpError::Unavailable(format!("rynasd: {}", a.message)),
        },
        UpstreamError::Timeout => TotpError::Unavailable("rynasd: timeout".into()),
        UpstreamError::Unavailable(m) | UpstreamError::BadGateway(m) => {
            TotpError::Unavailable(format!("rynasd: {m}"))
        }
    }
}

fn user_path(user: &str, tail: &str) -> Result<String, TotpError> {
    // the name goes into a URL path: only accept the safe charset
    if !rynas_auth::valid_username(user) {
        return Err(TotpError::InvalidUser);
    }
    Ok(format!("/v1/totp/{user}{tail}"))
}

const NONE: Option<&()> = None;

impl Authenticator for RynasdClient {
    fn realm(&self) -> &str {
        "pam"
    }

    fn authenticate<'a>(
        &'a self,
        username: &'a str,
        password: &'a str,
    ) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async move {
            let req = AuthenticateRequest {
                username: username.into(),
                password: password.into(),
            };
            match self
                .call(Method::POST, "/v1/authenticate", None, Some(&req))
                .await
            {
                Ok(id) => Ok(id),
                Err(UpstreamError::Api(StatusCode::UNAUTHORIZED, _)) => {
                    Err(AuthError::InvalidCredentials)
                }
                // rynasd's own throttling: treat as a failure, the web limiter reports it
                Err(UpstreamError::Api(StatusCode::TOO_MANY_REQUESTS, _)) => {
                    Err(AuthError::InvalidCredentials)
                }
                Err(UpstreamError::Api(s, e)) => {
                    Err(AuthError::Unavailable(format!("rynasd: {s} {}", e.message)))
                }
                Err(UpstreamError::Timeout) => {
                    Err(AuthError::Unavailable("rynasd: timeout".into()))
                }
                Err(UpstreamError::Unavailable(m) | UpstreamError::BadGateway(m)) => {
                    Err(AuthError::Unavailable(format!("rynasd: {m}")))
                }
            }
        })
    }
}

impl TotpBackend for RynasdClient {
    fn status<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<TotpStatus, TotpError>> {
        Box::pin(async move {
            self.call(Method::GET, &user_path(user, "")?, Some(user), NONE)
                .await
                .map_err(totp_err)
        })
    }

    fn enroll<'a>(
        &'a self,
        user: &'a str,
        account: &'a str,
    ) -> BoxFuture<'a, Result<TotpEnrollment, TotpError>> {
        Box::pin(async move {
            let body = EnrollRequest {
                account: account.into(),
            };
            self.call(
                Method::POST,
                &user_path(user, "/enroll")?,
                Some(user),
                Some(&body),
            )
            .await
            .map_err(totp_err)
        })
    }

    fn confirm<'a>(
        &'a self,
        user: &'a str,
        code: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, TotpError>> {
        Box::pin(async move {
            let body = TotpCode { code: code.into() };
            self.call::<TotpConfirmed>(
                Method::POST,
                &user_path(user, "/confirm")?,
                Some(user),
                Some(&body),
            )
            .await
            .map(|c| c.recovery_codes)
            .map_err(totp_err)
        })
    }

    fn disable<'a>(&'a self, user: &'a str, code: &'a str) -> BoxFuture<'a, Result<(), TotpError>> {
        Box::pin(async move {
            let body = TotpCode { code: code.into() };
            self.call::<()>(
                Method::POST,
                &user_path(user, "/disable")?,
                Some(user),
                Some(&body),
            )
            .await
            .map_err(totp_err)
        })
    }

    fn verify<'a>(&'a self, user: &'a str, code: &'a str) -> BoxFuture<'a, Result<(), TotpError>> {
        Box::pin(async move {
            let body = TotpCode { code: code.into() };
            self.call::<()>(
                Method::POST,
                &user_path(user, "/verify")?,
                Some(user),
                Some(&body),
            )
            .await
            .map_err(totp_err)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadlines_per_route() {
        assert_eq!(deadline("/v1/authenticate"), AUTH_DEADLINE);
        assert_eq!(deadline("/v1/totp/alice/verify"), AUTH_DEADLINE);
        assert_eq!(deadline("/v1/storage/disks/sda"), STORAGE_DEADLINE);
        assert_eq!(deadline("/v1/tasks?limit=5"), DEFAULT_DEADLINE);
    }

    #[tokio::test]
    async fn unreachable_is_backend_unavailable() {
        let c = RynasdClient::new("/nonexistent/rynasd.sock");
        let e = c
            .call::<()>(Method::GET, "/v1/health", None, NONE)
            .await
            .unwrap_err();
        let Error(status, api) = Error::from(e);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(api.code, error_codes::BACKEND_UNAVAILABLE);
    }

    #[tokio::test]
    async fn no_answer_in_time_is_timeout() {
        let dir = std::env::temp_dir().join(format!("rynas-web-slow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let slow = axum::Router::new().route(
            "/v1/tasks",
            axum::routing::get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "[]"
            }),
        );
        tokio::spawn(async move { axum::serve(listener, slow).await });
        let c = RynasdClient::new(&socket).with_deadline_cap(Duration::from_millis(200));
        let e = c
            .call::<()>(Method::GET, "/v1/tasks", Some("alice"), NONE)
            .await
            .unwrap_err();
        let Error(status, api) = Error::from(e);
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(api.code, error_codes::TIMEOUT);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
