//! Configuration and task endpoints of the internal API (see [`rynas_auth::ipc`]).
//!
//! `rynas-web` has already checked the session and the CSRF token and tells who the user is
//! in the [`USER_HEADER`] header: the pending changeset is that user's, revisions and tasks
//! record that name.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt, stream};
use rynas_api_types::{
    ApplyRequest, DriftReport, HostnameConfig, HostnameState, PendingEvent, RevisionDetail,
    RevisionSummary, StagedChanges, TASK_LOG_COMPLETE, TASK_LOG_NEXT_OFFSET, TaskEventKind,
    TaskInfo, TaskStarted, error_codes,
};
use rynas_auth::ipc::USER_HEADER;
use rynas_config::{Config, ConfigError, HOSTNAME_KEY, Invalid};
use rynas_tasks::{TaskError, TaskManager};
use serde::Deserialize;

use rynas_scheduler::SCHEDULER_KEY;
use rynas_scheduler::domain::root_task_changes;
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

use crate::roles::AdminPolicy;

/// What the configuration and task endpoints work on.
pub struct Ops {
    pub config: Arc<Config>,
    pub tasks: TaskManager,
    /// Who may bring root user tasks (see [`root_task_guard`]); the system groups by default.
    pub admins: AdminPolicy,
    /// [`Config::pending_generation`] after each change of a changeset (`config_pending`
    /// events of the task stream).
    pending: tokio::sync::broadcast::Sender<u64>,
    /// Runs on the apply task's blocking thread after a successful `config.apply`, with the
    /// keys it applied, before the task ends: state that views cache and that the apply has
    /// just changed is read again here (the disk settings), so a client that reloads when
    /// the task ends sees the new values.
    pub after_apply: Option<AfterApply>,
}

/// See [`Ops::after_apply`].
pub type AfterApply = Arc<dyn Fn(&[String], &dyn Fn(&str)) + Send + Sync>;

impl Ops {
    pub fn new(config: Arc<Config>, tasks: TaskManager) -> Self {
        let (pending, _) = tokio::sync::broadcast::channel(16);
        let tx = pending.clone();
        config.on_pending_change(Box::new(move |g| {
            // no subscriber is fine
            let _ = tx.send(g);
        }));
        Self {
            config,
            tasks,
            admins: AdminPolicy::default(),
            pending,
            after_apply: None,
        }
    }
}

/// Root user tasks are for administrators (rynasd's own check, [`crate::roles`]): `who` may
/// bring a `scheduler` value from `old` to `new` only if no root user task is added, changed
/// or removed, or if `who` is an administrator. Checked where a whole value arrives at once —
/// restoring a revision, importing an archive, applying a changeset — besides the scheduler
/// endpoints, which check each task.
pub fn root_task_guard(
    admins: &AdminPolicy,
    who: &str,
    old: Option<&Value>,
    new: Option<&Value>,
) -> Res<()> {
    let ids = root_task_changes(old, new);
    if ids.is_empty() {
        return Ok(());
    }
    admins.require_admin(who, &format!("root tasks changed ({})", ids.join(", ")))
}

pub use crate::error::Error;

impl From<ConfigError> for Error {
    fn from(e: ConfigError) -> Self {
        if let ConfigError::Invalid(Invalid {
            field: Some(f),
            message,
        }) = e
        {
            return Error::invalid(f, message);
        }
        let msg = e.to_string();
        let (status, code) = match e {
            ConfigError::UnknownKey(_) | ConfigError::NotFound(_) => {
                (StatusCode::NOT_FOUND, error_codes::NOT_FOUND)
            }
            ConfigError::Invalid(_) => (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE),
            ConfigError::NothingPending => (StatusCode::CONFLICT, error_codes::NOTHING_PENDING),
            ConfigError::Conflict(_) => (StatusCode::CONFLICT, error_codes::CONFLICT),
            ConfigError::GroupSplit(_) => (StatusCode::CONFLICT, error_codes::APPLY_GROUP_SPLIT),
            ConfigError::Apply(_) | ConfigError::Cancelled | ConfigError::Storage(_) => {
                tracing::error!("configuration: {msg}");
                (StatusCode::INTERNAL_SERVER_ERROR, error_codes::INTERNAL)
            }
        };
        Error::new(status, code, msg)
    }
}

