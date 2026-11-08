//! Network endpoints of the internal API (`/v1/network/...`, public under
//! `/api/v1/network/...`), on top of [`rynas_net::Engine`] (D-NET).
//!
//! Interface settings are staged per user (`PUT /v1/network/interfaces/{name}`) and applied
//! with `POST /v1/network/apply`, which is **not** `/config/apply`: a network change is a
//! transaction with a timed confirmation, so it has its own cycle. The apply answers at once
//! with the confirmation token and the URLs to confirm from; the change itself runs in a task
//! (`network.apply`) that stays running until the change is confirmed or rolled back. The
//! rollback timer is a systemd transient unit (`rynasctl net rollback`), so it fires even when
//! rynasd is gone; the task rolls back by itself only when that timer did not fire.
//!
//! `rynas-web` adds the local address of the session's connection
//! ([`rynas_auth::ipc::LOCAL_ADDR_HEADER`]): the management interface is derived from it, and a
//! confirmation must arrive on an address of the new configuration.
//!
//! The rescue settings (`network.rescue`: Avahi service file) are a normal configuration key:
//! `PUT /v1/network/rescue` stages, `/config/apply` applies. A reset button is a setting of
//! the machine's hardware module (`rynas_module::Module::reset_button`), shown here read-only.
//!
//! Internal only: `POST /v1/network/events/flush` (`rynasctl` after a rollback, the boot guard
//! or the watchdog): the engine's queued events go to the notification engine.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use rynas_api_types::network::{
    AdoptMode, DiagnosticResult, DnsLookupRequest, InterfaceConfig, NetAdoptRequest,
    NetAdoptScheduled, NetApplyRequest, NetApplyStarted, NetConfirmRequest, NetConfirmed,
    NetInterface, NetRestoreRequest, NetRevision, NetworkState, PingRequest, RescueInfo,
    RescueSettings, TracerouteRequest, TrialKind,
};
use rynas_api_types::{ApiError, TaskStarted, error_codes};
use rynas_auth::ipc::LOCAL_ADDR_HEADER;
use rynas_net::diag::{self, RateLimit};
use rynas_net::rescue::RESCUE_KEY;
use rynas_net::txn::{Origin, Plan, new_token};
use rynas_net::{Engine, NetError};
use rynas_notify::Notifier;

use crate::ops::{Error, Ops, Res, user};

pub struct Network {
    pub ops: Arc<Ops>,
    pub engine: Arc<Engine>,
    pub notifier: Arc<dyn Notifier>,
    /// Diagnostics: 20 runs per minute for everybody (they send packets and start tools).
    pub diag_limit: RateLimit,
    /// `--dry-system`: no rollback timer exists, so the task rolls back at the deadline.
    pub dry: bool,
    /// The hardware modules (the reset button).
    pub modules: rynas_module::Modules,
}

impl Network {
    pub fn new(ops: Arc<Ops>, engine: Arc<Engine>, notifier: Arc<dyn Notifier>, dry: bool) -> Self {
        Self {
            ops,
            engine,
            notifier,
            diag_limit: RateLimit::new(20, Duration::from_secs(60)),
            dry,
            modules: rynas_module::Modules::none(),
        }
    }

    pub fn with_modules(mut self, modules: rynas_module::Modules) -> Self {
        self.modules = modules;
        self
    }

    /// Hands the engine's queued events (also those written by `rynasctl`) to the
    /// notification engine.
    pub fn flush_events(&self) {
        for e in self.engine.take_events() {
            self.notifier.emit(e);
        }
    }
}

type NetState = Option<Arc<Network>>;

fn net(st: &NetState) -> Res<Arc<Network>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "networking is not enabled in this rynasd".into(),
        )
    })
}

impl From<NetError> for Error {
    fn from(e: NetError) -> Self {
        match e {
            NetError::Invalid(m) => {
                Error::new(StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE, m)
            }
            NetError::InvalidField(f, m) => Error::invalid(f, m),
            NetError::NotFound(m) => Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, m),
            NetError::Conflict(m) => Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, m),
            NetError::NothingPending(m) => {
                Error::new(StatusCode::CONFLICT, error_codes::NOTHING_PENDING, m)
            }
            NetError::Failed(m) | NetError::Io(m) => Error::internal(m),
        }
    }
}

