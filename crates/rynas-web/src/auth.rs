//! `/api/v1/auth/*` handlers and the session/CSRF middleware of the whole API.
//!
//! - The session token travels only in the `rynas_session` cookie (`HttpOnly`,
//!   `SameSite=Strict`, `Secure` over HTTPS or with `--secure-cookies`).
//! - Every `/api/v1` route except `auth/login` and `auth/session` needs a valid session; any
//!   method other than GET/HEAD/OPTIONS also needs the session's CSRF token in `X-CSRF-Token`.
//!   Login itself takes JSON only, which a cross-site form cannot send without a CORS
//!   preflight.
//! - Only user-initiated requests renew the idle timeout (like DSM and Proxmox VE: an open
//!   tab alone does not keep a session alive). SSE streams (`/events`, `*/stream`) and
//!   requests marked [`BACKGROUND_HEADER`]`: 1` — the UI's timers, polling and reconnects —
//!   are checked but do not renew it; streams end when their session does.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use rynas_api_types::{
    BACKGROUND_HEADER, CSRF_HEADER, LoginRequest, Role, SESSION_COOKIE, Session, TotpCode,
    TotpConfirmed, TotpEnrollment, error_codes,
};
use rynas_auth::backend::TotpError;
use rynas_auth::service::TotpOpError;
use rynas_auth::session::SessionData;
use rynas_auth::{AuthService, LoginError};

use crate::api::Error;
use crate::app::AppState;

/// Shortest interval between two lockout notifications for the same account.
const LOCKOUT_NOTICE: std::time::Duration = std::time::Duration::from_secs(600);

/// The authenticated session of the current request (set by [`require_session`]).
#[derive(Clone)]
pub struct CurrentSession(pub SessionData);

/// The session token from the `Cookie` header(s).
pub fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == SESSION_COOKIE)
        .map(|(_, v)| v.trim_matches('"'))
        .filter(|v| !v.is_empty())
}

fn client_ip(ci: Option<Extension<ConnectInfo<SocketAddr>>>) -> IpAddr {
    // X-Forwarded-For is not trusted: behind a reverse proxy every client shares its IP
    ci.map(|Extension(ConnectInfo(a))| a.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

fn is_https(st: &AppState, headers: &HeaderMap) -> bool {
    // a spoofed header can only make the cookie stricter
    st.secure_cookies
        || headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("https"))
}

fn cookie(value: &str, max_age_s: u64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_s}{secure}"
    ))
    .expect("cookie made of token characters")
}

fn unauthenticated() -> Error {
    Error::new(
        StatusCode::UNAUTHORIZED,
        error_codes::UNAUTHENTICATED,
        "login required",
    )
}

fn rate_limited(retry: std::time::Duration) -> Response {
    let secs = retry.as_secs() + u64::from(retry.subsec_nanos() > 0);
    let mut r = Error::new(
        StatusCode::TOO_MANY_REQUESTS,
        error_codes::RATE_LIMITED,
        format!("too many failed attempts, retry in {secs} s"),
    )
    .into_response();
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
    r
}

fn bad_json(e: JsonRejection) -> Error {
    Error::from(e)
}

/// Whether a request renews the session's idle timeout: not for event streams (metrics,
/// tasks, events) nor for requests the client marks as background ([`BACKGROUND_HEADER`]).
pub fn renews_idle(path: &str, headers: &HeaderMap) -> bool {
    let stream = path.ends_with("/stream") || path.ends_with("/events");
    let background = headers
        .get(BACKGROUND_HEADER)
        .is_some_and(|v| v.as_bytes() == b"1");
    !(stream || background)
}

/// Middleware: a valid session for everything, the CSRF token for state-changing methods.
pub async fn require_session(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    let touch = renews_idle(req.uri().path(), req.headers());
    let Some(s) = session_token(req.headers()).and_then(|t| st.auth.sessions().get(t, touch))
    else {
        return unauthenticated().into_response();
    };
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        let h = req.headers().get(CSRF_HEADER).and_then(|v| v.to_str().ok());
        if !s.check_csrf(h) {
            return Error::new(
                StatusCode::FORBIDDEN,
                error_codes::CSRF,
                "missing or invalid X-CSRF-Token header",
            )
            .into_response();
        }
    }
    req.extensions_mut().insert(CurrentSession(s));
    next.run(req).await
}

