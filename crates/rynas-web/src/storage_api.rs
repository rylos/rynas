//! `/api/v1/storage/disks[/{name}]`: the disks as rynasd sees them (smartctl needs root),
//! plus what only the web server knows: bay numbers and the sleeping flag of the hardware
//! provider. The other storage views and the plans are passed through unchanged
//! ([`crate::proxy`]).

use axum::extract::{Path, State};
use axum::http::Method;
use axum::{Extension, Json};
use rynas_api_types::storage::{DiskDetail, DiskSummary, PowerState};

use crate::app::AppState;
use crate::auth::CurrentSession;
use crate::error::Error;

const NONE: Option<&()> = None;

/// Bay and M.2 slot numbers, and the power state when rynasd's own check had no answer (USB
/// bridges that do not pass ATA CHECK POWER MODE): recent I/O seen by the metrics collector
/// means awake, else the provider's sleeping flag (the same rule as the dashboard).
async fn add_hw(st: &AppState, disks: &mut [&mut DiskSummary]) {
    let hw = st.hw.clone();
    let h = match tokio::task::spawn_blocking(move || hw.status()).await {
        Ok(Ok(h)) => h,
        _ => None,
    };
    for d in disks.iter_mut() {
        let x = rynas_storage::overview::hw_disk(h.as_ref(), &d.name);
        d.bay = d.bay.or(x.bay);
        d.m2 = d.m2.or(x.m2);
        if d.power_state == PowerState::Unknown {
            let recent = st.store.disk_active_within(&d.name, crate::api::RECENT_IO);
            d.power_state = rynas_storage::overview::power_state(x.asleep, recent);
        }
    }
}

use rynas_api_types::validate::disk_name as valid_disk_name;

pub async fn disks(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
) -> Result<Json<Vec<DiskSummary>>, Error> {
    let mut v: Vec<DiskSummary> = st
        .rynasd
        .call(
            Method::GET,
            "/v1/storage/disks",
            Some(&s.user.username),
            NONE,
        )
        .await?;
    add_hw(&st, &mut v.iter_mut().collect::<Vec<_>>()).await;
    Ok(Json(v))
}

pub async fn disk(
    State(st): State<AppState>,
    Extension(CurrentSession(s)): Extension<CurrentSession>,
    Path(name): Path<String>,
) -> Result<Json<DiskDetail>, Error> {
    if !valid_disk_name(&name) {
        return Err(Error::bad_request("invalid disk name"));
    }
    let mut d: DiskDetail = st
        .rynasd
        .call(
            Method::GET,
            &format!("/v1/storage/disks/{name}"),
            Some(&s.user.username),
            NONE,
        )
        .await?;
    add_hw(&st, &mut [&mut d.summary]).await;
    Ok(Json(d))
}
