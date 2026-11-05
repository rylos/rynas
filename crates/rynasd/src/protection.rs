//! Data protection (P3): what the Task Scheduler and snapshot endpoints share.
//!
//! - [`crate::scheduler`]: `/v1/scheduler/...` (tasks, runs, previews, external timers and
//!   crontabs) and the internal routes called by `rynasctl` from the generated units;
//! - [`crate::snapshots`]: `/v1/snapshots/...` (jobs, listing, browse, restore, rollback,
//!   delete);
//! - [`crate::jobs`]: the internal actions a scheduler task runs as a rynasd task.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use rynas_api_types::error_codes;
use rynas_btrfs::SnapshotsDomain;
use rynas_btrfs::scan::SnapError;
use rynas_notify::Notifier;
use rynas_scheduler::SchedulerDomain;
use rynas_scheduler::history::History;
use rynas_scheduler::probe::{SystemdProbe, TimerState};

use crate::ops::{Error, Ops, Res};

pub struct Protection {
    pub ops: Arc<Ops>,
    pub scheduler: Arc<SchedulerDomain>,
    pub snapshots: Arc<SnapshotsDomain>,
    pub probe: Arc<dyn SystemdProbe>,
    pub history: Arc<History>,
    pub notifier: Arc<dyn Notifier>,
    /// rynasd's state directory (reasons log of system restore points, previews).
    pub state_dir: PathBuf,
    /// Timers and running task units, read at most every [`RUNTIME_TTL`] (guidelines S8).
    /// `(read at, read at in ms since the epoch, state)`.
    runtime: Mutex<Option<(Instant, u64, Arc<Runtime>)>>,
    /// One runtime read at a time.
    runtime_flight: tokio::sync::Mutex<()>,
}

/// What systemd says about the task units now.
#[derive(Debug, Default)]
pub struct Runtime {
    pub timers: Vec<TimerState>,
    pub running: Vec<String>,
}

pub const RUNTIME_TTL: Duration = Duration::from_secs(2);

impl Protection {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ops: Arc<Ops>,
        scheduler: Arc<SchedulerDomain>,
        snapshots: Arc<SnapshotsDomain>,
        probe: Arc<dyn SystemdProbe>,
        history: Arc<History>,
        notifier: Arc<dyn Notifier>,
        state_dir: PathBuf,
    ) -> Self {
        Self {
            ops,
            scheduler,
            snapshots,
            probe,
            history,
            notifier,
            state_dir,
            runtime: Mutex::new(None),
            runtime_flight: tokio::sync::Mutex::new(()),
        }
    }

    /// The cached runtime state, refreshed when older than [`RUNTIME_TTL`] or older than the
    /// end of the latest configuration apply (which starts and stops timers); a failing
    /// systemd answers an empty state (logged), never an error page.
    pub async fn runtime(self: &Arc<Self>) -> Arc<Runtime> {
        let last_apply = self
            .ops
            .tasks
            .list(20)
            .iter()
            .filter(|t| t.kind.starts_with("config."))
            .filter_map(|t| t.end_ms)
            .max()
            .unwrap_or(0);
        let fresh = |p: &Protection| {
            p.runtime
                .lock()
                .unwrap()
                .as_ref()
                .filter(|(at, at_ms, _)| at.elapsed() < RUNTIME_TTL && *at_ms > last_apply)
                .map(|(_, _, r)| r.clone())
        };
        if let Some(r) = fresh(self) {
            return r;
        }
        let _flight = self.runtime_flight.lock().await;
        if let Some(r) = fresh(self) {
            return r;
        }
        let p = self.clone();
        let read_ms = rynas_tasks::now_ms();
        let r = tokio::task::spawn_blocking(move || {
            let timers = p.probe.timers().unwrap_or_else(|e| {
                tracing::warn!("systemd timers: {e}");
                Vec::new()
            });
            let running = p.probe.running_tasks().unwrap_or_else(|e| {
                tracing::warn!("systemd units: {e}");
                Vec::new()
            });
            Runtime { timers, running }
        })
        .await
        .unwrap_or_default();
        let r = Arc::new(r);
        *self.runtime.lock().unwrap() = Some((Instant::now(), read_ms, r.clone()));
        r
    }

    /// Forget the cached runtime (after run now, apply).
    pub fn invalidate(&self) {
        *self.runtime.lock().unwrap() = None;
    }
}

pub type ProtState = Option<Arc<Protection>>;

pub fn prot(st: &ProtState) -> Res<Arc<Protection>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "the scheduler and snapshots are not enabled in this rynasd".into(),
        )
    })
}

/// Runs `f` on a blocking thread.
pub async fn blocking<T: Send + 'static>(
    p: &Arc<Protection>,
    f: impl FnOnce(&Protection) -> Res<T> + Send + 'static,
) -> Res<T> {
    let p = p.clone();
    tokio::task::spawn_blocking(move || f(&p))
        .await
        .map_err(Error::internal)?
}

impl From<SnapError> for Error {
    fn from(e: SnapError) -> Self {
        match e {
            SnapError::Invalid(m) => {
                Error::new(StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE, m)
            }
            SnapError::NotFound(m) => Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, m),
            SnapError::Refused(m) => Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, m),
            SnapError::Failed(m) => Error::internal(m),
        }
    }
}

pub fn invalid(msg: impl Into<String>) -> Error {
    Error::new(
        StatusCode::BAD_REQUEST,
        error_codes::INVALID_VALUE,
        msg.into(),
    )
}

pub fn conflict(code: &'static str, msg: impl Into<String>) -> Error {
    Error::new(StatusCode::CONFLICT, code, msg.into())
}

pub fn not_found(what: impl Into<String>) -> Error {
    Error::new(
        StatusCode::NOT_FOUND,
        error_codes::NOT_FOUND,
        format!("{} not found", what.into()),
    )
}

/// `?limit=` and `?before=` of history lists (guidelines A17).
#[derive(serde::Deserialize)]
pub struct Page {
    pub limit: Option<usize>,
    pub before: Option<u64>,
}

impl Page {
    pub fn limit(&self) -> usize {
        self.limit
            .unwrap_or(crate::ops::LIST_DEFAULT)
            .clamp(1, crate::ops::LIST_MAX)
    }
}
