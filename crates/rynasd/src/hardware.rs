//! The provider-neutral hardware endpoint of the internal API (bay identify,
//! `/v1/hardware/bays/{n}/identify`, public under `/api/v1/hardware/...`) and the state shared
//! with the power schedule ([`crate::power`]).
//!
//! The actions go through `rynas_hw::HardwareControl` of the machine's hardware module
//! (`rynas_module::Module::hardware_control`); without one the endpoint answers 404. A
//! module's own endpoints are its router under `/v1/modules/<id>` (`rynas_module::http`).
//! [`ack_hardware_alerts`] is what a module's "mute" acknowledges.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use rynas_api_types::notify::{EventState, NotifyCategory};
use rynas_api_types::{IdentifyRequest, error_codes};
use rynas_hw::{ControlError, HardwareControl};

use crate::ops::{Error, Ops, Res};

/// What the hardware and power endpoints work on.
pub struct Hardware {
    pub ops: Arc<Ops>,
    /// Actions of the hardware module's provider; `None`: no module hardware.
    pub control: Option<Arc<dyn HardwareControl>>,
    /// Root of `/sys`, `/proc`, `/run` (tests: a fake tree).
    pub hw_root: PathBuf,
    /// rynasd's state directory (`power/state.json`).
    pub state_dir: PathBuf,
    pub notifier: Arc<dyn rynas_notify::Notifier>,
    /// Task Scheduler (shutdown rules are its tasks); `None` answers 503 on power writes.
    pub protection: Option<Arc<crate::protection::Protection>>,
    /// argv of the clean poweroff of a scheduled shutdown (`systemctl poweroff`).
    pub poweroff: Vec<String>,
}

pub type HwState = Option<Arc<Hardware>>;

pub fn hw(st: &HwState) -> Res<Arc<Hardware>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "hardware and power are not enabled in this rynasd".into(),
        )
    })
}

impl From<ControlError> for Error {
    fn from(e: ControlError) -> Self {
        match e {
            ControlError::NotFound(m) => Error::not_found(m),
            ControlError::Invalid(m) => {
                Error::new(StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE, m)
            }
            ControlError::Unavailable(m) => Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                error_codes::BACKEND_UNAVAILABLE,
                m,
            ),
            ControlError::Busy(m) => Error::new(StatusCode::CONFLICT, error_codes::BUSY, m),
            ControlError::Failed(m) => Error::internal(m),
        }
    }
}

/// Runs `f` on a blocking thread (files, FIFO, SQLite).
pub async fn blocking<T: Send + 'static>(
    h: &Arc<Hardware>,
    f: impl FnOnce(&Hardware) -> Res<T> + Send + 'static,
) -> Res<T> {
    let h = h.clone();
    tokio::task::spawn_blocking(move || f(&h))
        .await
        .map_err(Error::internal)?
}

/// Acknowledges the open hardware notifications as `who` so their reminders stop (a module's
/// "mute"); how many.
pub fn ack_hardware_alerts(
    engine: &rynas_notify::engine::Engine,
    who: &str,
) -> Result<u64, String> {
    let open = engine
        .list(&rynas_notify::engine::ListQuery {
            limit: 500,
            category: Some(NotifyCategory::Hardware),
            state: Some(EventState::Firing),
            acked: Some(false),
            ..Default::default()
        })
        .map_err(|e| e.to_string())?;
    let mut acknowledged = 0;
    for n in open.iter().filter(|n| n.resolved_ms.is_none()) {
        if engine.ack(n.id, who).is_ok() {
            acknowledged += 1;
        }
    }
    Ok(acknowledged)
}

pub fn router(st: HwState) -> Router {
    Router::new()
        .route("/v1/hardware/bays/{n}/identify", post(identify))
        .with_state(st)
}

/// Default identify duration, as in the spec (`docs/plugins/synology.md`).
pub const IDENTIFY_DEFAULT: Duration = Duration::from_secs(30);

async fn identify(
    State(st): State<HwState>,
    Path(n): Path<String>,
    body: Bytes,
) -> Res<StatusCode> {
    let h = hw(&st)?;
    let bay: u32 = n
        .parse()
        .map_err(|_| Error::bad_request("the bay must be a number"))?;
    // the body is optional
    let req: IdentifyRequest = if body.iter().all(u8::is_ascii_whitespace) {
        IdentifyRequest::default()
    } else {
        rynas_http::from_slice(&body).map_err(Error::from)?
    };
    let d = req
        .duration_s
        .map_or(IDENTIFY_DEFAULT, |s| Duration::from_secs(s as u64));
    blocking(&h, move |h| {
        let c = h
            .control
            .clone()
            .ok_or_else(|| Error::not_found("this machine has no bay LEDs rynas can drive"))?;
        Ok(c.identify(bay, d)?)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
