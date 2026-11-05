//! Application state and the full router.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::http::{HeaderValue, header};
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{DefaultPredicate, Predicate, SizeAbove};
use tower_http::set_header::SetResponseHeaderLayer;

use rynas_auth::AuthService;

use crate::hardware::HardwareProvider;
use crate::metrics::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub hw: Arc<dyn HardwareProvider>,
    pub auth: Arc<AuthService>,
    /// The root daemon (authentication, configuration, tasks, storage, users and shares):
    /// every call to it goes through this client.
    pub rynasd: Arc<crate::rynasd_client::RynasdClient>,
    /// Filesystem sizes for the storage overview (statvfs off the shared pool).
    pub mounts: Arc<crate::storage::MountProber>,
    /// Always mark the session cookie `Secure` (also set per request over HTTPS).
    pub secure_cookies: bool,
    /// Events for the notification engine in rynasd (logins, hardware).
    pub notifier: Arc<crate::notify::WebNotifier>,
    /// Open event streams per session and in total (guidelines M1).
    pub streams: Arc<crate::streams::StreamLimits>,
    /// UPS status from upsd (`ups.status` events, `GET /ups`).
    pub ups: Arc<crate::ups::UpsMonitor>,
}

pub fn router(state: AppState, ui_dir: PathBuf) -> Router {
    let api = crate::api::router(state.clone());
    // outside /api/v1 too: 401 without a session, so /api/v2/x reveals nothing (A23)
    let api_rest = axum::Router::new()
        .route("/api", axum::routing::any(crate::auth::fallback))
        .route("/api/{*rest}", axum::routing::any(crate::auth::fallback))
        .layer(axum::middleware::from_fn(crate::error::json_errors))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .with_state(state);
    let header = |name, value: &'static str| {
        SetResponseHeaderLayer::if_not_present(name, HeaderValue::from_static(value))
    };
    let csp = HeaderValue::from_str(&crate::static_files::default_csp())
        .expect("static CSP is a valid header");
    Router::new()
        .nest("/api/v1", api)
        .merge(api_rest)
        .merge(crate::static_files::router(ui_dir))
        // dynamic compression only for what has no precompressed sibling (API JSON, a dev
        // build of the UI): gzip at level 4 (32 KiB window, no brotli window kept in memory),
        // bodies over 1 KiB; event streams and images are skipped (guidelines S5)
        .layer(
            CompressionLayer::new()
                .no_br()
                .no_deflate()
                .no_zstd()
                .quality(tower_http::CompressionLevel::Precise(4))
                .compress_when(DefaultPredicate::new().and(SizeAbove::new(1024))),
        )
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            csp,
        ))
        .layer(header(header::X_CONTENT_TYPE_OPTIONS, "nosniff"))
        .layer(header(header::REFERRER_POLICY, "same-origin"))
        .layer(header(header::X_FRAME_OPTIONS, "DENY"))
        // peer and local address of the connection (the network endpoints need both)
        .layer(axum::middleware::from_fn(crate::conn::expose))
}
