//! Endpoints passed through to `rynasd` unchanged: configuration, tasks, users and shares,
//! the storage views rynas-web has nothing to add to, and the routes of the hardware modules
//! (`/modules/<id>/…`, forwarded by prefix: rynas-web knows no module; rynasd has the routes
//! only where the module is detected and answers 404 elsewhere).
//!
//! The session and CSRF checks have already run ([`crate::auth::require_session`]); the
//! request goes on through [`RynasdClient::send`](crate::rynasd_client::RynasdClient::send)
//! with the session user, and the answer comes back as is (status, JSON or text body, task
//! log headers). SSE streams end with their session, like the metrics stream.

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post, put};
use axum::{Extension, Router};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use rynas_api_types::{TASK_LOG_COMPLETE, TASK_LOG_NEXT_OFFSET, error_codes};

use crate::app::AppState;
use crate::auth::CurrentSession;
use crate::conn::{Client, LocalAddr};
use crate::error::Error;
use rynas_auth::ipc::{CLIENT_ADDR_HEADER, HOST_HEADER, LOCAL_ADDR_HEADER};
use rynas_auth::session::SessionData;

/// Largest request body forwarded (guidelines M3).
const MAX_BODY: usize = 1 << 20;

/// The forwarded routes (relative to `/api/v1`). Listed one by one so that unknown paths
/// keep the 401/404 behaviour of the rest of the API.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/config/pending", get(forward))
        .route("/config/apply", post(forward))
        .route("/config/discard", post(forward))
        .route("/config/revisions", get(forward))
        .route("/config/revisions/{n}", get(forward))
        .route("/config/revisions/{n}/restore", post(forward))
        .route("/config/drift", get(forward))
        .route("/config/drift/{key}/import", post(forward))
        .route("/config/drift/{key}/overwrite", post(forward))
        .route("/system/hostname", get(forward).put(forward))
        // HTTPS certificate of the web interface (W0): details, local CA download, import,
        // back to the local certificate; the served pair is reloaded right after a change
        .route("/system/tls", get(forward))
        .route("/system/tls/ca", get(forward))
        .route("/system/tls/import", post(forward_tls))
        .route("/system/tls/local", post(forward_tls))
        .route("/tasks", get(forward))
        .route("/tasks/stream", get(forward))
        .route("/tasks/{id}", get(forward))
        .route("/tasks/{id}/log", get(forward))
        .route("/tasks/{id}/cancel", post(forward))
        // storage views and plans (disks are enriched with bays: crate::storage_api)
        .route("/storage/arrays", get(forward))
        .route("/storage/filesystems", get(forward))
        .route("/storage/tree", get(forward))
        .route("/storage/plan/volume", post(forward))
        .route("/storage/plan/grow", post(forward))
        .route("/storage/plan/replace", post(forward))
        .route("/storage/plan/scrub", post(forward))
        // storage write side: plans executed as tasks, LUKS, subvolumes, settings
        .route("/storage/volumes", post(forward))
        .route("/storage/volumes/{name}/grow", post(forward))
        .route("/storage/volumes/{name}/replace", post(forward))
        .route("/storage/volumes/{name}/scrub", post(forward))
        .route("/storage/volumes/{name}/unlock", post(forward))
        .route("/storage/volumes/{name}/lock", post(forward))
        .route("/storage/volumes/{name}/reveal-recovery-key", post(forward))
        .route(
            "/storage/volumes/{name}/luks-header",
            get(forward).delete(forward),
        )
        .route(
            "/storage/volumes/{name}/settings",
            get(forward).put(forward),
        )
        .route(
            "/storage/volumes/{name}/subvolumes",
            get(forward).post(forward),
        )
        .route(
            "/storage/volumes/{name}/subvolumes/{sub}",
            put(forward).delete(forward),
        )
        .route("/storage/disks/{name}/settings", get(forward).put(forward))
        // users, groups, shares, SMB and NFS (step 8)
        .route("/users", get(forward).post(forward))
        .route("/users/{name}", get(forward).put(forward).delete(forward))
        .route("/users/{name}/password", post(forward))
        .route("/groups", get(forward).post(forward))
        .route("/groups/{name}", get(forward).put(forward).delete(forward))
        .route("/services/smb", get(forward).put(forward))
        .route("/services/smb/recycle-cleanup", post(forward))
        .route("/services/nfs", get(forward).put(forward))
        // user homes: setting, home moves (plan, task, confirm, undo)
        .route("/services/homes", get(forward).put(forward))
        .route("/services/homes/move/plan", post(forward))
        .route("/services/homes/move", post(forward))
        .route("/services/homes/move/{name}/finish", post(forward))
        .route("/services/homes/move/{name}/undo", post(forward))
        .route("/shares", get(forward).post(forward))
        .route("/shares/{name}", get(forward).put(forward).delete(forward))
        .route("/shares/{name}/effective", get(forward))
        .route("/shares/{name}/nocow", post(forward))
        .route("/shares/plan/delete", post(forward))
        .route("/folders", get(forward))
        .route("/folders/plan/delete", post(forward))
        .route("/folders/{volume}/{name}", delete(forward))
        // notifications (P3): list, ack, silences, targets, rules; live changes come on
        // /events as `notification`
        .route("/notifications", get(forward))
        .route("/notifications/ack-all", post(forward))
        .route("/notifications/silence", post(forward))
        .route("/notifications/silences", get(forward))
        .route("/notifications/silences/{id}", delete(forward))
        .route("/notifications/targets", get(forward).post(forward))
        .route(
            "/notifications/targets/{id}",
            get(forward).put(forward).delete(forward),
        )
        .route("/notifications/targets/{id}/test", post(forward))
        .route("/notifications/rules", get(forward).post(forward))
        .route(
            "/notifications/rules/{id}",
            get(forward).put(forward).delete(forward),
        )
        .route("/notifications/{id}/ack", post(forward))
        // task scheduler and snapshots (P3)
        .route("/scheduler/tasks", get(forward).post(forward))
        .route("/scheduler/tasks/preview-calendar", post(forward))
        .route(
            "/scheduler/tasks/{id}",
            get(forward).put(forward).delete(forward),
        )
        .route("/scheduler/tasks/{id}/run", post(forward))
        .route("/scheduler/tasks/{id}/enable", post(forward))
        .route("/scheduler/tasks/{id}/disable", post(forward))
        .route("/scheduler/tasks/{id}/history", get(forward))
        .route("/scheduler/tasks/{id}/history/{n}", get(forward))
        .route("/scheduler/system-timers", get(forward))
        .route("/scheduler/cron-entries", get(forward))
        .route("/snapshots", get(forward))
        .route("/snapshots/system", get(forward).put(forward))
        .route("/snapshots/jobs", get(forward).post(forward))
        .route("/snapshots/jobs/preview-retention", post(forward))
        .route(
            "/snapshots/jobs/{name}",
            get(forward).put(forward).delete(forward),
        )
        .route("/snapshots/{id}", delete(forward))
        .route("/snapshots/{id}/browse", get(forward))
        .route("/snapshots/{id}/restore", post(forward))
        .route("/snapshots/{id}/rollback", post(forward))
        .route("/snapshots/{id}/preview-rollback", get(forward))
        // hardware modules: every route of a module, by prefix; the provider-neutral bay
        // identify; the power schedule (P4)
        .route("/modules/{id}/{*rest}", any(forward))
        .route("/hardware/bays/{n}/identify", post(forward))
        .route("/power/schedule", get(forward).put(forward))
        .route("/power/next", get(forward))
        // UPS (P6): the shutdown policy and the battery test; the status itself is
        // read from upsd by rynas-web (`/ups`, `ups.status` events)
        .route("/ups/settings", get(forward).put(forward))
        .route("/ups/test", post(forward))
        // UPS setup wizard: state, USB detection task, drivers, plan of an apply
        .route("/ups/device", get(forward).put(forward))
        .route("/ups/detect", post(forward))
        .route("/ups/drivers", get(forward))
        .route("/ups/plan/device", post(forward))
        .route("/ups/netserver", get(forward).put(forward))
        .route("/ups/plan/netserver", post(forward))
        // system updates (P6): apt list and tasks, automatic updates, kernel / DOM
        .route("/updates", get(forward))
        .route("/updates/check", post(forward))
        .route("/updates/install", post(forward))
        .route("/updates/autoremove", post(forward))
        .route("/updates/packages/{name}/changelog", post(forward))
        .route("/updates/auto", get(forward).put(forward))
        .route("/updates/kernel", get(forward))
        // removable drives: state, actions as tasks, settings; changes come on /events
        // as `removable.state`
        .route("/removable", get(forward))
        .route("/removable/refresh", post(forward))
        .route("/removable/mounts", get(forward))
        .route("/removable/settings", get(forward).put(forward))
        .route("/removable/mount-all", post(forward))
        .route("/removable/remove-all", post(forward))
        .route("/removable/volumes/{name}/mount", post(forward))
        .route("/removable/volumes/{name}/unmount", post(forward))
        .route("/removable/volumes/{name}/unlock", post(forward))
        .route("/removable/volumes/{name}/lock", post(forward))
        .route("/removable/volumes/{name}/holders", get(forward))
        .route("/removable/drives/{name}/remove", post(forward))
        // Docker (phase 2, D1): engine status, install and settings, listings, actions as
        // tasks, logs (JSON and SSE), stacks on disk, snapshots, update check; live samples
        // and change hints come on /events as `docker.stats` / `docker.changed`
        .route("/docker/status", get(forward))
        .route("/docker/settings", get(forward).put(forward))
        .route("/docker/plan/engine", post(forward))
        .route("/docker/install", post(forward))
        .route("/docker/engine/migrate", post(forward))
        .route("/docker/containers", get(forward))
        .route("/docker/containers/{id}/logs", get(forward))
        .route("/docker/containers/{id}/logs/stream", get(forward))
        .route("/docker/containers/{id}/{action}", post(forward))
        .route("/docker/stacks", get(forward).post(forward))
        .route("/docker/stacks/validate", post(forward))
        // adoption (§5.7): preview GET, adopt POST; import and its preview
        .route("/docker/stacks/import", post(forward))
        .route("/docker/stacks/import/preview", post(forward))
        .route("/docker/stacks/{name}/adopt", get(forward).post(forward))
        .route("/docker/stacks/{name}", get(forward))
        .route("/docker/stacks/{name}/files", get(forward).put(forward))
        .route("/docker/stacks/{name}/snapshots", get(forward).post(forward))
        .route("/docker/stacks/{name}/snapshots/{id}", delete(forward))
        .route("/docker/stacks/{name}/{action}", post(forward))
        .route("/docker/images", get(forward))
        .route("/docker/volumes", get(forward))
        .route("/docker/networks", get(forward))
        .route("/docker/df", get(forward))
        .route("/docker/stats", get(forward))
        .route("/docker/updates", get(forward))
        .route("/docker/updates/check", post(forward))
        // app catalogue (phase 2, A2): cards, entry detail and images, install plan and
        // install, installed apps, update plan, update, rollback, convert, uninstall
        .route("/apps/catalog", get(forward))
        .route("/apps/catalog/{id}", get(forward))
        .route("/apps/catalog/{id}/icon", get(forward))
        .route("/apps/catalog/{id}/screenshots/{file}", get(forward))
        .route("/apps/catalog/{id}/plan", post(forward))
        .route("/apps/catalog/{id}/install", post(forward))
        .route("/apps/installed", get(forward))
        .route("/apps/installed/{stack}", get(forward))
        .route("/apps/installed/{stack}/update", get(forward).post(forward))
        .route("/apps/installed/{stack}/{action}", post(forward))
        // web access (phase 2, W1): state and staging of the `web_access` key, the engine
        // configuration an apply would load, anti-drift now, route diagnostics, and the
        // timed confirmation of the interface's own route (with the Host and the local
        // address: rynasd accepts it only through the proxy on a new name)
        .route("/web-access", get(forward).put(forward))
        .route("/web-access/plan", post(forward))
        .route("/web-access/sync", post(forward))
        .route("/web-access/confirm", post(forward_host))
        .route("/web-access/routes/{id}/test", post(forward))
        // W2: domains, certificate dashboard, providers
        .route("/web-access/domains/{id}/test", post(forward))
        .route("/web-access/domains/{id}/records", post(forward))
        .route("/web-access/certificates", get(forward))
        .route("/web-access/providers", get(forward))
        // imported certificates (§5.5)
        .route("/web-access/imports", get(forward).post(forward))
        .route("/web-access/imports/inspect", post(forward))
        .route("/web-access/imports/{id}/check", post(forward))
        .route("/web-access/imports/{id}/accept", post(forward))
        .route("/web-access/imports/{id}", delete(forward))
        // Tailscale (remote access): status, install/uninstall, login/logout, options with
        // their timed confirmation; with the local address of the connection (rynasd tells
        // whether the session itself comes over the tailnet)
        .route("/tailscale", get(forward_client))
        .route("/tailscale/install", post(forward_client))
        .route("/tailscale/uninstall", post(forward_client))
        .route("/tailscale/login", post(forward_client))
        .route("/tailscale/logout", post(forward_client))
        .route("/tailscale/options", put(forward_client))
        .route("/tailscale/options/confirm", post(forward_client))
        // network (P4): with the local address of the session's connection
        .route("/network/interfaces", get(forward_net))
        .route("/network/interfaces/{name}", put(forward_net))
        .route("/network/state", get(forward_net))
        .route("/network/discard", post(forward_net))
        .route("/network/apply", post(forward_net))
        .route("/network/confirm", post(forward_net))
        .route("/network/rollback", post(forward_net))
        .route("/network/rescue", get(forward_net).put(forward_net))
        .route("/network/adopt", post(forward_net))
        .route("/network/revisions", get(forward_net))
        .route("/network/revisions/{n}/restore", post(forward_net))
        .route("/network/diagnostics/{tool}", post(forward_net))
}