/// The local address of the session's connection, from rynas-web.
fn local_ip(h: &HeaderMap) -> Option<IpAddr> {
    let v = h.get(LOCAL_ADDR_HEADER)?.to_str().ok()?;
    v.parse::<SocketAddr>()
        .map(|s| s.ip())
        .or_else(|_| v.parse::<IpAddr>())
        .ok()
}

async fn blocking<T: Send + 'static>(
    n: &Arc<Network>,
    f: impl FnOnce(&Network) -> Result<T, NetError> + Send + 'static,
) -> Res<T> {
    let n2 = n.clone();
    let r = tokio::task::spawn_blocking(move || f(&n2))
        .await
        .map_err(Error::internal)?;
    n.flush_events();
    r.map_err(Error::from)
}

fn json_body<T>(b: Result<Json<T>, JsonRejection>) -> Res<T> {
    b.map(|Json(v)| v).map_err(Error::from)
}

pub fn router(st: NetState) -> Router {
    Router::new()
        .route("/v1/network/interfaces", get(interfaces))
        .route("/v1/network/interfaces/{name}", put(stage))
        .route("/v1/network/state", get(state))
        .route("/v1/network/discard", post(discard))
        .route("/v1/network/apply", post(apply))
        .route("/v1/network/confirm", post(confirm))
        .route("/v1/network/rollback", post(rollback))
        .route("/v1/network/rescue", get(rescue_get).put(rescue_put))
        .route("/v1/network/adopt", post(adopt))
        .route("/v1/network/revisions", get(revisions))
        .route("/v1/network/revisions/{n}/restore", post(restore))
        .route("/v1/network/diagnostics/{tool}", post(diagnostics))
        // internal only (rynasctl as root): never forwarded by rynas-web
        .route("/v1/network/events/flush", post(flush))
        .with_state(st)
}

async fn interfaces(State(st): State<NetState>, h: HeaderMap) -> Res<Json<Vec<NetInterface>>> {
    let (n, owner, local) = (net(&st)?, user(&h)?, local_ip(&h));
    Ok(Json(
        blocking(&n, move |n| n.engine.interfaces(&owner, local)).await?,
    ))
}

async fn state(State(st): State<NetState>, h: HeaderMap) -> Res<Json<NetworkState>> {
    let (n, owner, local) = (net(&st)?, user(&h)?, local_ip(&h));
    Ok(Json(
        blocking(&n, move |n| n.engine.state(&owner, local)).await?,
    ))
}

async fn stage(
    State(st): State<NetState>,
    h: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<InterfaceConfig>, JsonRejection>,
) -> Res<Json<NetInterface>> {
    let (n, owner, local) = (net(&st)?, user(&h)?, local_ip(&h));
    let cfg = json_body(body)?;
    Ok(Json(
        blocking(&n, move |n| {
            n.engine.stage(&owner, &name, &cfg)?;
            n.engine.invalidate();
            n.engine
                .interfaces(&owner, local)?
                .into_iter()
                .find(|i| i.name == name)
                .ok_or_else(|| NetError::NotFound(format!("no interface {name}")))
        })
        .await?,
    ))
}