impl From<TaskError> for Error {
    fn from(e: TaskError) -> Self {
        let msg = e.to_string();
        let (status, code) = match e {
            TaskError::NotFound => (StatusCode::NOT_FOUND, error_codes::NOT_FOUND),
            TaskError::Finished => (StatusCode::CONFLICT, error_codes::TASK_FINISHED),
            TaskError::Busy => (StatusCode::CONFLICT, error_codes::BUSY),
            TaskError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, error_codes::INTERNAL),
        };
        Error::new(status, code, msg)
    }
}

pub(crate) type Res<T> = Result<T, Error>;

fn unconfigured() -> Error {
    Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        error_codes::BACKEND_UNAVAILABLE,
        "configuration and tasks are not enabled in this rynasd".into(),
    )
}

type OpsState = Option<Arc<Ops>>;

fn ops(st: &OpsState) -> Res<Arc<Ops>> {
    st.clone().ok_or_else(unconfigured)
}

/// The user `rynas-web` acts for.
pub(crate) fn user(h: &HeaderMap) -> Res<String> {
    h.get(USER_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|u| rynas_auth::valid_username(u))
        .map(str::to_string)
        .ok_or_else(|| {
            Error::new(
                StatusCode::BAD_REQUEST,
                error_codes::BAD_REQUEST,
                format!("missing or invalid {USER_HEADER} header"),
            )
        })
}

/// Runs a configuration call on a blocking thread (SQLite, files, commands, D-Bus).
pub(crate) async fn blocking<T: Send + 'static>(
    ops: &Arc<Ops>,
    f: impl FnOnce(&Config) -> Result<T, ConfigError> + Send + 'static,
) -> Res<T> {
    let cfg = ops.config.clone();
    tokio::task::spawn_blocking(move || f(&cfg))
        .await
        .map_err(Error::internal)?
        .map_err(Error::from)
}

pub fn router(ops: OpsState) -> Router {
    Router::new()
        .route("/v1/config/pending", get(pending))
        .route("/v1/config/apply", post(apply))
        .route("/v1/config/discard", post(discard))
        .route("/v1/config/revisions", get(revisions))
        .route("/v1/config/revisions/{n}", get(revision))
        .route("/v1/config/revisions/{n}/restore", post(restore))
        .route("/v1/config/drift", get(drift))
        .route("/v1/config/drift/{key}/import", post(drift_import))
        .route("/v1/config/drift/{key}/overwrite", post(drift_overwrite))
        .route("/v1/system/hostname", get(hostname_get).put(hostname_put))
        .route("/v1/tasks", get(tasks))
        .route("/v1/tasks/stream", get(task_stream))
        .route("/v1/tasks/{id}", get(task))
        .route("/v1/tasks/{id}/log", get(task_log))
        .route("/v1/tasks/{id}/cancel", post(task_cancel))
        .with_state(ops)
}

#[derive(Deserialize, Default)]
struct PendingQuery {
    /// `1` or `true`: the full answer with `base`, `value` and `diff`.
    detail: Option<String>,
}

/// `GET /v1/config/pending[?detail=1]`: compact by default (what the pending-changes bar
/// reads), the whole documents and their diff with `detail`.
///
/// The answer carries an `ETag` (a hash of the body, so it covers the user, `detail` and
/// the `conflict` flags); a request whose `If-None-Match` matches gets `304 Not Modified`
/// without a body. Clients learn that it is worth asking again from the `config.pending`
/// event of `/api/v1/events` instead of polling.
async fn pending(
    State(st): State<OpsState>,
    h: HeaderMap,
    Query(q): Query<PendingQuery>,
) -> Res<Response> {
    let (ops, user) = (ops(&st)?, user(&h)?);
    let detail = matches!(q.detail.as_deref(), Some("1" | "true"));
    let mut p = blocking(&ops, move |c| c.pending(&user)).await?;
    if !detail {
        for c in &mut p.changes {
            c.base = None;
            c.value = None;
            c.diff = Vec::new();
        }
    }
    let body = serde_json::to_vec(&p).map_err(Error::internal)?;
    let etag = etag_of(&body);
    let mut r = if if_none_match(&h, &etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            body,
        )
            .into_response()
    };
    r.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).map_err(Error::internal)?,
    );
    Ok(r)
}

