//! Test harness: the full router with a fake password backend and in-memory TOTP.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use rynas_api_types::{LoginRequest, MetricsSample, Session};
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::ratelimit::RateLimitConfig;
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_auth::{AuthConfig, AuthService, Backend, ManualClock};
use rynas_web::app::{AppState, router};
use rynas_web::hardware::HardwareProvider;
use rynas_web::metrics::Store;
use rynas_web::rynasd_client::RynasdClient;
use tower::ServiceExt;

pub const T0_MS: u64 = 1_800_000_000_000;

/// Accepts `admin`/`admin-pw` (group rynas-admin) and `guest`/`guest-pw` (no admin group).
/// Like PAM, it cannot tell an unknown user from a wrong password.
pub struct FakeAuth;

impl Authenticator for FakeAuth {
    fn realm(&self) -> &str {
        "pam"
    }
    fn authenticate<'a>(
        &'a self,
        username: &'a str,
        password: &'a str,
    ) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async move {
            let users: HashMap<&str, (&str, &str)> = [
                ("admin", ("admin-pw", "rynas-admin")),
                ("guest", ("guest-pw", "users")),
            ]
            .into();
            match users.get(username) {
                Some((pw, group)) if *pw == password => Ok(Identity {
                    username: username.into(),
                    groups: vec![group.to_string()],
                }),
                _ => Err(AuthError::InvalidCredentials),
            }
        })
    }
}

/// A backend that is never reachable (rynasd down).
pub struct DownAuth;

impl Authenticator for DownAuth {
    fn realm(&self) -> &str {
        "pam"
    }
    fn authenticate<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Err(AuthError::Unavailable("connection refused".into())) })
    }
}

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub fn ui_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("rynas-web-test-ui-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("index.html"),
        "<!doctype html><script>window.t=1</script><p>shell</p>",
    )
    .unwrap();
    std::fs::write(d.join("app-0123456789abcdef.js"), "console.log(1)").unwrap();
    d
}

pub struct TestApp {
    pub router: axum::Router,
    pub store: Arc<Store>,
    pub clock: Arc<ManualClock>,
    pub auth: Arc<AuthService>,
    pub streams: Arc<rynas_web::streams::StreamLimits>,
}

pub fn build(hw: Arc<dyn HardwareProvider>, authenticator: Arc<dyn Authenticator>) -> TestApp {
    // nothing listens there: forwarded endpoints answer 503
    let socket =
        std::env::temp_dir().join(format!("rynas-web-test-{}-none.sock", std::process::id()));
    build_with_rynasd(hw, authenticator, socket)
}

/// Like [`build`], with the rynasd endpoints going to the socket at `socket`.
pub fn build_with_rynasd(
    hw: Arc<dyn HardwareProvider>,
    authenticator: Arc<dyn Authenticator>,
    socket: PathBuf,
) -> TestApp {
    let store = Store::new();
    for i in 0..1200u64 {
        store.push(MetricsSample {
            ts_ms: i * 1000,
            cpu_pct: 10.0,
            ..Default::default()
        });
    }
    let clock = Arc::new(ManualClock::new(T0_MS));
    let totp = Arc::new(TotpManager::new(
        MemoryStorage::default(),
        clock.clone(),
        "rynas",
    ));
    let cfg = AuthConfig {
        rate: RateLimitConfig {
            free_per_user: 3,
            free_per_ip: 10,
            base_delay: Duration::from_secs(4),
            log_failures: false,
            ..Default::default()
        },
        failure_floor: Duration::ZERO,
        host: "testnas".into(),
        ..Default::default()
    };
    let auth = Arc::new(AuthService::with_clock(
        vec![Backend {
            authenticator,
            totp,
        }],
        cfg,
        clock.clone(),
    ));
    let state = AppState {
        store: store.clone(),
        hw,
        auth: auth.clone(),
        rynasd: Arc::new(RynasdClient::new(socket.clone())),
        mounts: rynas_web::storage::MountProber::new(),
        secure_cookies: false,
        notifier: rynas_web::notify::WebNotifier::new(Arc::new(RynasdClient::new(socket))),
        streams: Default::default(),
    };
    let streams = state.streams.clone();
    TestApp {
        streams,
        router: router(state, ui_dir()),
        store,
        clock,
        auth,
    }
}

pub fn app(hw: Arc<dyn HardwareProvider>) -> TestApp {
    build(hw, Arc::new(FakeAuth))
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&self.body)))
    }
    pub fn code(&self) -> String {
        self.json::<rynas_api_types::ApiError>().code
    }
}

/// Credentials of a logged-in client.
#[derive(Clone, Debug, Default)]
pub struct Creds {
    pub cookie: Option<String>,
    pub csrf: Option<String>,
}

impl TestApp {
    pub async fn send(
        &self,
        method: Method,
        uri: &str,
        creds: &Creds,
        body: Option<serde_json::Value>,
    ) -> Resp {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(c) = &creds.cookie {
            b = b.header(header::COOKIE, format!("theme=dark; rynas_session={c}"));
        }
        if let Some(t) = &creds.csrf {
            b = b.header("x-csrf-token", t);
        }
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string())),
            None => b.body(Body::empty()),
        }
        .unwrap();
        let r = self.router.clone().oneshot(req).await.unwrap();
        let (parts, body) = r.into_parts();
        Resp {
            status: parts.status,
            headers: parts.headers,
            body: to_bytes(body, 1 << 24).await.unwrap().to_vec(),
        }
    }

    pub async fn get(&self, uri: &str, creds: &Creds) -> Resp {
        self.send(Method::GET, uri, creds, None).await
    }

    pub async fn try_login(&self, req: &LoginRequest) -> Resp {
        self.send(
            Method::POST,
            "/api/v1/auth/login",
            &Creds::default(),
            Some(serde_json::to_value(req).unwrap()),
        )
        .await
    }

    pub async fn login_as(&self, username: &str, password: &str, totp: Option<&str>) -> Creds {
        let r = self
            .try_login(&LoginRequest {
                username: username.into(),
                password: password.into(),
                totp: totp.map(Into::into),
            })
            .await;
        assert_eq!(
            r.status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&r.body)
        );
        let session: Session = r.json();
        Creds {
            cookie: Some(cookie_value(&r.headers).expect("session cookie")),
            csrf: Some(session.csrf_token),
        }
    }

    pub async fn login(&self) -> Creds {
        self.login_as("admin", "admin-pw", None).await
    }
}

/// Value of the `rynas_session` cookie set by a response.
pub fn cookie_value(h: &HeaderMap) -> Option<String> {
    let v = h.get(header::SET_COOKIE)?.to_str().ok()?;
    let (kv, _) = v.split_once(';')?;
    kv.strip_prefix("rynas_session=").map(Into::into)
}