async fn discard(State(st): State<NetState>, h: HeaderMap) -> Res<StatusCode> {
    let (n, owner) = (net(&st)?, user(&h)?);
    blocking(&n, move |n| n.engine.discard(&owner)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Grace after the deadline before rynasd rolls back by itself (the timer did not fire).
const TIMER_GRACE: Duration = Duration::from_secs(20);

/// Starts the task that runs a plan and follows its trial; answers 202 with the token.
async fn start_trial(n: Arc<Network>, plan: Plan, owner: String) -> Res<Response> {
    let token = (!plan.skip_test).then(new_token);
    let kind = match plan.kind {
        TrialKind::Apply if plan.target.kind == rynas_net::store::RevKind::Restore => {
            "network.restore"
        }
        TrialKind::Apply => "network.apply",
        TrialKind::Adopt | TrialKind::AdoptAtBoot => "network.adopt",
    };
    let answer = NetApplyStarted {
        task_id: String::new(),
        token: token.clone(),
        deadline_ms: rynas_tasks::now_ms() + u64::from(plan.timeout_s) * 1000 + 2_000,
        timeout_s: plan.timeout_s,
        candidate_urls: plan.candidate_urls.clone(),
        management_touched: plan.management_touched,
    };
    let job_n = n.clone();
    let id = n.ops.tasks.spawn(kind, &owner, move |ctx| async move {
        let n = job_n;
        ctx.log(&format!(
            "interfaces: {}; timed test: {}",
            plan.interfaces.join(", "),
            if token.is_some() {
                format!("{} s", plan.timeout_s)
            } else {
                "skipped".into()
            }
        ));
        let (e, c, tid) = (n.engine.clone(), ctx.clone(), ctx.id().to_string());
        let (p, t) = (plan.clone(), token.clone());
        let begun = tokio::task::spawn_blocking(move || {
            e.begin(&p, t.as_deref(), Some(&tid), &|l| c.log(l))
        })
        .await
        .map_err(|e| e.to_string())?;
        n.flush_events();
        let begun = begun.map_err(|e| e.to_string())?;
        if token.is_none() {
            return Ok(());
        }
        ctx.log("waiting for the confirmation from the new address");
        let grace = if n.dry { Duration::ZERO } else { TIMER_GRACE };
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let e = n.engine.clone();
            let marker = tokio::task::spawn_blocking(move || e.trial())
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            let still = marker
                .as_ref()
                .is_some_and(|m| m.revision == begun.revision);
            if !still {
                break;
            }
            let late = rynas_tasks::now_ms() > begun.deadline_ms + grace.as_millis() as u64;
            if late || ctx.is_cancelled() {
                let reason = if late {
                    rynas_net::reason::TIMEOUT
                } else {
                    rynas_net::reason::MANUAL
                };
                if late && !n.dry {
                    ctx.log("the rollback timer did not fire: rolling back from rynasd");
                }
                let (e, c) = (n.engine.clone(), ctx.clone());
                let _ =
                    tokio::task::spawn_blocking(move || e.rollback(reason, &|l| c.log(l))).await;
                n.flush_events();
                break;
            }
        }
        let e = n.engine.clone();
        let lkg = tokio::task::spawn_blocking(move || e.store.state())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?
            .lkg;
        if lkg == Some(begun.revision) {
            ctx.log(&format!(
                "confirmed: revision {} is the last known good",
                begun.revision
            ));
            Ok(())
        } else {
            let e = n.engine.clone();
            let report = tokio::task::spawn_blocking(move || e.store.last_rollback())
                .await
                .ok()
                .and_then(|r| r.ok())
                .flatten()
                .map(|r| r.report);
            Err(match report {
                Some(r) => format!(
                    "not confirmed: revision {} restored ({}){}",
                    r.restored_revision,
                    r.reason,
                    if r.verified {
                        ", verified"
                    } else {
                        ", NOT verified"
                    }
                ),
                None => "not confirmed: rolled back".into(),
            })
        }
    })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(NetApplyStarted {
            task_id: id,
            ..answer
        }),
    )
        .into_response())
}

async fn apply(
    State(st): State<NetState>,
    h: HeaderMap,
    body: Result<Json<NetApplyRequest>, JsonRejection>,
) -> Res<Response> {
    let (n, owner, local) = (net(&st)?, user(&h)?, local_ip(&h));
    let req = json_body(body)?;
    let o = owner.clone();
    let plan = blocking(&n, move |n| {
        n.engine
            .plan_apply(&o, &o, &req.comment, local, req.timeout_s, req.skip_test)
    })
    .await?;
    start_trial(n, plan, owner).await
}

async fn confirm(
    State(st): State<NetState>,
    h: HeaderMap,
    body: Result<Json<NetConfirmRequest>, JsonRejection>,
) -> Res<Json<NetConfirmed>> {
    let (n, local) = (net(&st)?, local_ip(&h));
    user(&h)?;
    let req = json_body(body)?;
    let revision = blocking(&n, move |n| {
        n.engine.confirm(&req.token, Origin::Web(local))
    })
    .await?;
    Ok(Json(NetConfirmed { revision }))
}

