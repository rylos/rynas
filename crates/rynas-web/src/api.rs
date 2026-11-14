//! `/api/v1` handlers.

use std::time::Duration;

use axum::extract::{RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};

use axum::routing::{any, get, post};
use axum::{Json, Router, middleware};

use rynas_api_types::{HardwareStatus, MetricsSample, StorageOverview, SystemInfo, error_codes};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::app::AppState;
use crate::auth;
use crate::metrics::history::{MAX_POINTS, parse_minutes};
use crate::storage_api;

pub use crate::error::Error;

type ApiResult<T> = Result<Json<T>, Error>;

/// A disk with I/O in this window is reported "active".
pub(crate) const RECENT_IO: Duration = Duration::from_secs(10);

/// The `/api/v1` router: `auth/login` and `auth/session` are public, everything else goes
/// through [`auth::require_session`] (session cookie + CSRF header).
pub fn router(state: AppState) -> Router {
    // everything but the user's own session: changes need the administrator role
    let admin = Router::new()
        .route("/system", get(system))
        .route("/metrics/history", get(history))
        .route("/metrics/stream", get(crate::events::metrics_stream))
        .route("/events", get(crate::events::events))
        .route("/storage", get(storage_overview))
        .route("/storage/disks", get(storage_api::disks))
        .route("/storage/disks/{name}", get(storage_api::disk))
        .route("/hardware", get(hardware))
        .route("/ups", get(ups))
        .merge(crate::proxy::routes())
        .route_layer(middleware::from_fn(auth::require_admin));
    let protected = Router::new()
        .route("/auth/logout", post(auth::logout))
        .route("/auth/totp/enroll", post(auth::totp_enroll))
        .route("/auth/totp/confirm", post(auth::totp_confirm))
        .route("/auth/totp/disable", post(auth::totp_disable))
        // the file manager: every signed-in user, as their own Linux user
        .merge(crate::files::routes())
        // the user's own preferences of the interface (sidebar layout)
        .merge(crate::proxy::own_routes())
        .merge(admin)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ));
    Router::new()
        .route("/auth/login", post(auth::login))
        .route("/auth/session", get(auth::session))
        // the PC agents: their own token or code, checked by rynasd (no session)
        .merge(crate::pc_agent::routes())
        .merge(protected)
        // unknown endpoints: 401 without a session, so they do not reveal what exists
        .route("/{*rest}", any(auth::fallback))
        .fallback(auth::fallback)
        // 405, query string and other rejections: ApiError bodies too (A8)
        .layer(middleware::from_fn(crate::error::json_errors))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .with_state(state)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Error> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::internal(e.to_string()))
}

async fn system() -> ApiResult<SystemInfo> {
    Ok(Json(blocking(crate::system::system_info).await?))
}

async fn history(
    State(st): State<AppState>,
    RawQuery(q): RawQuery,
) -> ApiResult<Vec<MetricsSample>> {
    let minutes = parse_minutes(q.as_deref()).map_err(Error::bad_request)?;
    Ok(Json(st.store.history(minutes as usize * 60, MAX_POINTS)))
}

async fn hardware(State(st): State<AppState>) -> ApiResult<HardwareStatus> {
    let hw = st.hw.clone();
    match blocking(move || hw.status()).await? {
        Ok(Some(h)) => Ok(Json(h)),
        Ok(None) => Ok(Json(HardwareStatus::default())),
        Err(e) => Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            format!("hardware provider: {e}"),
        )),
    }
}

async fn ups(State(st): State<AppState>) -> ApiResult<rynas_api_types::ups::UpsStatus> {
    Ok(Json(st.ups.status().await))
}

async fn storage_overview(State(st): State<AppState>) -> ApiResult<StorageOverview> {
    use rynas_storage::host::SystemHost;
    use rynas_storage::overview;
    let (hw, store) = (st.hw.clone(), st.store.clone());
    // sysfs and procfs only: quick, never blocks on a device
    let (disks, arrays, mounts, removable) = blocking(move || {
        let hw = hw.status().ok().flatten();
        let recent = |d: &str| store.disk_active_within(d, RECENT_IO);
        let disks = overview::disks(&SystemHost, hw.as_ref(), &recent);
        let mounts = overview::mounts(&SystemHost);
        let removable: Vec<bool> = mounts
            .iter()
            .map(|m| overview::removable_mount(&SystemHost, &disks, m))
            .collect();
        (disks, overview::arrays(&SystemHost), mounts, removable)
    })
    .await?;
    // statvfs through the prober: own threads, bounded wait, never the shared pool (C3)
    let mounted: Vec<&str> = mounts.iter().map(|m| m.mountpoint.as_str()).collect();
    st.mounts.retain(&mounted);
    let probes = mounts.iter().map(|m| st.mounts.sizes(&m.mountpoint));
    let sizes = futures_util::future::join_all(probes).await;
    let filesystems = mounts
        .iter()
        .zip(sizes)
        .zip(removable)
        .map(|((m, p), r)| overview::filesystem(m, p.sizes, r))
        .collect();
    Ok(Json(StorageOverview {
        disks,
        arrays,
        filesystems,
    }))
}
