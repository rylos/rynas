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
                    for ev in watch.check(&inv.disks, &inv.arrays, &inv.filesystems, &inv.flex) {
                        notifier.emit(ev);
                    }
                }
                Err(e) => tracing::warn!("storage check: {}", e.2),
            }
        }
    });
}

/// How often the arrays left inactive after boot are looked for (a glance at /proc/mdstat;
/// the inventory is read only when an array is inactive).
const DEGRADED_START_INTERVAL: Duration = Duration::from_secs(60);

/// Starts the arrays of rynas volumes that stayed inactive because a drive is missing, once
/// the boot has settled, then the volume group and the mount of the volume; tells the
/// administrators. The decisions and the reasons are in [`rynas_storage::recover`]. Never
/// under `--dry-system`, only for arrays seen inactive soon after boot, never while a storage
/// operation holds a disk or the volume (it may stop arrays on purpose): the start holds the
/// volume's reservation itself.
pub fn spawn_degraded_start(st: crate::server::DaemonState, notifier: Arc<dyn Notifier>) {
    use rynas_storage::host::SystemHost;
    use rynas_storage::recover::{self, Recovery};
    tokio::spawn(async move {
        let mut seen = Recovery::default();
        let mut tick = tokio::time::interval(DEGRADED_START_INTERVAL);
        loop {
            tick.tick().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let uptime = recover::uptime_s(&SystemHost);
            // only what the boot left inactive (an array stopped later stays stopped)
            if seen.finished(uptime) {
                tracing::debug!("degraded start: past the boot window, no longer watching");
                return;
            }
            let mdstat = tokio::fs::read_to_string("/proc/mdstat").await.unwrap_or_default();
            if !mdstat.lines().any(|l| l.contains(" : inactive ")) {
                seen.due(Vec::new(), now, uptime);
                continue;
            }
            // a quick look first; the reservation below decides
            if st.storage_ops.as_ref().is_some_and(|o| o.storage_busy()) {
                continue;
            }
            let inv = match crate::server::inventory(&st).await {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!("degraded start: {}", e.2);
                    continue;
                }
            };
            // without the records nothing is known to be rynas's
            let Some(records) = inv.records.clone() else { continue };
            for v in seen.due(recover::startable(&inv, &records), now, uptime) {
                // the volume reserved for the start, checked in the same step: never while an
                // operation works on a disk or on this volume (it may stop arrays on purpose)
                let held = match &st.storage_ops {
                    Some(o) => match o.reserve_for_recovery(&v.label) {
                        Some(h) => Some(h),
                        None => continue,
                    },
                    // no write side: no operation can run
                    None => None,
                };
                // what was read before the reservation may be stale: read again under it
                st.storage.invalidate();
                let still = match crate::server::inventory(&st).await {
                    Ok(i) => i.records.as_ref().is_some_and(|r| recover::startable(&i, r).contains(&v)),
                    Err(_) => false,
                };
                if !still {
                    continue;
                }
                seen.forget(&v);
                let names = degraded_arrays(&v);
                tracing::warn!("volume {}: starting {} without the missing drives", v.label, names);
                let job = v.clone();
                let r = tokio::task::spawn_blocking(move || recover::start(&SystemHost, &job))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                drop(held);
                st.storage.invalidate();
                notifier.emit(degraded_start_event(&v.label, &names, r));
            }
        }
    });
}

/// The arrays of a degraded start as the notification lists them: each with the drives it
/// started on (`md127 (raid1) on 1 of 2 drives`).
fn degraded_arrays(v: &rynas_storage::recover::VolumeStart) -> String {
    v.arrays
        .iter()
        .map(|a| format!("{} ({}) on {} of {} drives", a.array, a.level, a.present, a.raid_disks))
        .collect::<Vec<_>>()
        .join(", ")
}

fn degraded_start_event(volume: &str, arrays: &str, r: Result<bool, String>) -> rynas_api_types::notify::NewEvent {
    use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
    use rynas_notify::event::{code, field, oneshot};
    let key = format!("storage:degraded-start:{volume}");
    let ev = match r {
        Ok(_) => {
            tracing::warn!("volume {volume} started degraded");
            let mut ev = oneshot(
                NotifyCategory::Storage,
                NotifySeverity::Warning,
                "storage",
                key,
                format!("Volume {volume} started degraded"),
            );
            ev.body = format!(
                "Started degraded: {arrays}. They stayed inactive after boot because a drive is missing and were started on the drives present so that the volume is available. If the missing drive had dropped out earlier and came back later, the drives present may hold older data than it. Reconnect or replace the missing drive to restore redundancy: it is rebuilt from the drives present."
            );
            code(ev, "storage.volume_started_degraded")
        }
        Err(e) => {
            tracing::error!("volume {volume}: degraded start failed: {e}");
            let mut ev = oneshot(
                NotifyCategory::Storage,
                NotifySeverity::Error,
                "storage",
                key,
                format!("Volume {volume} could not be started"),
            );
            ev.body = format!("{arrays} stayed inactive after boot because a drive is missing, and starting them without it failed: {e}");
            field(code(ev, "storage.volume_start_failed"), "error", e)
        }
    };
    field(field(ev, "volume", volume), "arrays", arrays)
}

/// How often rynasd looks for the alert a storage change leaves when the initramfs refresh
/// went wrong ([`rynas_storage::domain::INITRAMFS_ALERT`], a missing file).
const INITRAMFS_ALERT_INTERVAL: Duration = Duration::from_secs(20);