/// Headers of rynasd's answer that reach the client.
fn passed_headers() -> [HeaderName; 9] {
    debug_assert!(TASK_LOG_NEXT_OFFSET.eq_ignore_ascii_case("x-rynas-next-offset"));
    debug_assert!(TASK_LOG_COMPLETE.eq_ignore_ascii_case("x-rynas-log-complete"));
    [
        header::CONTENT_TYPE,
        // downloads (the LUKS header backup)
        header::CONTENT_DISPOSITION,
        header::RETRY_AFTER,
        header::LOCATION,
        // conditional reads (`/config/pending`)
        header::ETAG,
        // rynasd's own policy wins over the app-wide defaults (those layers are
        // `if_not_present`): the sandboxing CSP and cache lifetime of app icons and
        // screenshots (docs/phase2/docker-apps.md §5.2)
        header::CONTENT_SECURITY_POLICY,
        header::CACHE_CONTROL,
        HeaderName::from_static("x-rynas-next-offset"),
        HeaderName::from_static("x-rynas-log-complete"),
    ]
}

async fn forward(
    State(st): State<AppState>,
    Extension(CurrentSession(session)): Extension<CurrentSession>,
    req: Request,
) -> Response {
    forward_with(st, session, req, Vec::new()).await
}

/// A certificate change: forwarded, then the published pair is loaded at once instead of at
/// the next file check.
async fn forward_tls(
    State(st): State<AppState>,
    Extension(CurrentSession(session)): Extension<CurrentSession>,
    req: Request,
) -> Response {
    let tls = st.tls.clone();
    let r = forward_with(st, session, req, Vec::new()).await;
    if r.status().is_success()
        && let Some(t) = tls
    {
        match tokio::task::spawn_blocking(move || t.reload_now()).await {
            Ok(Ok(_)) => tracing::info!("https: certificate reloaded after a change"),
            Ok(Err(e)) => tracing::warn!("https: certificate not reloaded: {e}"),
            Err(e) => tracing::warn!("https: {e}"),
        }
    }
    r
}

