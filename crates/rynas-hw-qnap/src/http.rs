//! The module's routes under `/v1/modules/qnap` (`docs/plugins/qnap.md`). While the machine is
//! not confirmed every route answers 404, like on any other hardware.

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_module::http::{Ctx, Error, Res, blocking, docs, stage, user};

use crate::api::{BeepRequest, MuteResult, QnapSettings, QnapSettingsState, QnapStatus};
use crate::domain::{Cell, QNAP_KEY};

#[derive(Clone)]
struct St {
    ctx: Ctx,
    ctl: Cell,
}

impl St {
    fn ctl(&self) -> Res<std::sync::Arc<crate::control::Controller>> {
        self.ctl.get().cloned().ok_or_else(|| Error::not_found("this machine is not an identified QNAP"))
    }
}

pub fn router(ctx: Ctx, ctl: Cell) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/settings", get(settings_get).put(settings_put))
        .route("/beep", post(beep))
        .route("/mute", post(mute))
        .with_state(St { ctx, ctl })
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