/// Tells the administrators when regenerating the initramfs after a change of the arrays
/// failed or was rejected: the change itself is kept (never under `--dry-system`).
pub fn spawn_initramfs_alerts(notifier: Arc<dyn Notifier>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(INITRAMFS_ALERT_INTERVAL);
        loop {
            tick.tick().await;
            let found = tokio::task::spawn_blocking(|| rynas_storage::domain::take_initramfs_alert(std::path::Path::new("/")))
                .await
                .ok()
                .flatten();
            if let Some(a) = found {
                notifier.emit(initramfs_event(&a));
            }
        }
    });
}

fn initramfs_event(a: &rynas_storage::domain::InitramfsAlert) -> NewEvent {
    use rynas_notify::event::{code, field, oneshot};
    use rynas_storage::domain::InitramfsAlertKind as K;
    let release = if a.release.is_empty() { "-" } else { a.release.as_str() };
    let d = &a.detail;
    let (severity, c, title, body) = match a.kind {
        K::Failed => (
            NotifySeverity::Warning,
            "storage.initramfs_refresh_failed",
            "Boot image not brought up to date".to_string(),
            format!(
                "Regenerating the initial boot image (initramfs) of kernel {release} after a change of the arrays failed: {d}. The change was kept and the volumes work; the boot image still lists the arrays from before, which matters only for an array needed before the system starts. Run update-initramfs -u from a root shell."
            ),
        ),
        K::Rejected => (
            NotifySeverity::Warning,
            "storage.initramfs_refresh_rejected",
            "Boot image not updated: the new one was unusable".to_string(),
            format!(
                "The regenerated initial boot image (initramfs) of kernel {release} was unusable ({d}), so the previous one was put back and the NAS still starts. The change was kept; the boot image still lists the arrays from before. Run update-initramfs -u -k {release} from a root shell and check the result."
            ),
        ),
        K::NotRestored => (
            NotifySeverity::Error,
            "storage.initramfs_restore_failed",
            "The NAS may not start: its boot image is unusable".to_string(),
            format!(
                "The regenerated initial boot image (initramfs) of kernel {release} is unusable and the previous one could not be put back: {d}. Fix it from a root shell before restarting the NAS."
            ),
        ),
    };
    match a.kind {
        K::NotRestored => tracing::error!("initramfs of {release}: {d}"),
        _ => tracing::warn!("initramfs of {release}: {d}"),
    }
    let mut ev = oneshot(NotifyCategory::Storage, severity, "storage", format!("storage:initramfs:{release}"), title);
    ev.body = body;
    field(field(code(ev, c), "release", release), "detail", d.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_storage::domain::{InitramfsAlert, InitramfsAlertKind};
    use rynas_storage::recover::{Runnable, VolumeStart};

    fn fields(ev: &NewEvent) -> String {
        serde_json::to_string(ev).unwrap()
    }

    /// The notification says on how many of how many drives each array started.
    #[test]
    fn degraded_start_says_on_how_many_drives() {
        let a = |name: &str, level: &str, present: u32, raid_disks: u32| Runnable {
            array: name.into(),
            path: format!("/dev/{name}"),
            uuid: "AA:01".into(),
            level: level.into(),
            present,
            raid_disks,
            members: Vec::new(),
        };
        let v = VolumeStart {
            label: "vol2".into(),
            arrays: vec![a("md126", "raid1", 1, 2), a("md127", "raid5", 2, 3)],
            vg: Some("rynas_vol2".into()),
            mount_unit: None,
        };
        let names = degraded_arrays(&v);
        assert_eq!(names, "md126 (raid1) on 1 of 2 drives, md127 (raid5) on 2 of 3 drives");
        let ev = degraded_start_event("vol2", &names, Ok(true));
        assert_eq!(ev.title, "Volume vol2 started degraded");
        assert!(ev.body.starts_with("Started degraded: md126 (raid1) on 1 of 2 drives"), "{}", ev.body);
        assert!(ev.body.contains("older data"), "{}", ev.body);
        assert!(fields(&ev).contains("storage.volume_started_degraded"));
    }

    /// Each outcome of an initramfs refresh has its code; only an initramfs left unusable
    /// is an error.
    #[test]
    fn initramfs_alerts_become_notifications() {
        let alert = |kind| InitramfsAlert { kind, release: "6.12.10-amd64".into(), detail: "no kernel modules".into() };
        let ev = initramfs_event(&alert(InitramfsAlertKind::Rejected));
        assert_eq!(ev.severity, NotifySeverity::Warning);
        let j = fields(&ev);
        assert!(j.contains("storage.initramfs_refresh_rejected"), "{j}");
        assert!(j.contains("6.12.10-amd64") && j.contains("no kernel modules"), "{j}");
        let ev = initramfs_event(&alert(InitramfsAlertKind::Failed));
        assert!(fields(&ev).contains("storage.initramfs_refresh_failed"));
        let ev = initramfs_event(&alert(InitramfsAlertKind::NotRestored));
        assert_eq!(ev.severity, NotifySeverity::Error);
        assert!(fields(&ev).contains("storage.initramfs_restore_failed"));
        let ev = initramfs_event(&InitramfsAlert { kind: InitramfsAlertKind::Failed, release: String::new(), detail: "x".into() });
        assert!(ev.body.contains("of kernel -"), "{}", ev.body);
    }
}
