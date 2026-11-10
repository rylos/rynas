//! The SSH service (D-SSH, `docs/phase2/ssh.md`), administrators only:
//!
//! - `GET /v1/services/ssh`: settings (read from the drop-in, or from the running sshd before
//!   the first save), ports, host key fingerprints, accounts with their level and key count,
//!   problems, the change under test (its token only for the administrator who made it);
//! - `PUT /v1/services/ssh` ([`SshApply`]): applies; a change that can shut someone out
//!   answers with the trial, undone unless `POST /v1/services/ssh/confirm` comes in time
//!   (`POST …/rollback` undoes it now);
//! - `GET|POST /v1/services/ssh/keys/{user}`, `POST …/keys/{user}/remove`: the account's
//!   `authorized_keys`;
//! - `GET /v1/services/ssh/logins`: logins and failures of the last day (journal).
//!
//! The engine is [`rynas_services::ssh::Ssh`]; an expired trial is also undone here every few
//! seconds (besides `rynas-ssh-rollback.timer`), so a restart of rynasd never leaves one.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::error_codes;
use rynas_api_types::ssh::*;
use rynas_services::ssh::{Ssh, SshError};

use crate::ops::{Error, Res, user};
use crate::roles::AdminPolicy;

pub struct SshSvc {
    pub ssh: Arc<Ssh>,
    pub admins: AdminPolicy,
}

pub type SshHandle = Option<Arc<SshSvc>>;

fn svc(st: &SshHandle) -> Res<Arc<SshSvc>> {
    st.clone().ok_or_else(|| {
        Error::new(StatusCode::SERVICE_UNAVAILABLE, error_codes::BACKEND_UNAVAILABLE, "SSH is not available".into())
    })
}

impl From<SshError> for Error {
    fn from(e: SshError) -> Self {
        match e {
            SshError::Invalid(f, m) => Error::invalid(f, m),
            SshError::Lockout(m) => Error::new(StatusCode::CONFLICT, error_codes::SSH_LOCKOUT, m),
            SshError::Conflict(m) => Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, m),
            SshError::NotFound(m) => Error::not_found(m),
            SshError::Failed(m) => Error::internal(m),
        }
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Res<T> {
    tokio::task::spawn_blocking(f).await.map_err(Error::internal)
}

pub fn router(st: SshHandle) -> Router {
    let admins = st.as_ref().map(|s| s.admins.clone()).unwrap_or_default();
    Router::new()
        .route("/v1/services/ssh", get(state_get).put(apply_put))
        .route("/v1/services/ssh/confirm", post(confirm_post))
        .route("/v1/services/ssh/rollback", post(rollback_post))
        .route("/v1/services/ssh/logins", get(logins_get))
        .route("/v1/services/ssh/keys/{user}", get(keys_get).post(key_add))
        .route("/v1/services/ssh/keys/{user}/remove", post(key_remove))
        .with_state(st)
        .route_layer(axum::middleware::from_fn_with_state(
            crate::roles::AdminGate { policy: admins, what: "the SSH service" },
            crate::roles::admin_only,
        ))
}

async fn state_get(State(st): State<SshHandle>, hd: HeaderMap) -> Res<Json<SshState>> {
    let (s, who) = (svc(&st)?, user(&hd)?);
    let mut state = blocking(move || s.ssh.state()).await?;
    if let Some(t) = state.trial.as_mut()
        && t.owner != who
    {
        t.token.clear();
    }
    Ok(Json(state))
}

async fn apply_put(
    State(st): State<SshHandle>,
    hd: HeaderMap,
    body: Result<Json<SshApply>, JsonRejection>,
) -> Res<Json<SshApplied>> {
    let (s, who) = (svc(&st)?, user(&hd)?);
    let Json(req) = body?;
    let x = &req.settings;
    tracing::info!(
        target: "rynas::audit",
        "ssh apply enabled={} port={} password={} root={:?} allow={:?} groups={:?} tcp={} by {who}",
        x.enabled, x.port, x.password_auth, x.root_login, x.allow, x.allow_groups, x.tcp_forwarding
    );
    let trial = blocking(move || s.ssh.apply(req.settings, req.timeout_s, &who)).await??;
    Ok(Json(SshApplied { trial }))
}

async fn confirm_post(
    State(st): State<SshHandle>,
    hd: HeaderMap,
    body: Result<Json<SshConfirm>, JsonRejection>,
) -> Res<StatusCode> {
    let (s, who) = (svc(&st)?, user(&hd)?);
    let Json(req) = body?;
    blocking(move || s.ssh.confirm(&req.token)).await??;
    tracing::info!(target: "rynas::audit", "ssh change confirmed by {who}");
    Ok(StatusCode::NO_CONTENT)
}

async fn rollback_post(State(st): State<SshHandle>, hd: HeaderMap) -> Res<StatusCode> {
    let (s, who) = (svc(&st)?, user(&hd)?);
    let undone = blocking(move || s.ssh.rollback("manual")).await??;
    if !undone {
        return Err(Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, "no SSH change waits for a confirmation".into()));
    }
    tracing::info!(target: "rynas::audit", "ssh change undone by {who}");
    Ok(StatusCode::NO_CONTENT)
}