async fn rollback(State(st): State<NetState>, h: HeaderMap) -> Res<Response> {
    let (n, owner) = (net(&st)?, user(&h)?);
    let pending = blocking(&n, |n| {
        Ok(n.engine.trial()?.is_some() || n.engine.store.boot_adopt()?.is_some())
    })
    .await?;
    if !pending {
        return Err(NetError::NothingPending("no network change to roll back".into()).into());
    }
    let job_n = n.clone();
    let id = n
        .ops
        .tasks
        .spawn("network.rollback", &owner, move |ctx| async move {
            let (e, c) = (job_n.engine.clone(), ctx.clone());
            let r = tokio::task::spawn_blocking(move || {
                e.rollback(rynas_net::reason::MANUAL, &|l| c.log(l))
            })
            .await
            .map_err(|e| e.to_string())?;
            job_n.flush_events();
            match r.map_err(|e| e.to_string())? {
                Some(rep) if !rep.verified => Err(format!(
                    "revision {} restored but not verified: {}",
                    rep.restored_revision,
                    rep.problems.join("; ")
                )),
                Some(rep) => {
                    ctx.log(&format!(
                        "revision {} restored and verified",
                        rep.restored_revision
                    ));
                    Ok(())
                }
                None => Ok(()),
            }
        })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

fn rescue_info(n: &Network, owner: &str) -> Res<RescueInfo> {
    let c = &n.ops.config;
    let (eff, cur, _): (RescueSettings, RescueSettings, Option<RescueSettings>) =
        crate::services::docs(c, owner, RESCUE_KEY)?;
    // read-only reflection of the hardware module's setting
    let reset_button = n.modules.reset_button(c, owner).map_err(Error::internal)?;
    let inv = n.engine.inventory()?;
    Ok(RescueInfo {
        mdns_name: format!("{}.local", n.engine.hostname()),
        mdns_active: inv.services.avahi,
        link_local: n.engine.link_locals()?,
        reset_button,
        pending: (eff != cur).then_some(eff),
        settings: cur,
    })
}

async fn rescue_get(State(st): State<NetState>, h: HeaderMap) -> Res<Json<RescueInfo>> {
    let (n, owner) = (net(&st)?, user(&h)?);
    let n2 = n.clone();
    Ok(Json(
        tokio::task::spawn_blocking(move || rescue_info(&n2, &owner))
            .await
            .map_err(Error::internal)??,
    ))
}

async fn rescue_put(
    State(st): State<NetState>,
    h: HeaderMap,
    body: Result<Json<serde_json::Value>, JsonRejection>,
) -> Res<Json<RescueInfo>> {
    let (n, owner) = (net(&st)?, user(&h)?);
    let v = json_body(body)?;
    if v.get("reset_button").is_some() {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            error_codes::INVALID_VALUE,
            "the reset button is not a rescue setting: it belongs to the hardware settings of \
             this machine"
                .into(),
        ));
    }
    let s: RescueSettings = rynas_http::from_value(v).map_err(Error::from)?;
    let n2 = n.clone();
    Ok(Json(
        tokio::task::spawn_blocking(move || {
            crate::services::stage(&n2.ops.config, &owner, RESCUE_KEY, &s)?;
            rescue_info(&n2, &owner)
        })
        .await
        .map_err(Error::internal)??,
    ))
}

async fn adopt(
    State(st): State<NetState>,
    h: HeaderMap,
    body: Result<Json<NetAdoptRequest>, JsonRejection>,
) -> Res<Response> {
    let (n, owner, local) = (net(&st)?, user(&h)?, local_ip(&h));
    let req = json_body(body)?;
    if !req.confirm {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            error_codes::CONFIRMATION_REQUIRED,
            "the migration changes how the whole network is configured: send confirm: true".into(),
        ));
    }
    let o = owner.clone();
    match req.mode {
        AdoptMode::Now => {
            let plan = blocking(&n, move |n| n.engine.plan_adopt(&o, local, req.timeout_s)).await?;
            start_trial(n, plan, owner).await
        }
        AdoptMode::NextBoot => {
            let (revision, interfaces) =
                blocking(&n, move |n| n.engine.adopt_next_boot(&o)).await?;
            Ok(Json(NetAdoptScheduled {
                revision,
                interfaces,
            })
            .into_response())
        }
    }
}

