//! Notification endpoints of the internal API, the hardware module forward socket and the
//! background sources (finished tasks, periodic storage check).
//!
//! rynasd is the single owner of the engine ([`rynas_notify::engine`]). Public routes mirror
//! `/api/v1/notifications/...`; two are internal only: `POST /v1/notifications/events`
//! (rynas-web emits hardware and login events) and `GET /v1/notifications/stream` (SSE,
//! multiplexed by rynas-web into `/api/v1/events` as `notification`).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{Stream, stream};
use rynas_api_types::error_codes;
use rynas_api_types::notify::{
    AckAllResult, EventState, NewEvent, Notification, NotifyAccepted, NotifyCategory, NotifyRule,
    NotifySeverity, NotifyTarget, Silence, SilenceRequest, TargetRequest, TargetTestResult,
};
use rynas_notify::Notifier;
use rynas_notify::checks::{StorageWatch, task_event};
use rynas_notify::engine::{Engine, ListQuery, NotifyError};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use crate::error::{Error, created};
use crate::ops::{LIST_DEFAULT, LIST_MAX, RESYNC, user};

type Res<T> = Result<T, Error>;
type St = Option<Engine>;

impl From<NotifyError> for Error {
    fn from(e: NotifyError) -> Self {
        let msg = e.to_string();
        match e {
            NotifyError::NotFound(_) => {
                Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, msg)
            }
            NotifyError::Invalid(_) => {
                Error::new(StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE, msg)
            }
            NotifyError::Exists(_) => {
                Error::new(StatusCode::CONFLICT, error_codes::ALREADY_EXISTS, msg)
            }
            NotifyError::Conflict(_) => {
                Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, msg)
            }
            NotifyError::Storage(_) => Error::internal(msg),
        }
    }
}

fn engine(st: &St) -> Res<Engine> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "notifications are not enabled in this rynasd".into(),
        )
    })
}

/// Runs an engine call on the blocking pool (SQLite).
async fn blocking<T: Send + 'static>(
    e: &Engine,
    f: impl FnOnce(&Engine) -> rynas_notify::engine::Result<T> + Send + 'static,
) -> Res<T> {
    let e = e.clone();
    tokio::task::spawn_blocking(move || f(&e))
        .await
        .map_err(Error::internal)?
        .map_err(Error::from)
}

fn json_body<T>(b: Result<Json<T>, JsonRejection>) -> Res<T> {
    b.map(|Json(v)| v).map_err(Error::from)
}

fn id(s: &str) -> Res<u64> {
    s.parse()
        .map_err(|_| Error::bad_request("the id must be a number"))
}

pub fn router(st: St) -> Router {
    Router::new()
        .route("/v1/notifications", get(list))
        .route("/v1/notifications/events", post(emit))
        .route("/v1/notifications/stream", get(changes))
        .route("/v1/notifications/ack-all", post(ack_all))
        .route("/v1/notifications/silence", post(silence))
        .route("/v1/notifications/silences", get(silences))
        .route(
            "/v1/notifications/silences/{id}",
            axum::routing::delete(unsilence),
        )
        .route(
            "/v1/notifications/targets",
            get(targets).post(target_create),
        )
        .route(
            "/v1/notifications/targets/{id}",
            get(target_get).put(target_put).delete(target_delete),
        )
        .route("/v1/notifications/targets/{id}/test", post(target_test))
        .route("/v1/notifications/rules", get(rules).post(rule_create))
        .route(
            "/v1/notifications/rules/{id}",
            get(rule_get).put(rule_put).delete(rule_delete),
        )
        .route("/v1/notifications/{id}/ack", post(ack))
        .with_state(st)
}

/// `?limit=&before=&severity=&category=&state=&acked=&key=&source=`; `severity` means
/// "at least".
#[derive(Deserialize)]
struct ListParams {
    limit: Option<usize>,
    before: Option<u64>,
    severity: Option<NotifySeverity>,
    category: Option<NotifyCategory>,
    state: Option<EventState>,
    acked: Option<bool>,
    key: Option<String>,
    source: Option<String>,
}

async fn list(State(st): State<St>, Query(p): Query<ListParams>) -> Res<Json<Vec<Notification>>> {
    let e = engine(&st)?;
    let q = ListQuery {
        limit: p.limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX),
        before: p.before,
        min_severity: p.severity,
        category: p.category,
        state: p.state,
        acked: p.acked,
        key: p.key,
        source: p.source,
    };
    Ok(Json(blocking(&e, move |e| e.list(&q)).await?))
}