/// Strong entity tag of a body: FNV-1a 64 of its bytes, quoted. Not a security
/// measure, only "has it changed" (a collision costs one missed refresh).
pub fn etag_of(body: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in body {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("\"{h:016x}\"")
}

/// Whether `If-None-Match` lists `etag` (or is `*`); weak tags compare by their value.
fn if_none_match(h: &HeaderMap, etag: &str) -> bool {
    h.get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim())
        .any(|t| t == "*" || t.strip_prefix("W/").unwrap_or(t) == etag)
}

async fn apply(
    State(st): State<OpsState>,
    h: HeaderMap,
    body: Result<Json<ApplyRequest>, JsonRejection>,
) -> Res<Response> {
    let (ops, user) = (ops(&st)?, user(&h)?);
    let Json(req) = body?;
    // refuse at once what the task would refuse, so the UI gets a plain error
    let (u, k) = (user.clone(), req.keys.clone());
    let mut p = blocking(&ops, move |c| {
        c.check_groups(&u, &k)?;
        c.pending(&u)
    })
    .await?;
    // a dialog's Save applies only the keys it staged (guidelines A19b)
    if !req.keys.is_empty() {
        p.changes.retain(|c| req.keys.contains(&c.key));
    }
    if p.changes.is_empty() {
        return Err(ConfigError::NothingPending.into());
    }
    let stale: Vec<String> = p
        .changes
        .iter()
        .filter(|c| c.conflict)
        .map(|c| c.key.clone())
        .collect();
    if !stale.is_empty() {
        return Err(ConfigError::Conflict(stale).into());
    }
    // whatever staged it (a task endpoint, a restore, an archive import, a drift import)
    if let Some(c) = p.changes.iter().find(|c| c.key == SCHEDULER_KEY) {
        root_task_guard(&ops.admins, &user, c.base.as_ref(), c.value.as_ref())?;
    }
    let cfg = ops.config.clone();
    let owner = user.clone();
    let after = ops.after_apply.clone();
    let id = ops
        .tasks
        .spawn("config.apply", &user, move |ctx| async move {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || {
                let log = |l: &str| c.log(l);
                let rev = cfg.apply_keys(&owner, &owner, &req.comment, &req.keys, &log, &|| {
                    c.is_cancelled()
                })?;
                log(&format!("revision {rev}"));
                if let Some(after) = after {
                    let keys = cfg
                        .revision(rev)
                        .map(|d| d.revision.keys)
                        .unwrap_or_default();
                    after(&keys, &log);
                }
                Ok::<_, ConfigError>(())
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
        })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

async fn discard(State(st): State<OpsState>, h: HeaderMap) -> Res<StatusCode> {
    let (ops, user) = (ops(&st)?, user(&h)?);
    blocking(&ops, move |c| c.discard(&user)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `?limit=` of the history lists (guidelines A17): the same default and maximum everywhere.
#[derive(Deserialize)]
struct Limit {
    limit: Option<usize>,
}

pub const LIST_DEFAULT: usize = 50;
pub const LIST_MAX: usize = 500;

impl Limit {
    fn get(&self) -> usize {
        self.limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX)
    }
}

async fn revisions(
    State(st): State<OpsState>,
    Query(q): Query<Limit>,
) -> Res<Json<Vec<RevisionSummary>>> {
    let ops = ops(&st)?;
    let limit = q.get();
    Ok(Json(blocking(&ops, move |c| c.revisions(limit)).await?))
}

fn rev_id(s: &str) -> Res<u64> {
    s.parse().map_err(|_| {
        Error::new(
            StatusCode::BAD_REQUEST,
            error_codes::BAD_REQUEST,
            "the revision must be a number".into(),
        )
    })
}

async fn revision(State(st): State<OpsState>, Path(n): Path<String>) -> Res<Json<RevisionDetail>> {
    let (ops, n) = (ops(&st)?, rev_id(&n)?);
    Ok(Json(blocking(&ops, move |c| c.revision(n)).await?))
}

async fn restore(
    State(st): State<OpsState>,
    h: HeaderMap,
    Path(n): Path<String>,
) -> Res<Json<StagedChanges>> {
    let (ops, user, n) = (ops(&st)?, user(&h)?, rev_id(&n)?);
    let o = ops.clone();
    let staged = tokio::task::spawn_blocking(move || -> Res<u32> {
        let c = &o.config;
        if c.keys().iter().any(|k| k == SCHEDULER_KEY) {
            let target = c.revision_snapshot(n)?.remove(SCHEDULER_KEY);
            let applied = c.applied(SCHEDULER_KEY)?;
            root_task_guard(&o.admins, &user, applied.as_ref(), target.as_ref())?;
        }
        Ok(c.restore(&user, n)?)
    })
    .await
    .map_err(Error::internal)??;
    Ok(Json(StagedChanges { staged }))
}

async fn drift(State(st): State<OpsState>) -> Res<Json<DriftReport>> {
    let ops = ops(&st)?;
    Ok(Json(blocking(&ops, |c| c.drift()).await?))
}

async fn drift_import(
    State(st): State<OpsState>,
    h: HeaderMap,
    Path(key): Path<String>,
) -> Res<Json<StagedChanges>> {
    let (ops, user) = (ops(&st)?, user(&h)?);
    let staged = blocking(&ops, move |c| c.drift_import(&user, &key)).await?;
    Ok(Json(StagedChanges { staged }))
}

async fn drift_overwrite(
    State(st): State<OpsState>,
    h: HeaderMap,
    Path(key): Path<String>,
) -> Res<Response> {
    let (ops, user) = (ops(&st)?, user(&h)?);
    if !ops.config.keys().contains(&key) {
        return Err(ConfigError::UnknownKey(key).into());
    }
    let cfg = ops.config.clone();
    let id = ops
        .tasks
        .spawn("config.overwrite", &user, move |ctx| async move {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || cfg.drift_overwrite(&key, &|l| c.log(l)))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())
        })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

fn hostname_state(c: &Config, user: &str) -> Result<HostnameState, ConfigError> {
    let parse = |v: serde_json::Value| -> Result<HostnameConfig, ConfigError> {
        serde_json::from_value(v).map_err(|e| ConfigError::Storage(e.to_string()))
    };
    let running = c.system().hostname().map_err(ConfigError::Storage)?;
    let applied = match c.current(HOSTNAME_KEY)? {
        Some(v) => parse(v)?,
        None => HostnameConfig {
            hostname: running.clone(),
        },
    };
    let pending = match c.pending_value(user, HOSTNAME_KEY)? {
        Some(Some(v)) => Some(parse(v)?),
        _ => None,
    };
    Ok(HostnameState {
        applied,
        pending,
        running,
    })
}

async fn hostname_get(State(st): State<OpsState>, h: HeaderMap) -> Res<Json<HostnameState>> {
    let (ops, user) = (ops(&st)?, user(&h)?);
    Ok(Json(
        blocking(&ops, move |c| hostname_state(c, &user)).await?,
    ))
}

async fn hostname_put(
    State(st): State<OpsState>,
    h: HeaderMap,
    body: Result<Json<HostnameConfig>, JsonRejection>,
) -> Res<Json<HostnameState>> {
    let (ops, user) = (ops(&st)?, user(&h)?);
    let Json(req) = body?;
    Ok(Json(
        blocking(&ops, move |c| {
            let v = serde_json::to_value(req)
                .map_err(|e| ConfigError::Invalid(e.to_string().into()))?;
            c.stage(&user, HOSTNAME_KEY, Some(v))?;
            hostname_state(c, &user)
        })
        .await?,
    ))
}

async fn tasks(State(st): State<OpsState>, Query(q): Query<Limit>) -> Res<Json<Vec<TaskInfo>>> {
    let ops = ops(&st)?;
    Ok(Json(ops.tasks.list(q.get())))
}

async fn task(State(st): State<OpsState>, Path(id): Path<String>) -> Res<Json<TaskInfo>> {
    let ops = ops(&st)?;
    Ok(Json(ops.tasks.get(&id).ok_or(TaskError::NotFound)?))
}

#[derive(Deserialize)]
struct LogQuery {
    offset: Option<u64>,
    limit: Option<usize>,
}

/// Plain text from `offset`; the next offset and whether the log is complete travel in
/// headers so that the body can be appended as is.
async fn task_log(
    State(st): State<OpsState>,
    Path(id): Path<String>,
    Query(q): Query<LogQuery>,
) -> Res<Response> {
    let ops = ops(&st)?;
    let limit = q.limit.unwrap_or(256 << 10).clamp(1, 1 << 20);
    let tasks = ops.tasks.clone();
    let chunk =
        tokio::task::spawn_blocking(move || tasks.read_log(&id, q.offset.unwrap_or(0), limit))
            .await
            .map_err(Error::internal)??;
    let mut r = chunk.text.into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert(TASK_LOG_NEXT_OFFSET, HeaderValue::from(chunk.next));
    h.insert(
        TASK_LOG_COMPLETE,
        HeaderValue::from_static(if chunk.complete { "true" } else { "false" }),
    );
    Ok(r)
}

async fn task_cancel(State(st): State<OpsState>, Path(id): Path<String>) -> Res<StatusCode> {
    let ops = ops(&st)?;
    ops.tasks.cancel(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

fn event_name(k: TaskEventKind) -> &'static str {
    match k {
        TaskEventKind::Started => "started",
        TaskEventKind::Updated => "updated",
        TaskEventKind::Finished => "finished",
    }
}

#[derive(Deserialize, Default)]
struct TaskStreamQuery {
    /// `1` or `true`: also the `config_pending` events.
    config: Option<String>,
}

/// SSE: one event per task change (`started`, `updated`, `finished`), data = `TaskEvent`,
/// serialized once for every subscriber. A subscriber that fell behind gets `resync` (data
/// `{}`): it re-reads `/tasks` instead of missing changes silently (guidelines A21).
///
/// With `?config=1` the stream also carries `config_pending` (data `PendingEvent`) whenever
/// a changeset may have changed, so that rynas-web's `/events` needs no third connection;
/// after a `resync` the subscriber re-reads the pending changes too.
async fn task_stream(
    State(st): State<OpsState>,
    Query(q): Query<TaskStreamQuery>,
) -> Res<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let ops = ops(&st)?;
    let rx = ops.tasks.subscribe_json();
    let tasks = stream::unfold(rx, |mut rx| async move {
        let ev = match rx.recv().await {
            Ok(e) => Event::default().event(event_name(e.kind)).data(&*e.json),
            Err(RecvError::Lagged(_)) => Event::default().event(RESYNC).data("{}"),
            Err(RecvError::Closed) => return None,
        };
        Some((Ok(ev), rx))
    });
    let config = matches!(q.config.as_deref(), Some("1" | "true")).then(|| {
        stream::unfold(ops.pending.subscribe(), |mut rx| async move {
            let ev = match rx.recv().await {
                Ok(generation) => Event::default()
                    .event(CONFIG_PENDING)
                    .data(serde_json::to_string(&PendingEvent { generation }).unwrap_or_default()),
                Err(RecvError::Lagged(_)) => Event::default().event(RESYNC).data("{}"),
                Err(RecvError::Closed) => return None,
            };
            Some((Ok(ev), rx))
        })
    });
    let events = stream::select(tasks, stream::iter(config).flatten());
    Ok(Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

/// Event of the task stream (with `?config=1`): some changeset may have changed.
pub const CONFIG_PENDING: &str = "config_pending";

/// SSE event that tells a client it missed events and must re-read the collection.
pub const RESYNC: &str = "resync";
