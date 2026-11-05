//! Synology hardware endpoints of the internal API (`/v1/hardware/synology/...`, public under
//! `/api/v1/hardware/synology/...`) and the state shared with the power schedule
//! ([`crate::power`]).
//!
//! The provider is `rynas_hw_synology::Synology`, behind the `rynas_hw` traits: the handlers
//! only use `HardwareControl` for actions, so the provider can move into a plugin process.
//! Settings are the `synology.hw` configuration key (staged in the session user's
//! changeset, applied with `/v1/config/apply`). On a machine that is not a Synology every
//! synology endpoint answers 404.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::error_codes;
use rynas_api_types::notify::{EventState, NotifyCategory};
use rynas_api_types::synology::{
    BeepRequest, FanCurves, IdentifyRequest, MuteResult, SynologyPlatform, SynologySettings,
    SynologySettingsState, SynologyStatus,
};
use rynas_hw::{ControlError, HardwareControl};
use rynas_hw_synology::Synology;
use rynas_hw_synology::domain::{SYNOLOGY_KEY, foreign_commands};
use rynas_power::wake::PowerState;

use crate::ops::{Error, Ops, Res, user};
use crate::services::{docs, stage};

/// Start of rynasd on a Synology: makes `synology.hw` the owner of `/etc/synohwd.toml` —
/// network rescue on the reset button the first time, the legacy `# BEGIN rynas network`
/// block of `network.rescue` folded in (see `rynas_hw_synology::domain::startup_value`).
/// Returns the revision when something was applied. Blocking.
pub fn adopt_synohwd_conf(cfg: &rynas_config::Config) -> Result<Option<u64>, String> {
    if !cfg.keys().iter().any(|k| k == SYNOLOGY_KEY) {
        return Ok(None);
    }
    // the old `network.rescue` value had the reset button (later values never do)
    let legacy = cfg
        .applied(rynas_net::rescue::RESCUE_KEY)
        .map_err(|e| e.to_string())?
        .and_then(|v| v.get("reset_button").and_then(serde_json::Value::as_bool));
    let Some((v, comment)) = rynas_hw_synology::domain::startup_value(cfg, legacy)? else {
        return Ok(None);
    };
    cfg.apply_value("rynas", SYNOLOGY_KEY, Some(v), comment, &|l| {
        tracing::info!("synology.hw: {l}")
    })
    .map(Some)
    .map_err(|e| e.to_string())
}