/// The network endpoints also get the local address of the connection: rynasd derives the
/// management interface from it and checks where a confirmation arrived.
async fn forward_net(
    State(st): State<AppState>,
    Extension(CurrentSession(session)): Extension<CurrentSession>,
    local: Option<Extension<LocalAddr>>,
    req: Request,
) -> Response {
    let extra = local
        .map(|Extension(LocalAddr(a))| vec![(LOCAL_ADDR_HEADER, a.to_string())])
        .unwrap_or_default();
    forward_with(st, session, req, extra).await
}

/// The Tailscale endpoints: the local address of the connection and the client's address
/// (behind rynas-caddy the local address is loopback, the client the forwarded one), so
/// rynasd knows whether the session came over the tailnet.
async fn forward_client(
    State(st): State<AppState>,
    Extension(CurrentSession(session)): Extension<CurrentSession>,
    local: Option<Extension<LocalAddr>>,
    client: Option<Extension<Client>>,
    req: Request,
) -> Response {
    let mut extra = local
        .map(|Extension(LocalAddr(a))| vec![(LOCAL_ADDR_HEADER, a.to_string())])
        .unwrap_or_default();
    if let Some(Extension(c)) = client {
        extra.push((CLIENT_ADDR_HEADER, c.ip.to_string()));
    }
    forward_with(st, session, req, extra).await
}