async fn logins_get(State(st): State<SshHandle>) -> Res<Json<SshLogins>> {
    let s = svc(&st)?;
    Ok(Json(blocking(move || s.ssh.logins()).await?))
}

async fn keys_get(State(st): State<SshHandle>, UrlPath(name): UrlPath<String>) -> Res<Json<SshKeyList>> {
    let s = svc(&st)?;
    Ok(Json(blocking(move || s.ssh.keys(&name)).await??))
}

async fn key_add(
    State(st): State<SshHandle>,
    hd: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<SshKeyAdd>, JsonRejection>,
) -> Res<(StatusCode, Json<SshKey>)> {
    let (s, who) = (svc(&st)?, user(&hd)?);
    let Json(req) = body?;
    let n = name.clone();
    let k = blocking(move || s.ssh.add_key(&n, &req.key)).await??;
    tracing::info!(target: "rynas::audit", "ssh key {} added to {name} by {who}", k.fingerprint);
    Ok((StatusCode::CREATED, Json(k)))
}

async fn key_remove(
    State(st): State<SshHandle>,
    hd: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<SshKeyRemove>, JsonRejection>,
) -> Res<StatusCode> {
    let (s, who) = (svc(&st)?, user(&hd)?);
    let Json(req) = body?;
    let (n, fp) = (name.clone(), req.fingerprint.clone());
    blocking(move || s.ssh.remove_key(&n, &fp)).await??;
    tracing::info!(target: "rynas::audit", "ssh key {} removed from {name} by {who}", req.fingerprint);
    Ok(StatusCode::NO_CONTENT)
}

/// Undoes an expired trial: at start, then every 5 s.
pub fn spawn_watch(s: Arc<SshSvc>) {
    tokio::spawn(async move {
        loop {
            let ssh = s.ssh.clone();
            let r = tokio::task::spawn_blocking(move || ssh.expire(rynas_tasks::now_ms())).await;
            match r {
                Ok(Ok(true)) => tracing::warn!("ssh: an unconfirmed change was undone"),
                Ok(Err(e)) => tracing::error!("ssh: undoing an unconfirmed change: {e}"),
                _ => {}
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use rynas_api_types::ssh::{SSH_BASE_GROUPS, SshAllow, SshSettings};
    use rynas_btrfs::receive;
    use rynas_services::ssh::config;

    /// The accounts of features that receive over SSH stay allowed when only listed accounts
    /// may log in: the PC backup setup check reads the same sshd lines.
    #[test]
    fn listed_accounts_cover_the_receiving_features() {
        assert!(SSH_BASE_GROUPS.contains(&rynas_api_types::pcbackup::ACCOUNT));
        assert!(SSH_BASE_GROUPS.contains(&crate::web_access::imported::DROP_GROUP));
        let s = SshSettings { allow: SshAllow::Listed, allow_groups: vec!["sudo".into()], ..SshSettings::default() };
        let dropin = config::render(&s);
        let acct = rynas_api_types::pcbackup::ACCOUNT;
        assert!(receive::sshd_allows(&dropin, acct, &[acct]));
        assert!(!receive::sshd_allows(&dropin, "someone", &["users"]));
        assert_eq!(receive::sshd_port(&dropin), None);
        let s = SshSettings { port: 2222, ..s };
        assert_eq!(receive::sshd_port(&config::render(&s)), Some(2222));
    }
}