/// Whether `role` may change the system. An exhaustive match: a new role cannot be added
/// without deciding here what it may do.
pub fn is_admin(role: Role) -> bool {
    match role {
        Role::Admin => true,
    }
}

/// Reads that are as sensitive as a change (they hand out key material, or anything of
/// Docker and the app catalogue: container logs and stack files hold secrets, and the
/// engine is root).
fn admin_read(path: &str) -> bool {
    path.ends_with("/luks-header")
        || ["/api/v1/docker/", "/docker/", "/api/v1/apps/", "/apps/"]
            .iter()
            .any(|p| path.starts_with(p))
}

/// Route layer after [`require_session`] on every route except the user's own session
/// (logout, TOTP): any method other than GET/HEAD/OPTIONS — and the few reads of
/// [`admin_read`] — needs the administrator role (403 `forbidden`). Every session is an
/// administrator today; the guard makes adding other roles safe. A request that reaches it
/// without a session is refused too.
pub async fn require_admin(req: Request, next: Next) -> Response {
    let read = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if read && !admin_read(req.uri().path()) {
        return next.run(req).await;
    }
    let role = req
        .extensions()
        .get::<CurrentSession>()
        .map(|s| s.0.user.role);
    if role.is_some_and(is_admin) {
        return next.run(req).await;
    }
    Error::new(
        StatusCode::FORBIDDEN,
        error_codes::FORBIDDEN,
        "administrators only",
    )
    .into_response()
}

/// `/api/v1` fallback: 404 for a logged-in client, 401 otherwise.
pub async fn fallback(State(st): State<AppState>, headers: HeaderMap) -> Error {
    match session_token(&headers).and_then(|t| st.auth.sessions().get(t, false)) {
        Some(_) => Error::not_found("no such API endpoint"),
        None => unauthenticated(),
    }
}

pub async fn login(
    State(st): State<AppState>,
    ci: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return bad_json(e).into_response(),
    };
    let ip = client_ip(ci);
    match st.auth.login(ip, &req).await {
        Ok((token, s)) => {
            // a session cookie from before is replaced: drop that session too
            if let Some(old) = session_token(&headers) {
                st.auth
                    .sessions()
                    .remove(&rynas_auth::session::session_id(old));
            }
            let max_age = st.auth.sessions().config().absolute_timeout.as_secs();
            let mut r = Json(st.auth.sessions().to_api(&s)).into_response();
            r.headers_mut().insert(
                header::SET_COOKIE,
                cookie(&token, max_age, is_https(&st, &headers)),
            );
            r
        }
        Err(LoginError::InvalidCredentials) => Error::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_CREDENTIALS,
            "invalid user name or password",
        )
        .into_response(),
        Err(LoginError::TotpRequired) => Error::new(
            StatusCode::UNAUTHORIZED,
            error_codes::TOTP_REQUIRED,
            "a TOTP code is required",
        )
        .into_response(),
        Err(LoginError::InvalidTotp) => Error::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_TOTP,
            "invalid TOTP or recovery code",
        )
        .into_response(),
        Err(LoginError::RateLimited(d)) => {
            // once per account and lockout window, not for every refused attempt
            st.notifier.emit_once(
                rynas_notify::checks::auth_lockout(&req.username, &ip.to_string(), d.as_secs()),
                d.max(LOCKOUT_NOTICE),
            );
            rate_limited(d)
        }
        Err(LoginError::Unavailable(e)) => {
            tracing::error!("login unavailable: {e}");
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                error_codes::AUTH_UNAVAILABLE,
                "authentication service unavailable",
            )
            .into_response()
        }
    }
}