/// Internal: an event from rynas-web (hardware, logins). Stored at once, so the answer
/// tells whether it was new.
async fn emit(State(st): State<St>, body: Result<Json<NewEvent>, JsonRejection>) -> Res<Response> {
    let e = engine(&st)?;
    let ev = json_body(body)?;
    let n = blocking(&e, move |e| e.ingest(ev, false)).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(NotifyAccepted {
            id: n.map(|n| n.id),
        }),
    )
        .into_response())
}

/// Internal SSE: `notification` events (data `NotificationEvent`, serialized once),
/// `resync` after a lag.
async fn changes(State(st): State<St>) -> Res<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let e = engine(&st)?;
    let rx = e.subscribe();
    let events = stream::unfold(rx, |mut rx| async move {
        let ev = match rx.recv().await {
            Ok(j) => Event::default().event("notification").data(&*j),
            Err(RecvError::Lagged(_)) => Event::default().event(RESYNC).data("{}"),
            Err(RecvError::Closed) => return None,
        };
        Some((Ok(ev), rx))
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

async fn ack(State(st): State<St>, h: HeaderMap, Path(n): Path<String>) -> Res<Json<Notification>> {
    let (e, user, n) = (engine(&st)?, user(&h)?, id(&n)?);
    Ok(Json(blocking(&e, move |e| e.ack(n, &user)).await?))
}

async fn ack_all(State(st): State<St>, h: HeaderMap) -> Res<Json<AckAllResult>> {
    let (e, user) = (engine(&st)?, user(&h)?);
    let acked = blocking(&e, move |e| e.ack_all(&user)).await?;
    Ok(Json(AckAllResult { acked }))
}

async fn silence(
    State(st): State<St>,
    h: HeaderMap,
    body: Result<Json<SilenceRequest>, JsonRejection>,
) -> Res<Response> {
    let (e, user, req) = (engine(&st)?, user(&h)?, json_body(body)?);
    let s: Silence = blocking(&e, move |e| e.silence(&req, &user)).await?;
    Ok(created(
        "/api/v1/notifications/silences",
        &s.id.to_string(),
        s,
    ))
}

async fn silences(State(st): State<St>) -> Res<Json<Vec<Silence>>> {
    let e = engine(&st)?;
    Ok(Json(blocking(&e, |e| e.silences()).await?))
}

async fn unsilence(State(st): State<St>, Path(n): Path<String>) -> Res<StatusCode> {
    let (e, n) = (engine(&st)?, id(&n)?);
    blocking(&e, move |e| e.delete_silence(n)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn targets(State(st): State<St>) -> Res<Json<Vec<NotifyTarget>>> {
    let e = engine(&st)?;
    Ok(Json(blocking(&e, |e| e.targets()).await?))
}

async fn target_create(
    State(st): State<St>,
    body: Result<Json<TargetRequest>, JsonRejection>,
) -> Res<Response> {
    let (e, req) = (engine(&st)?, json_body(body)?);
    let t = blocking(&e, move |e| e.create_target(&req)).await?;
    Ok(created(
        "/api/v1/notifications/targets",
        &t.id.to_string(),
        t,
    ))
}

async fn target_get(State(st): State<St>, Path(n): Path<String>) -> Res<Json<NotifyTarget>> {
    let (e, n) = (engine(&st)?, id(&n)?);
    Ok(Json(blocking(&e, move |e| e.target(n)).await?))
}

async fn target_put(
    State(st): State<St>,
    Path(n): Path<String>,
    body: Result<Json<TargetRequest>, JsonRejection>,
) -> Res<Json<NotifyTarget>> {
    let (e, n, req) = (engine(&st)?, id(&n)?, json_body(body)?);
    Ok(Json(blocking(&e, move |e| e.update_target(n, &req)).await?))
}

async fn target_delete(State(st): State<St>, Path(n): Path<String>) -> Res<StatusCode> {
    let (e, n) = (engine(&st)?, id(&n)?);
    blocking(&e, move |e| e.delete_target(n)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Sends a test message (at most ~40 s: the channel timeouts) and reports the outcome.
async fn target_test(
    State(st): State<St>,
    h: HeaderMap,
    Path(n): Path<String>,
) -> Res<Json<TargetTestResult>> {
    let (e, user, n) = (engine(&st)?, user(&h)?, id(&n)?);
    Ok(Json(e.test_target(n, &user).await?))
}

async fn rules(State(st): State<St>) -> Res<Json<Vec<NotifyRule>>> {
    let e = engine(&st)?;
    Ok(Json(blocking(&e, |e| e.rules()).await?))
}

async fn rule_create(
    State(st): State<St>,
    body: Result<Json<NotifyRule>, JsonRejection>,
) -> Res<Response> {
    let (e, rule) = (engine(&st)?, json_body(body)?);
    let r = blocking(&e, move |e| e.create_rule(&rule)).await?;
    Ok(created("/api/v1/notifications/rules", &r.id.to_string(), r))
}

async fn rule_get(State(st): State<St>, Path(n): Path<String>) -> Res<Json<NotifyRule>> {
    let (e, n) = (engine(&st)?, id(&n)?);
    Ok(Json(blocking(&e, move |e| e.rule(n)).await?))
}

async fn rule_put(
    State(st): State<St>,
    Path(n): Path<String>,
    body: Result<Json<NotifyRule>, JsonRejection>,
) -> Res<Json<NotifyRule>> {
    let (e, n, rule) = (engine(&st)?, id(&n)?, json_body(body)?);
    Ok(Json(blocking(&e, move |e| e.update_rule(n, &rule)).await?))
}

async fn rule_delete(State(st): State<St>, Path(n): Path<String>) -> Res<StatusCode> {
    let (e, n) = (engine(&st)?, id(&n)?);
    blocking(&e, move |e| e.delete_rule(n)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------- module forward socket

/// The router of `/run/rynas/notify.sock`, opened only when a hardware module provides an
/// `EventForward` (D-MODULE): `POST /v1/notify` with a body the module parses, `GET
/// /v1/health`.
pub fn socket_router(engine: Engine, parser: Arc<dyn rynas_module::EventForward>) -> Router {
    let max = parser.max_body();
    Router::new()
        .route("/v1/notify", post(forward))
        .route(
            "/v1/health",
            get(|| async {
                Json(rynas_auth::ipc::Health {
                    version: env!("CARGO_PKG_VERSION").into(),
                })
            }),
        )
        .layer(DefaultBodyLimit::max(max))
        .fallback(|| async { Error::not_found("no such endpoint") })
        .layer(axum::middleware::from_fn(crate::error::json_errors))
        .with_state((engine, parser))
}

type ForwardState = (Engine, Arc<dyn rynas_module::EventForward>);

async fn forward(State((e, f)): State<ForwardState>, h: HeaderMap, body: Bytes) -> Res<Response> {
    let ct = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
    let (ev, force) = f.parse(ct, &body).map_err(Error::bad_request)?;
    let n = blocking(&e, move |e| e.ingest(ev, force)).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(NotifyAccepted {
            id: n.map(|n| n.id),
        }),
    )
        .into_response())
}

// ------------------------------------------------------------------- background sources

/// Finished tasks: a failure opens `task:<type>` (`config:apply` for configuration
/// applies), the next success of the same type resolves it. Tasks of user
/// [`crate::scheduler::SCHEDULER_USER`] are left to the scheduler's run notification.
pub fn spawn_task_watch(tasks: &rynas_tasks::TaskManager, notifier: Arc<dyn Notifier>) {
    let mut rx = tasks.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                // tasks started by scheduler units are notified by their run, with the
                // task's own policy (crate::scheduler::run_event)
                Ok(ev)
                    if ev.kind == rynas_api_types::TaskEventKind::Finished
                        && ev.task.user != crate::scheduler::SCHEDULER_USER =>
                {
                    if let Some(n) = task_event(&ev.task) {
                        notifier.emit(n);
                    }
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            }
        }
    });
}

/// Every `interval`: disk verdicts, md arrays, btrfs device errors and volume usage from
/// the shared storage inventory (the same 5 s snapshot and single-flight lock as the
/// storage pages; nothing in it wakes a disk: SMART is only the cached on-demand reading,
/// md details and btrfs usage are skipped while a disk sleeps).
pub fn spawn_storage_watch(
    st: crate::server::DaemonState,
    notifier: Arc<dyn Notifier>,
    interval: Duration,
    modules: rynas_module::Modules,
) {
    tokio::spawn(async move {
        let mut watch = StorageWatch::default();
        // let the system settle after boot (arrays assembling, mounts)
        tokio::time::sleep(Duration::from_secs(60).min(interval)).await;
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            match crate::server::inventory(&st).await {
                Ok(inv) => {
                    let health: Vec<_> = inv.disks.iter().map(|d| (d.name.clone(), d.health)).collect();
                    modules.storage_update(&health);
                    for ev in watch.check(&inv.disks, &inv.arrays, &inv.filesystems) {
                        notifier.emit(ev);
                    }
                }
                Err(e) => tracing::warn!("storage check: {}", e.2),
            }
        }
    });
}
