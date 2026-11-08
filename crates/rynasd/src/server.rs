//! The internal HTTP API (see [`rynas_auth::ipc`]) and the peer-checked Unix listener.
//!
//! Storage endpoints (`/v1/storage/...`) mirror the public `/api/v1/storage/...` ones and
//! return the `rynas_api_types::storage` types. The ones here are read-only (plans have no
//! side effects); the write side is in [`crate::storage_ops`].

use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::storage::{
    GrowPlanRequest, ReplacePlanRequest, ScrubPlanRequest, VolumePlanRequest,
};
use rynas_api_types::{ApiError, TotpCode, TotpConfirmed, error_codes};
use rynas_auth::backend::{AuthError, Authenticator, TotpError};
use rynas_auth::ipc::{AuthenticateRequest, EnrollRequest, Health};
use rynas_auth::ratelimit::{LoginLimiter, RateLimitConfig};
use rynas_auth::totp::TotpManager;
use rynas_auth::{Clock, log_safe, valid_username};
use rynas_storage::service::StorageError;
use rynas_storage::{Inventory, Storage, plan};
use tokio::net::{UnixListener, UnixStream};

#[derive(Clone)]
pub struct DaemonState {
    pub auth: Arc<dyn Authenticator>,
    pub totp: Arc<TotpManager>,
    /// Second line of defence should `rynas-web` be compromised: per-user throttling of
    /// password checks (the web server already throttles per IP and user).
    pub limiter: Arc<LoginLimiter>,
    pub groups: fn(&str) -> Option<Vec<String>>,
    /// Configuration store and task engine; `None` answers 503 on their endpoints.
    pub ops: Option<Arc<crate::ops::Ops>>,
    pub storage: Arc<Storage>,
    /// One inventory refresh at a time (single flight): concurrent page loads wait for the
    /// running one, without holding a thread, and then share its cached result.
    pub storage_lock: Arc<tokio::sync::Mutex<()>>,
    /// Users, groups and shares (needs `ops`); `None` answers 503 on their endpoints.
    pub services: Option<Arc<crate::services::Services>>,
    /// Storage write side (needs `ops`); `None` answers 503 on its endpoints.
    pub storage_ops: Option<Arc<crate::storage_ops::StorageOps>>,
    /// Where rynasd's own events go (TOTP changes, tasks, storage): the engine, or a
    /// [`rynas_notify::NullNotifier`].
    pub notifier: Arc<dyn rynas_notify::Notifier>,
    /// The notification engine; `None` answers 503 on its endpoints.
    pub notifications: Option<rynas_notify::engine::Engine>,
    /// Task Scheduler and snapshots (needs `ops`); `None` answers 503 on their endpoints.
    pub protection: Option<Arc<crate::protection::Protection>>,
    /// Provider-neutral hardware actions and the power schedule; `None` answers 503 on their
    /// endpoints.
    pub hardware: crate::hardware::HwState,
    /// The routes of the detected hardware modules (`/v1/modules/<id>/…`,
    /// `rynas_module::Modules::router`); empty without a module.
    pub modules: Router,
    /// Networking (needs `ops`); `None` answers 503 on its endpoints.
    pub network: Option<Arc<crate::network::Network>>,
    /// UPS shutdown policy, battery test, setup wizard, a module layer's pre-shutdown hook
    /// (needs `ops`); `None`
    /// answers 503 on its endpoints.
    pub ups: crate::ups::UpsState,
    /// System updates (needs `ops`); `None` answers 503 on its endpoints.
    pub updates: crate::updates::UpdState,
    /// The file manager (needs `ops`); `None` answers 503 on its endpoints.
    pub files: Option<Arc<crate::files::Files>>,
    /// Removable drives (needs `ops`); `None` answers 503 on its endpoints.
    pub removable: crate::removable::RmState,
    /// Docker (needs `ops`); `None` answers 503 on its endpoints.
    pub docker: crate::docker::DkState,
    pub apps: crate::apps::AppsState,
    /// HTTPS certificate of the web interface; `None` answers 503 on its endpoints.
    pub tls: crate::tls::TlsState,
    /// Web access (the reverse proxy); `None` answers 503 on its endpoints.
    pub web_access: crate::web_access::WaState,
    /// Tailscale (remote access); `None` answers 503 on its endpoints.
    pub tailscale: crate::tailscale::TsState,
    /// Manual restart and shut down ([`crate::system_power`]).
    pub system_power: crate::system_power::SpState,
    /// PC backup, receive side ([`crate::pc_backup`]).
    pub pc_backup: crate::pc_backup::PbState,
    /// Backups to another NAS, a disk or the cloud; `None` answers 503 on its endpoints.
    pub restic: crate::restic_backup::RsState,
}

