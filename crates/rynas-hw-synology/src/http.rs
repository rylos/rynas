//! The module's routes (`rynas_module::Module::router`): rynasd nests them under
//! `/v1/modules/synology` (public `/api/v1/modules/synology/...`), only where the module is
//! detected. Without Synology hardware (synodeb alone) every route answers 404.
//!
//! The handlers only use `HardwareControl` for actions, so the provider can move into a
//! plugin process. Settings are the `synology.hw` configuration key (staged in the session
//! user's changeset, applied with `/v1/config/apply`). Bay identify is the core's
//! provider-neutral `/v1/hardware/bays/{n}/identify` ([`rynas_module::Module::hardware_control`]).

use std::sync::Arc;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::synology::{
    BeepRequest, FanCurves, MuteResult, SynologyPlatform, SynologySettings, SynologySettingsState,
    SynologyStatus,
};
use rynas_hw::HardwareControl;
use rynas_module::http::{Ctx, Error, Res, blocking, docs, stage, user};

use crate::Synology;
use crate::domain::{SYNOLOGY_KEY, foreign_commands};

#[derive(Clone)]
struct St {
    ctx: Ctx,
    synology: Option<Arc<Synology>>,
}

impl St {
    fn synology(&self) -> Res<Arc<Synology>> {
        self.synology.clone().ok_or_else(|| {
            Error::not_found("this machine is not a Synology (no synology-nas platform driver)")
        })
    }
}

/// The routes, relative to the module's prefix.
pub fn router(ctx: Ctx, synology: Option<Arc<Synology>>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/settings", get(settings_get).put(settings_put))
        .route("/beep", post(beep))
        .route("/mute", post(mute))
        .route("/fan-curves", get(fan_curves))
        .with_state(St { ctx, synology })
}

async fn status(State(st): State<St>) -> Res<Json<SynologyStatus>> {
    Ok(Json(
        blocking(move || {
            let p = st.synology()?;
            let mut s = p
                .synology_status()
                .map_err(|e| Error::unavailable(format!("synohwd: {e}")))?;
            s.boot = rynas_power::wake::PowerState::load(&st.ctx.state_dir).boot;
            Ok(s)
        })
        .await?,
    ))
}

fn settings_state(st: &St, owner: &str) -> Res<SynologySettingsState> {
    let p = st.synology()?;
    let c = &st.ctx.config;
    let (eff, cur, _): (SynologySettings, SynologySettings, Option<SynologySettings>) =
        docs(c, owner, SYNOLOGY_KEY)?;
    let running = p.synology_status().map(|s| s.running).unwrap_or(false);
    Ok(SynologySettingsState {
        pending: (eff != cur).then_some(eff),
        foreign_commands: if c.applied(SYNOLOGY_KEY)?.is_some() {
            Vec::new()
        } else {
            foreign_commands(&st.ctx.hw_root)
        },
        applied: cur,
        platform: SynologyPlatform {
            model: p.product().to_string(),
            bays: p.bays(),
            fans: crate::model::FANS,
            synohwd_running: running,
        },
    })
}

async fn settings_get(State(st): State<St>, hd: HeaderMap) -> Res<Json<SynologySettingsState>> {
    let owner = user(&hd)?;
    Ok(Json(blocking(move || settings_state(&st, &owner)).await?))
}

async fn settings_put(
    State(st): State<St>,
    hd: HeaderMap,
    body: Result<Json<SynologySettings>, JsonRejection>,
) -> Res<Json<SynologySettingsState>> {
    let owner = user(&hd)?;
    let Json(s) = body?;
    Ok(Json(
        blocking(move || {
            st.synology()?;
            stage(&st.ctx.config, &owner, SYNOLOGY_KEY, &s)?;
            settings_state(&st, &owner)
        })
        .await?,
    ))
}

async fn beep(
    State(st): State<St>,
    body: Result<Json<BeepRequest>, JsonRejection>,
) -> Res<StatusCode> {
    let Json(req) = body?;
    blocking(move || Ok(st.synology()?.beep(req.kind)?)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Stops a sounding alarm (none with synohwd: its beeps are single) and acknowledges the open
/// hardware notifications so their reminders stop.
async fn mute(State(st): State<St>, hd: HeaderMap) -> Res<Json<MuteResult>> {
    let who = user(&hd)?;
    Ok(Json(
        blocking(move || {
            let stopped_beep = st.synology()?.mute()?;
            let acknowledged = (st.ctx.ack_hardware)(&who).map_err(Error::internal)?;
            Ok(MuteResult {
                stopped_beep,
                acknowledged,
            })
        })
        .await?,
    ))
}

async fn fan_curves(State(st): State<St>, hd: HeaderMap) -> Res<Json<FanCurves>> {
    let owner = user(&hd)?;
    Ok(Json(
        blocking(move || {
            let p = st.synology()?;
            let (_, cur, _): (SynologySettings, SynologySettings, _) =
                docs(&st.ctx.config, &owner, SYNOLOGY_KEY)?;
            p.set_active_mode(cur.fan_mode);
            p.fan_curves()
                .ok_or_else(|| Error::not_found("this provider has no fan curves"))
        })
        .await?,
    ))
}