/// What the hardware and power endpoints work on.
pub struct Hardware {
    pub ops: Arc<Ops>,
    /// `None`: not a Synology.
    pub synology: Option<Arc<Synology>>,
    /// Root of `/sys`, `/proc`, `/run/synohwd` (tests: a fake tree).
    pub hw_root: PathBuf,
    /// rynasd's state directory (`power/state.json`).
    pub state_dir: PathBuf,
    /// The notification engine (mute acknowledges hardware alerts); `None` in tests.
    pub notifications: Option<rynas_notify::engine::Engine>,
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

fn synology(h: &Hardware) -> Res<Arc<Synology>> {
    h.synology.clone().ok_or_else(|| {
        Error::not_found("this machine is not a Synology (no synology-nas platform driver)")
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

pub fn router(st: HwState) -> Router {
    Router::new()
        .route("/v1/hardware/synology/status", get(status))
        .route(
            "/v1/hardware/synology/settings",
            get(settings_get).put(settings_put),
        )
        .route("/v1/hardware/synology/beep", post(beep))
        .route("/v1/hardware/synology/mute", post(mute))
        .route("/v1/hardware/synology/bays/{n}/identify", post(identify))
        .route("/v1/hardware/synology/fan-curves", get(fan_curves))
        .with_state(st)
}

async fn status(State(st): State<HwState>) -> Res<Json<SynologyStatus>> {
    let h = hw(&st)?;
    Ok(Json(
        blocking(&h, |h| {
            let p = synology(h)?;
            let mut s = p.synology_status().map_err(|e| {
                Error::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    error_codes::BACKEND_UNAVAILABLE,
                    format!("synohwd: {e}"),
                )
            })?;
            s.boot = PowerState::load(&h.state_dir).boot;
            Ok(s)
        })
        .await?,
    ))
}

fn settings_state(h: &Hardware, owner: &str) -> Res<SynologySettingsState> {
    let p = synology(h)?;
    let c = &h.ops.config;
    let (eff, cur, _): (SynologySettings, SynologySettings, Option<SynologySettings>) =
        docs(c, owner, SYNOLOGY_KEY)?;
    let running = p.synology_status().map(|s| s.running).unwrap_or(false);
    Ok(SynologySettingsState {
        pending: (eff != cur).then_some(eff),
        foreign_commands: if c.applied(SYNOLOGY_KEY)?.is_some() {
            Vec::new()
        } else {
            foreign_commands(&h.hw_root)
        },
        applied: cur,
        platform: SynologyPlatform {
            model: p.product().to_string(),
            bays: p.bays(),
            fans: rynas_hw_synology::model::FANS,
            synohwd_running: running,
        },
    })
}

async fn settings_get(
    State(st): State<HwState>,
    hd: HeaderMap,
) -> Res<Json<SynologySettingsState>> {
    let (h, owner) = (hw(&st)?, user(&hd)?);
    Ok(Json(
        blocking(&h, move |h| settings_state(h, &owner)).await?,
    ))
}

async fn settings_put(
    State(st): State<HwState>,
    hd: HeaderMap,
    body: Result<Json<SynologySettings>, JsonRejection>,
) -> Res<Json<SynologySettingsState>> {
    let (h, owner) = (hw(&st)?, user(&hd)?);
    let Json(s) = body?;
    Ok(Json(
        blocking(&h, move |h| {
            synology(h)?;
            stage(&h.ops.config, &owner, SYNOLOGY_KEY, &s)?;
            settings_state(h, &owner)
        })
        .await?,
    ))
}

async fn beep(
    State(st): State<HwState>,
    body: Result<Json<BeepRequest>, JsonRejection>,
) -> Res<StatusCode> {
    let h = hw(&st)?;
    let Json(req) = body?;
    blocking(&h, move |h| Ok(synology(h)?.beep(req.kind)?)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Stops a sounding alarm (none with synohwd: its beeps are single) and acknowledges the open
/// hardware notifications so their reminders stop.
async fn mute(State(st): State<HwState>, hd: HeaderMap) -> Res<Json<MuteResult>> {
    let (h, who) = (hw(&st)?, user(&hd)?);
    Ok(Json(
        blocking(&h, move |h| {
            let stopped_beep = synology(h)?.mute()?;
            let mut acknowledged = 0;
            if let Some(e) = &h.notifications {
                let open = e
                    .list(&rynas_notify::engine::ListQuery {
                        limit: 500,
                        category: Some(NotifyCategory::Hardware),
                        state: Some(EventState::Firing),
                        acked: Some(false),
                        ..Default::default()
                    })
                    .map_err(Error::internal)?;
                for n in open.iter().filter(|n| n.resolved_ms.is_none()) {
                    if e.ack(n.id, &who).is_ok() {
                        acknowledged += 1;
                    }
                }
            }
            Ok(MuteResult {
                stopped_beep,
                acknowledged,
            })
        })
        .await?,
    ))
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
    blocking(&h, move |h| Ok(synology(h)?.identify(bay, d)?)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn fan_curves(State(st): State<HwState>, hd: HeaderMap) -> Res<Json<FanCurves>> {
    let (h, owner) = (hw(&st)?, user(&hd)?);
    Ok(Json(
        blocking(&h, move |h| {
            let p = synology(h)?;
            let (_, cur, _): (SynologySettings, SynologySettings, _) =
                docs(&h.ops.config, &owner, SYNOLOGY_KEY)?;
            p.set_active_mode(cur.fan_mode);
            p.fan_curves()
                .ok_or_else(|| Error::not_found("this provider has no fan curves"))
        })
        .await?,
    ))
}