impl DaemonState {
    pub fn new(
        auth: Arc<dyn Authenticator>,
        totp: Arc<TotpManager>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let rate = RateLimitConfig {
            free_per_user: 10,
            free_per_ip: u32::MAX,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5 * 60),
            log_failures: false,
            ..Default::default()
        };
        Self {
            auth,
            totp,
            limiter: Arc::new(LoginLimiter::new(rate, clock)),
            groups: crate::groups::user_groups,
            ops: None,
            storage: Arc::new(Storage::new(Arc::new(rynas_storage::host::SystemHost))),
            storage_lock: Arc::new(tokio::sync::Mutex::new(())),
            services: None,
            storage_ops: None,
            notifier: Arc::new(rynas_notify::NullNotifier),
            notifications: None,
            protection: None,
            hardware: None,
            modules: Router::new(),
            network: None,
            ups: None,
            updates: None,
            files: None,
            removable: None,
            docker: None,
            apps: None,
            tls: None,
            web_access: None,
            tailscale: None,
            system_power: None,
            pc_backup: None,
            restic: None,
        }
    }
}

use crate::error::Error;

impl From<TotpError> for Error {
    fn from(e: TotpError) -> Self {
        let msg = e.to_string();
        match e {
            TotpError::AlreadyEnabled => {
                Error::new(StatusCode::CONFLICT, error_codes::TOTP_ALREADY_ENABLED, msg)
            }
            TotpError::NotEnrolled => {
                Error::new(StatusCode::CONFLICT, error_codes::TOTP_NOT_ENROLLED, msg)
            }
            TotpError::InvalidCode => {
                Error::new(StatusCode::BAD_REQUEST, error_codes::INVALID_TOTP, msg)
            }
            TotpError::InvalidUser => Error::bad_request(msg),
            TotpError::Unavailable(_) => Error::internal(msg),
        }
    }
}

type Res<T> = Result<T, Error>;

/// The limiter wants an IP: rynasd has a single local client.
const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

fn check_user(user: &str) -> Res<()> {
    if valid_username(user) {
        Ok(())
    } else {
        Err(Error::bad_request("invalid user name"))
    }
}