pub async fn session(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Session>, Error> {
    let touch = renews_idle("/auth/session", &headers);
    let s = session_token(&headers)
        .and_then(|t| st.auth.sessions().get(t, touch))
        .ok_or_else(unauthenticated)?;
    Ok(Json(st.auth.sessions().to_api(&s)))
}

pub async fn logout(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
    headers: HeaderMap,
) -> Response {
    st.auth.logout(&s.id);
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut()
        .insert(header::SET_COOKIE, cookie("", 0, is_https(&st, &headers)));
    r
}

fn totp_error(e: TotpOpError) -> Response {
    let (status, code, msg) = match e {
        TotpOpError::RateLimited(d) => return rate_limited(d),
        TotpOpError::NoBackend => (
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::AUTH_UNAVAILABLE,
            "no authentication backend for this session".to_string(),
        ),
        TotpOpError::Totp(t) => {
            let msg = t.to_string();
            match t {
                TotpError::AlreadyEnabled => {
                    (StatusCode::CONFLICT, error_codes::TOTP_ALREADY_ENABLED, msg)
                }
                TotpError::NotEnrolled => {
                    (StatusCode::CONFLICT, error_codes::TOTP_NOT_ENROLLED, msg)
                }
                TotpError::InvalidCode => (StatusCode::BAD_REQUEST, error_codes::INVALID_TOTP, msg),
                TotpError::InvalidUser => (StatusCode::BAD_REQUEST, error_codes::BAD_REQUEST, msg),
                TotpError::Unavailable(m) => {
                    tracing::error!("TOTP backend: {m}");
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        error_codes::AUTH_UNAVAILABLE,
                        "TOTP storage unavailable".to_string(),
                    )
                }
            }
        }
    };
    Error::new(status, code, msg).into_response()
}

pub async fn totp_enroll(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
) -> Response {
    match st.auth.totp_enroll(&s).await {
        Ok(e) => Json::<TotpEnrollment>(e).into_response(),
        Err(e) => totp_error(e),
    }
}

pub async fn totp_confirm(
    State(st): State<AppState>,
    ci: Option<Extension<ConnectInfo<SocketAddr>>>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
    body: Result<Json<TotpCode>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return bad_json(e).into_response(),
    };
    match st.auth.totp_confirm(client_ip(ci), &s, &req.code).await {
        Ok(recovery_codes) => Json(TotpConfirmed { recovery_codes }).into_response(),
        Err(e) => totp_error(e),
    }
}

pub async fn totp_disable(
    State(st): State<AppState>,
    ci: Option<Extension<ConnectInfo<SocketAddr>>>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
    body: Result<Json<TotpCode>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return bad_json(e).into_response(),
    };
    match st.auth.totp_disable(client_ip(ci), &s, &req.code).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => totp_error(e),
    }
}

/// Periodically drop expired sessions (they are also dropped lazily on access).
pub fn spawn_session_reaper(auth: Arc<AuthService>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            tick.tick().await;
            auth.sessions().purge();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::any;
    use rynas_api_types::UserInfo;
    use tower::ServiceExt;

    async fn status(with_session: bool, method: Method, path: &str) -> StatusCode {
        let app = Router::new()
            .route("/{*p}", any(|| async { StatusCode::OK }))
            .route_layer(axum::middleware::from_fn(require_admin));
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        if with_session {
            req.extensions_mut().insert(CurrentSession(SessionData {
                id: [0; 32],
                user: UserInfo {
                    username: "alice".into(),
                    realm: "pam".into(),
                    role: Role::Admin,
                },
                csrf_token: String::new(),
                totp_enabled: false,
                created_ms: 0,
                last_seen_ms: 0,
            }));
        }
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn changes_need_an_administrator() {
        for m in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            assert_eq!(status(true, m.clone(), "/users").await, StatusCode::OK);
            assert_eq!(status(false, m, "/users").await, StatusCode::FORBIDDEN);
        }
        // plain reads pass the guard (the session middleware already ran)
        assert_eq!(status(false, Method::GET, "/users").await, StatusCode::OK);
        assert_eq!(status(false, Method::HEAD, "/system").await, StatusCode::OK);
        // key material is read by administrators only
        let header = "/storage/volumes/vol1/luks-header";
        assert_eq!(
            status(false, Method::GET, header).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(status(true, Method::GET, header).await, StatusCode::OK);
        assert!(is_admin(Role::Admin));
    }
}