/// The web access confirmation: the local address of the connection (rynas-web's loopback
/// port when the request came through the proxy) and the `Host` the browser asked for.
async fn forward_host(
    State(st): State<AppState>,
    Extension(CurrentSession(session)): Extension<CurrentSession>,
    local: Option<Extension<LocalAddr>>,
    req: Request,
) -> Response {
    let mut extra = local
        .map(|Extension(LocalAddr(a))| vec![(LOCAL_ADDR_HEADER, a.to_string())])
        .unwrap_or_default();
    if let Some(h) = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()) {
        extra.push((HOST_HEADER, h.to_string()));
    }
    forward_with(st, session, req, extra).await
}

async fn forward_with(
    st: AppState,
    session: SessionData,
    req: Request,
    mut extra: Vec<(&'static str, String)>,
) -> Response {
    let (parts, body) = req.into_parts();
    // conditional reads: rynasd answers 304 when the entity tag still matches
    if let Some(v) = parts
        .headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        extra.push(("if-none-match", v.to_string()));
    }
    // inside the /api/v1 nest the path is relative: /config/pending -> /v1/config/pending
    let target = match parts.uri.query() {
        Some(q) => format!("/v1{}?{q}", parts.uri.path()),
        None => format!("/v1{}", parts.uri.path()),
    };
    let body = match to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            return Error::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                error_codes::PAYLOAD_TOO_LARGE,
                format!("request body over {MAX_BODY} bytes"),
            )
            .into_response();
        }
    };
    let content_type = parts.headers.get(header::CONTENT_TYPE).cloned();
    let resp = match st
        .rynasd
        .send_with_headers(
            parts.method,
            &target,
            Some(&session.user.username),
            content_type,
            body,
            &extra,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return Error::from(e).into_response(),
    };
    let (rparts, incoming) = resp.into_parts();
    let is_sse = rparts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let body = if is_sse {
        let auth = st.auth.clone();
        let id = session.id;
        let data = incoming
            .into_data_stream()
            .take_while(move |_| std::future::ready(auth.sessions().is_valid(&id)));
        // counted like /events (crate::streams); over the global cap the upstream is dropped
        match st.streams.limit(id, data) {
            Ok(limited) => Body::from_stream(limited),
            Err(full) => return full.into_response(),
        }
    } else {
        Body::new(incoming)
    };
    let mut out = Response::new(body);
    *out.status_mut() = rparts.status;
    for name in passed_headers() {
        if let Some(v) = rparts.headers.get(&name) {
            out.headers_mut().insert(name, v.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rynasd_csp_and_cache_control_reach_the_client() {
        let h = passed_headers();
        assert!(h.contains(&header::CONTENT_SECURITY_POLICY));
        assert!(h.contains(&header::CACHE_CONTROL));
    }
}