pub fn router(state: DaemonState) -> Router {
    let admins = state
        .ops
        .as_ref()
        .map(|o| o.admins.clone())
        .unwrap_or_default();
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/authenticate", post(authenticate))
        .route("/v1/users/{user}/groups", get(groups))
        .route("/v1/totp/{user}", get(totp_status))
        .route("/v1/totp/{user}/enroll", post(totp_enroll))
        .route("/v1/totp/{user}/confirm", post(totp_confirm))
        .route("/v1/totp/{user}/disable", post(totp_disable))
        .route("/v1/totp/{user}/verify", post(totp_verify))
        .route("/v1/storage/disks", get(storage_disks))
        .route("/v1/storage/disks/{name}", get(storage_disk))
        .route("/v1/storage/arrays", get(storage_arrays))
        .route("/v1/storage/filesystems", get(storage_filesystems))
        .route("/v1/storage/tree", get(storage_tree))
        .route("/v1/storage/plan/volume", post(plan_volume))
        .route("/v1/storage/plan/grow", post(plan_grow))
        .route("/v1/storage/plan/replace", post(plan_replace))
        .route("/v1/storage/plan/scrub", post(plan_scrub))
        .with_state(state.clone())
        .merge(crate::ops::router(state.ops))
        .merge(crate::services::router(state.services))
        .merge(crate::storage_ops::router(state.storage_ops))
        .merge(crate::places::router())
        .merge(crate::notifications::router(state.notifications))
        .merge(crate::scheduler::router(state.protection.clone()))
        .merge(crate::replication::router(state.protection.clone()))
        .merge(crate::snapshots::router(state.protection))
        .merge(crate::hardware::router(state.hardware.clone()))
        .merge(crate::power::router(state.hardware))
        .merge(state.modules)
        .merge(crate::network::router(state.network))
        .merge(crate::ups::router(state.ups))
        .merge(crate::updates::router(state.updates))
        .merge(crate::files::router(state.files))
        .merge(crate::removable::router(state.removable))
        .merge(crate::docker::router(state.docker))
        .merge(crate::apps::router(state.apps))
        .merge(crate::tls::router(state.tls, admins))
        .merge(crate::web_access::router(state.web_access))
        .merge(crate::tailscale::router(state.tailscale))
        .merge(crate::system_power::router(state.system_power))
        .merge(crate::pc_backup::router(state.pc_backup))
        .merge(crate::restic_backup::router(state.restic))
        .fallback(|| async { Error::not_found("no such endpoint") })
        // 405, query string and path rejections: ApiError bodies too
        .layer(axum::middleware::from_fn(crate::error::json_errors))
}

async fn health() -> Json<Health> {
    Json(Health {
        version: env!("CARGO_PKG_VERSION").into(),
    })
}

async fn authenticate(
    State(st): State<DaemonState>,
    body: Result<Json<AuthenticateRequest>, JsonRejection>,
) -> Res<Response> {
    let Json(req) = body?;
    let invalid = || {
        Error::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_CREDENTIALS,
            "invalid user name or password".into(),
        )
    };
    if !valid_username(&req.username) {
        return Err(invalid());
    }
    if let Err(d) = st.limiter.check(LOCAL, &req.username) {
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", d.as_secs().max(1).to_string())],
            Json(ApiError::new(
                error_codes::RATE_LIMITED,
                "too many failed attempts",
            )),
        )
            .into_response());
    }
    match st.auth.authenticate(&req.username, &req.password).await {
        Ok(id) => {
            st.limiter.record_success(LOCAL, &req.username);
            Ok(Json(id).into_response())
        }
        Err(AuthError::Unavailable(e)) => {
            tracing::error!("authentication backend: {e}");
            Err(Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                error_codes::AUTH_UNAVAILABLE,
                e,
            ))
        }
        Err(AuthError::InvalidCredentials | AuthError::UnknownUser) => {
            tracing::warn!("PAM authentication failure for {}", log_safe(&req.username));
            st.limiter.record_failure(LOCAL, &req.username);
            Err(invalid())
        }
    }
}

async fn groups(State(st): State<DaemonState>, Path(user): Path<String>) -> Res<Json<Vec<String>>> {
    check_user(&user)?;
    (st.groups)(&user)
        .map(Json)
        .ok_or_else(|| Error::not_found("no such user"))
}

async fn totp_status(
    State(st): State<DaemonState>,
    Path(user): Path<String>,
) -> Res<Json<rynas_auth::TotpStatus>> {
    check_user(&user)?;
    Ok(Json(st.totp.status_sync(&user)?))
}

async fn totp_enroll(
    State(st): State<DaemonState>,
    Path(user): Path<String>,
    body: Result<Json<EnrollRequest>, JsonRejection>,
) -> Res<Json<rynas_api_types::TotpEnrollment>> {
    check_user(&user)?;
    let Json(req) = body?;
    Ok(Json(st.totp.enroll_sync(&user, &req.account)?))
}

async fn totp_confirm(
    State(st): State<DaemonState>,
    Path(user): Path<String>,
    body: Result<Json<TotpCode>, JsonRejection>,
) -> Res<Json<TotpConfirmed>> {
    check_user(&user)?;
    let Json(req) = body?;
    let recovery_codes = st.totp.confirm_sync(&user, &req.code)?;
    st.notifier
        .emit(rynas_notify::checks::totp_changed(&user, true));
    Ok(Json(TotpConfirmed { recovery_codes }))
}