async fn revisions(State(st): State<NetState>, h: HeaderMap) -> Res<Json<Vec<NetRevision>>> {
    let n = net(&st)?;
    user(&h)?;
    Ok(Json(blocking(&n, |n| n.engine.revisions()).await?))
}

async fn restore(
    State(st): State<NetState>,
    h: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NetRestoreRequest>, JsonRejection>,
) -> Res<Response> {
    let (n, owner, local) = (net(&st)?, user(&h)?, local_ip(&h));
    let id: u64 = id
        .parse()
        .map_err(|_| Error::bad_request("the revision must be a number"))?;
    let req = json_body(body)?;
    let o = owner.clone();
    let plan = blocking(&n, move |n| {
        n.engine.plan_restore(id, &o, local, req.timeout_s)
    })
    .await?;
    start_trial(n, plan, owner).await
}

async fn diagnostics(
    State(st): State<NetState>,
    h: HeaderMap,
    Path(tool): Path<String>,
    body: axum::body::Bytes,
) -> Res<Response> {
    let n = net(&st)?;
    user(&h)?;
    let parse = || -> Res<serde_json::Value> { rynas_http::from_slice(&body).map_err(Error::from) };
    let bad = |m: String| Error::new(StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE, m);
    let resolved = n
        .engine
        .inventory()
        .map(|i| i.services.resolved)
        .unwrap_or(false);
    let os = n.engine.os.clone();
    let (argv, timeout) = match tool.as_str() {
        "ping" => {
            let r: PingRequest = rynas_http::from_value(parse()?).map_err(Error::from)?;
            let secs = u64::from(r.count.unwrap_or(4)) * 2 + 4;
            (diag::ping_argv(&r).map_err(bad)?, Duration::from_secs(secs))
        }
        "traceroute" => {
            let r: TracerouteRequest = rynas_http::from_value(parse()?).map_err(Error::from)?;
            (
                diag::traceroute_argv(os.as_ref(), &r).map_err(bad)?,
                Duration::from_secs(45),
            )
        }
        "dns" => {
            let r: DnsLookupRequest = rynas_http::from_value(parse()?).map_err(Error::from)?;
            (
                diag::dns_argv(os.as_ref(), resolved, &r).map_err(bad)?,
                Duration::from_secs(10),
            )
        }
        _ => return Err(Error::not_found("no such diagnostic")),
    };
    if let Err(wait) = n.diag_limit.check(Instant::now()) {
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", wait.as_secs().max(1).to_string())],
            Json(ApiError::new(
                error_codes::RATE_LIMITED,
                "too many diagnostics: try again shortly",
            )),
        )
            .into_response());
    }
    let result: DiagnosticResult =
        tokio::task::spawn_blocking(move || diag::run(os.as_ref(), &argv, timeout))
            .await
            .map_err(Error::internal)?;
    Ok(Json(result).into_response())
}

async fn flush(State(st): State<NetState>) -> Res<StatusCode> {
    let n = net(&st)?;
    let n2 = n.clone();
    tokio::task::spawn_blocking(move || n2.flush_events())
        .await
        .map_err(Error::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// At start and then every minute: keeps `/etc/resolv.conf` in step with networkd's DNS
/// servers while rynas runs the network (repairing a file left without servers, e.g. by an
/// older rynas that migrated from ifupdown), then collects the events `rynasctl` queued while
/// rynasd was not running (boot guard, watchdog).
pub fn spawn_event_flush(n: Arc<Network>) {
    tokio::spawn(async move {
        loop {
            let n2 = n.clone();
            let _ = tokio::task::spawn_blocking(move || {
                n2.engine.sync_resolv(&|l| tracing::info!("network: {l}"));
                n2.flush_events()
            })
            .await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}
