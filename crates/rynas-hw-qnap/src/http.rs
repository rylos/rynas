//! The module's routes under `/v1/modules/qnap` (`docs/plugins/qnap.md`). While the machine is
//! not confirmed every route answers 404, like on any other hardware.

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::error_codes;
use rynas_module::http::{Ctx, Error, Res, blocking, docs, stage, user};

use crate::api::{
    BeepRequest, ConfirmMapping, MuteResult, PowerRecoveryRequest, PowerRecoveryState, QnapSettings, QnapSettingsState,
    QnapStatus,
};
use crate::domain::{Cell, QNAP_KEY};
use crate::module::Runner;

#[derive(Clone)]
struct St {
    ctx: Ctx,
    shared: Cell,
    runner: std::sync::Arc<dyn Runner>,
}

impl St {
    fn ctl(&self) -> Res<std::sync::Arc<crate::control::Controller>> {
        self.shared.ctl.get().cloned().ok_or_else(|| Error::not_found("this machine is not an identified QNAP"))
    }
}

pub fn router(ctx: Ctx, shared: Cell, runner: std::sync::Arc<dyn Runner>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/settings", get(settings_get).put(settings_put))
        .route("/beep", post(beep))
        .route("/mute", post(mute))
        .route("/confirm-mapping", post(confirm_mapping).delete(revoke_mapping))
        .route("/power-recovery", get(power_recovery_get).put(power_recovery_put))
        .route("/activate-gpio", post(activate_gpio))
        .with_state(St { ctx, shared, runner })
}

/// No port access of any kind before the user's confirmation.
fn require_confirmed(st: &St) -> Res<()> {
    if st.ctl()?.mapping_confirmed() {
        return Ok(());
    }
    Err(Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, "confirm the model on this machine first"))
}

/// The Super I/O's GPIO device, activated the way the vendor does it.
async fn activate_gpio(State(st): State<St>) -> Res<Json<PowerRecoveryState>> {
    Ok(Json(
        blocking(move || {
            require_confirmed(&st)?;
            st.runner.activate_gpio().map_err(Error::unavailable)
        })
        .await?,
    ))
}

/// The user typed the model's name after the first-boot procedure.
async fn confirm_mapping(State(st): State<St>, body: Result<Json<ConfirmMapping>, JsonRejection>) -> Res<StatusCode> {
    let Json(req) = body?;
    blocking(move || {
        st.ctl()?
            .confirm_mapping(&req.typed)
            .map_err(|m| Error::new(StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE, m))
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn revoke_mapping(State(st): State<St>) -> Res<StatusCode> {
    blocking(move || st.ctl()?.revoke_mapping().map_err(Error::internal)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Read from the hardware now, never cached.
async fn power_recovery_get(State(st): State<St>) -> Res<Json<PowerRecoveryState>> {
    Ok(Json(
        blocking(move || {
            require_confirmed(&st)?;
            st.runner.read().map_err(Error::unavailable)
        })
        .await?,
    ))
}

/// Only here, on an explicit request, and only with the mapping confirmed.
async fn power_recovery_put(
    State(st): State<St>,
    body: Result<Json<PowerRecoveryRequest>, JsonRejection>,
) -> Res<Json<PowerRecoveryState>> {
    let Json(req) = body?;
    Ok(Json(
        blocking(move || {
            require_confirmed(&st)?;
            st.runner.set(req.mode).map_err(Error::unavailable)
        })
        .await?,
    ))
}

async fn status(State(st): State<St>) -> Res<Json<QnapStatus>> {
    Ok(Json(blocking(move || Ok(st.ctl()?.status())).await?))
}

fn settings_state(st: &St, owner: &str) -> Res<QnapSettingsState> {
    st.ctl()?;
    let (eff, cur, _): (QnapSettings, QnapSettings, Option<QnapSettings>) = docs(&st.ctx.config, owner, QNAP_KEY)?;
    Ok(QnapSettingsState { pending: (eff != cur).then_some(eff), applied: cur })
}

async fn settings_get(State(st): State<St>, hd: HeaderMap) -> Res<Json<QnapSettingsState>> {
    let owner = user(&hd)?;
    Ok(Json(blocking(move || settings_state(&st, &owner)).await?))
}

async fn settings_put(
    State(st): State<St>,
    hd: HeaderMap,
    body: Result<Json<QnapSettings>, JsonRejection>,
) -> Res<Json<QnapSettingsState>> {
    let owner = user(&hd)?;
    let Json(s) = body?;
    Ok(Json(
        blocking(move || {
            st.ctl()?;
            stage(&st.ctx.config, &owner, QNAP_KEY, &s)?;
            settings_state(&st, &owner)
        })
        .await?,
    ))
}

async fn beep(State(st): State<St>, body: Result<Json<BeepRequest>, JsonRejection>) -> Res<StatusCode> {
    let Json(req) = body?;
    blocking(move || {
        let ctl = st.ctl()?;
        if !ctl.beeper_available() {
            return Err(Error::unavailable("no PC speaker device"));
        }
        ctl.beep_async(req.kind);
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn mute(State(st): State<St>, hd: HeaderMap) -> Res<Json<MuteResult>> {
    let who = user(&hd)?;
    Ok(Json(
        blocking(move || {
            let stopped_beep = st.ctl()?.mute();
            let acknowledged = (st.ctx.ack_hardware)(&who).map_err(Error::internal)?;
            Ok(MuteResult { stopped_beep, acknowledged })
        })
        .await?,
    ))
}