async fn totp_disable(
    State(st): State<DaemonState>,
    Path(user): Path<String>,
    body: Result<Json<TotpCode>, JsonRejection>,
) -> Res<StatusCode> {
    check_user(&user)?;
    let Json(req) = body?;
    st.totp.disable_sync(&user, &req.code)?;
    st.notifier
        .emit(rynas_notify::checks::totp_changed(&user, false));
    Ok(StatusCode::NO_CONTENT)
}

async fn totp_verify(
    State(st): State<DaemonState>,
    Path(user): Path<String>,
    body: Result<Json<TotpCode>, JsonRejection>,
) -> Res<StatusCode> {
    check_user(&user)?;
    let Json(req) = body?;
    st.totp.verify_sync(&user, &req.code)?;
    Ok(StatusCode::NO_CONTENT)
}

impl From<StorageError> for Error {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::NotFound(_) => Error::not_found(e.to_string()),
            StorageError::Tool(_) => Error::internal(format!("storage: {e}")),
        }
    }
}

/// Longest wait for a running inventory refresh (rynas-web gives up at 90 s anyway).
const STORAGE_WAIT: Duration = Duration::from_secs(85);

/// A blocking storage call on the blocking pool, without any global lock.
async fn blocking_storage<T: Send + 'static>(
    st: &DaemonState,
    f: impl FnOnce(&Storage) -> Result<T, StorageError> + Send + 'static,
) -> Res<T> {
    let storage = st.storage.clone();
    tokio::task::spawn_blocking(move || f(&storage))
        .await
        .map_err(Error::internal)?
        .map_err(Error::from)
}

/// The storage inventory: the cached one (5 s), else a refresh, at most one at a time
/// (guidelines S8). Waiters queue on an async lock, not on blocking threads, and give up
/// after [`STORAGE_WAIT`].
pub(crate) async fn inventory(st: &DaemonState) -> Res<Arc<Inventory>> {
    if let Some(inv) = st.storage.cached_inventory() {
        return Ok(inv);
    }
    let _guard = tokio::time::timeout(STORAGE_WAIT, st.storage_lock.lock())
        .await
        .map_err(|_| {
            Error::new(
                StatusCode::GATEWAY_TIMEOUT,
                error_codes::TIMEOUT,
                "the storage inventory is still being read".into(),
            )
        })?;
    if let Some(inv) = st.storage.cached_inventory() {
        return Ok(inv);
    }
    blocking_storage(st, |s| s.refresh_inventory()).await
}

async fn storage_disks(
    State(st): State<DaemonState>,
) -> Res<Json<Vec<rynas_api_types::storage::DiskSummary>>> {
    Ok(Json(inventory(&st).await?.disks.clone()))
}

use rynas_api_types::validate::disk_name as valid_disk_name;

/// One disk with SMART: outside the inventory lock (SMART of a disk is cached for 10
/// minutes and never read from a sleeping disk).
async fn storage_disk(
    State(st): State<DaemonState>,
    Path(name): Path<String>,
) -> Res<Json<rynas_api_types::storage::DiskDetail>> {
    if !valid_disk_name(&name) {
        return Err(Error::bad_request("invalid disk name"));
    }
    blocking_storage(&st, move |s| s.disk(&name))
        .await
        .map(Json)
}

async fn storage_arrays(
    State(st): State<DaemonState>,
) -> Res<Json<Vec<rynas_api_types::storage::MdArray>>> {
    Ok(Json(inventory(&st).await?.arrays.clone()))
}

async fn storage_filesystems(
    State(st): State<DaemonState>,
) -> Res<Json<Vec<rynas_api_types::storage::BtrfsFilesystem>>> {
    Ok(Json(inventory(&st).await?.filesystems.clone()))
}

async fn storage_tree(
    State(st): State<DaemonState>,
) -> Res<Json<rynas_api_types::storage::StorageTree>> {
    let inv = inventory(&st).await?;
    // cryptsetup status of the open LUKS mappings: quick, but still a command
    blocking_storage(&st, move |s| Ok(s.tree_of(&inv)))
        .await
        .map(Json)
}

type PlanRes = Res<Json<rynas_api_types::storage::StoragePlan>>;

/// Plans are pure functions of the (cached) inventory: no side effects, no command.
async fn plan_volume(
    State(st): State<DaemonState>,
    body: Result<Json<VolumePlanRequest>, JsonRejection>,
) -> PlanRes {
    let Json(req) = body?;
    Ok(Json(plan::plan_volume(&*inventory(&st).await?, &req)))
}

async fn plan_grow(
    State(st): State<DaemonState>,
    body: Result<Json<GrowPlanRequest>, JsonRejection>,
) -> PlanRes {
    let Json(req) = body?;
    Ok(Json(plan::plan_grow(&*inventory(&st).await?, &req)))
}

async fn plan_replace(
    State(st): State<DaemonState>,
    body: Result<Json<ReplacePlanRequest>, JsonRejection>,
) -> PlanRes {
    let Json(req) = body?;
    Ok(Json(plan::plan_replace(&*inventory(&st).await?, &req)))
}

async fn plan_scrub(
    State(st): State<DaemonState>,
    body: Result<Json<ScrubPlanRequest>, JsonRejection>,
) -> PlanRes {
    let Json(req) = body?;
    Ok(Json(plan::plan_scrub(&*inventory(&st).await?, &req)))
}

/// The credentials of the connected peer (`SO_PEERCRED`), available to handlers as
/// `ConnectInfo<PeerCred>` when the router is served with
/// `into_make_service_with_connect_info::<PeerCred>()` (see [`serve`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    pub uid: u32,
    pub pid: Option<i32>,
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, PeerCheckedListener>>
    for PeerCred
{
    fn connect_info(s: axum::serve::IncomingStream<'_, PeerCheckedListener>) -> Self {
        match s.io().peer_cred() {
            Ok(c) => PeerCred {
                uid: c.uid(),
                pid: c.pid(),
            },
            // never matches a uid check
            Err(_) => PeerCred {
                uid: u32::MAX,
                pid: None,
            },
        }
    }
}

/// Serves `app` on `listener` with the peer credentials of each connection attached to its
/// requests ([`PeerCred`]), which the root-only routes need ([`require_root_peer`]).
pub async fn serve(
    listener: PeerCheckedListener,
    app: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<PeerCred>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

/// Route layer of the internal routes only root may call (`rynasctl` from systemd units):
/// the connection's peer must be uid 0. Requests without peer credentials (a router served
/// without [`serve`]) are refused too. 403 `forbidden`.
pub async fn require_root_peer(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<PeerCred>>()
        .map(|c| c.0);
    match peer {
        Some(PeerCred { uid: 0, .. }) => next.run(req).await,
        other => {
            tracing::warn!("{} refused: root only (peer {other:?})", req.uri().path());
            Error::new(
                StatusCode::FORBIDDEN,
                error_codes::FORBIDDEN,
                "this endpoint is reserved to root".into(),
            )
            .into_response()
        }
    }
}

/// A Unix listener that drops connections from uids not in `allowed` (`SO_PEERCRED`).
pub struct PeerCheckedListener {
    inner: UnixListener,
    allowed: Arc<HashSet<u32>>,
}

impl PeerCheckedListener {
    pub fn new(inner: UnixListener, allowed: HashSet<u32>) -> Self {
        Self {
            inner,
            allowed: Arc::new(allowed),
        }
    }
}

impl axum::serve::Listener for PeerCheckedListener {
    type Io = UnixStream;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((stream, addr)) => match stream.peer_cred() {
                    Ok(c) if self.allowed.contains(&c.uid()) => return (stream, addr),
                    Ok(c) => {
                        tracing::warn!("refused connection from uid {} pid {:?}", c.uid(), c.pid())
                    }
                    Err(e) => tracing::warn!("SO_PEERCRED failed: {e}"),
                },
                Err(e) if is_connection_error(&e) => {}
                Err(e) => {
                    tracing::error!("accept: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}
