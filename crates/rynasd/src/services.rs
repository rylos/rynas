//! Users, groups, shares, SMB and NFS endpoints of the internal API.
//!
//! Every write stages a pending change of the session user in the `users` or `shares`
//! configuration key (see [`rynas_services`]); `POST /v1/config/apply` applies it. Reads
//! show the configuration as that user sees it (its staged value if any) next to the
//! system state. The only immediate action is the password, which never enters the
//! configuration store: it runs as task `user.password`.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::scheduler::{SchedulerConfig, TaskAction};
use rynas_api_types::snapshots::SnapshotsConfig;
use rynas_api_types::{
    EffectivePermission, FolderDeletePlan, FolderRef, FolderRemoval, GroupRequest, GroupView,
    LocalGroup, LocalUser, NfsSettings, NfsState, PasswordRequest, PendingOp, PrincipalKind, Share,
    ShareDeletePlanRequest, ShareNocowRequest, ShareSizes, ShareStatus, ShareWarning, ShareWarningArgs, ShareWarningCode, ShareView, SharesConfig, SmbSettings, SmbState,
    SnapshotSource, StagedChanges, TaskStarted, UnsharedFolder, UserBadge, UserStatus, UserView,
    UsersConfig, error_codes,
};
use rynas_btrfs::SNAPSHOTS_KEY;
use rynas_scheduler::SCHEDULER_KEY;
use rynas_services::folders::{self, Usages};
use rynas_config::Config;
use rynas_services::accounts::Secret;
use rynas_services::perm::{Principal, effective};
use rynas_services::shares::{NFS_SERVER, SMBD, drop_principal};
use rynas_services::{SHARES_KEY, SharesDomain, USERS_KEY, UsersDomain, recycle, users};
use serde::Deserialize;
use serde_json::Value;

use crate::error::created;
use crate::ops::{Error, Ops, Res, user};

pub mod crypt;
mod homes;
mod ids;

/// What these endpoints work on: the store and tasks of [`Ops`] plus typed access to the
/// domains registered in it, and the record of the home moves waiting for confirmation.
pub struct Services {
    pub ops: Arc<Ops>,
    pub users: Arc<UsersDomain>,
    pub shares: Arc<SharesDomain>,
    pub homes: Arc<rynas_services::HomesDomain>,
    pub moves: Arc<rynas_services::homes::MoveStore>,
}

type SvcState = Option<Arc<Services>>;

fn svc(st: &SvcState) -> Res<Arc<Services>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "users and shares are not enabled in this rynasd".into(),
        )
    })
}

fn bad(code: &'static str, msg: impl Into<String>) -> Error {
    Error::new(StatusCode::BAD_REQUEST, code, msg.into())
}

fn not_found(what: impl Into<String>) -> Error {
    Error::new(
        StatusCode::NOT_FOUND,
        error_codes::NOT_FOUND,
        format!("{} not found", what.into()),
    )
}

fn conflict(code: &'static str, msg: impl Into<String>) -> Error {
    Error::new(StatusCode::CONFLICT, code, msg.into())
}

fn internal(e: impl std::fmt::Display) -> Error {
    Error::internal(e)
}

fn json_body<T>(b: Result<Json<T>, JsonRejection>) -> Res<T> {
    b.map(|Json(v)| v).map_err(Error::from)
}

/// Runs `f` on a blocking thread (SQLite, `/etc` files, commands).
async fn blocking<T: Send + 'static>(
    s: &Arc<Services>,
    f: impl FnOnce(&Services) -> Res<T> + Send + 'static,
) -> Res<T> {
    let s = s.clone();
    tokio::task::spawn_blocking(move || f(&s))
        .await
        .map_err(internal)?
}

pub fn router(st: SvcState) -> Router {
    Router::new()
        .route("/v1/users", get(users_list).post(user_create))
        .route(
            "/v1/users/{user}",
            get(user_get).put(user_put).delete(user_delete),
        )
        .route("/v1/users/{user}/password", post(user_password))
        .route("/v1/users/adopt", post(ids::users_adopt))
        .route("/v1/groups", get(groups_list).post(group_create))
        .route("/v1/groups/adopt", post(ids::groups_adopt))
        .route("/v1/ids/plan", post(ids::ids_plan))
        .route("/v1/ids/remap", post(ids::ids_remap))
        .route(
            "/v1/groups/{group}",
            get(group_get).put(group_put).delete(group_delete),
        )
        .route("/v1/services/smb", get(smb_get).put(smb_put))
        .route("/v1/services/smb/recycle-cleanup", post(recycle_cleanup))
        .route("/v1/services/nfs", get(nfs_get).put(nfs_put))
        .route(
            "/v1/services/homes",
            get(homes::homes_get).put(homes::homes_put),
        )
        .route("/v1/services/homes/move/plan", post(homes::move_plan))
        .route("/v1/services/homes/move", post(homes::move_start))
        .route(
            "/v1/services/homes/move/{user}/finish",
            post(homes::move_finish),
        )
        .route("/v1/services/homes/move/{user}/undo", post(homes::move_undo))
        .route("/v1/shares", get(shares_list).post(share_create))
        .route(
            "/v1/shares/{share}",
            get(share_get).put(share_put).delete(share_delete),
        )
        .route("/v1/shares/{share}/effective", get(share_effective))
        .route("/v1/shares/{share}/nocow", post(share_nocow))
        .route("/v1/shares/{share}/unlock", post(crypt::unlock))
        .route("/v1/shares/{share}/lock", post(crypt::lock))
        .route("/v1/shares/{share}/crypt/password", post(crypt::change_password))
        .route("/v1/shares/{share}/crypt/auto-unlock", axum::routing::put(crypt::auto_unlock))
        .route("/v1/crypt", get(crypt::info))
        .route("/v1/crypt/install", post(crypt::install))
        .route("/v1/crypt/folders", post(crypt::create))
        .route(
            "/v1/crypt/folders/{volume}/{name}/reveal-recovery-key",
            post(crypt::reveal),
        )
        .route(
            "/v1/crypt/folders/{volume}/{name}/recovery-key",
            post(crypt::recovery_key),
        )
        .route("/v1/crypt/recovery-pending", get(crypt::recovery_pending))
        .route(
            "/v1/crypt/folders/{volume}/{name}/recovery-key-saved",
            post(crypt::recovery_saved),
        )
        .merge(
            Router::new()
                .route("/v1/crypt/auto-unlock", post(crypt::auto_unlock_now))
                .route_layer(axum::middleware::from_fn(crate::server::require_root_peer)),
        )
        .route("/v1/shares/plan/delete", post(share_delete_plan))
        .route("/v1/folders", get(folders_list))
        .route("/v1/share-sizes", get(share_sizes))
        .route("/v1/folders/plan/delete", post(folder_delete_plan))
        .route(
            "/v1/folders/{volume}/{name}",
            axum::routing::delete(folder_delete),
        )
        .with_state(st)
}

// ---------------------------------------------------------------------------------------------
// documents as the session user sees them
// ---------------------------------------------------------------------------------------------

pub(crate) fn from_value<T: serde::de::DeserializeOwned + Default>(v: Option<Value>) -> Res<T> {
    match v {
        Some(v) => serde_json::from_value(v).map_err(internal),
        None => Ok(T::default()),
    }
}

/// A stored value taken from history (a restored revision, an imported archive) without the
/// one-time requests it carried: the folders deleted with their shares (`remove_folders`) and
/// the homes deleted with their users (`remove_homes`). They ran when that document was
/// applied; staged again they would delete the folder of a share, or the home of a user, made
/// again since under the same name ([`rynas_config::Config::on_history_value`]).
pub fn history_value(key: &str, mut v: Value) -> Value {
    let field = match key {
        SHARES_KEY => "remove_folders",
        USERS_KEY => "remove_homes",
        _ => return v,
    };
    if let Some(o) = v.as_object_mut() {
        o.remove(field);
    }
    v
}

/// `(staged or current, current, applied)` of a key.
pub(crate) fn docs<T: serde::de::DeserializeOwned + Default + Clone>(
    c: &Config,
    owner: &str,
    key: &str,
) -> Res<(T, T, Option<T>)> {
    let current: T = from_value(c.current(key)?)?;
    let applied: Option<T> = match c.applied(key)? {
        Some(v) => Some(serde_json::from_value(v).map_err(internal)?),
        None => None,
    };
    let effective = match c.pending_value(owner, key)? {
        Some(Some(v)) => serde_json::from_value(v).map_err(internal)?,
        Some(None) => T::default(),
        None => current.clone(),
    };
    Ok((effective, current, applied))
}

pub(crate) fn stage<T: serde::Serialize>(c: &Config, owner: &str, key: &str, doc: &T) -> Res<()> {
    let v = serde_json::to_value(doc).map_err(internal)?;
    c.stage(owner, key, Some(v))?;
    Ok(())
}

pub(crate) fn pending_op<T: PartialEq>(now: Option<&T>, base: Option<&T>) -> Option<PendingOp> {
    match (now, base) {
        (Some(_), None) => Some(PendingOp::Add),
        (None, Some(_)) => Some(PendingOp::Delete),
        (Some(a), Some(b)) if a != b => Some(PendingOp::Change),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// users
// ---------------------------------------------------------------------------------------------

fn user_views(s: &Services, owner: &str) -> Res<Vec<UserView>> {
    let c = &s.ops.config;
    let (eff, cur, applied): (UsersConfig, UsersConfig, _) = docs(c, owner, USERS_KEY)?;
    let acc = s.users.accounts();
    let sys = acc.users().map_err(internal)?;
    let samba = acc.samba_users_cached().map_err(internal)?;
    let today = rynas_services::validate::today_days();
    let managed = |n: &str| {
        applied
            .as_ref()
            .is_some_and(|a: &UsersConfig| a.users.iter().any(|u| u.name == n))
    };
    let view = |u: &LocalUser, pending: Option<PendingOp>| {
        let on_system = sys.iter().any(|x| x.name == u.name);
        let in_samba = samba.as_ref().map(|l| l.iter().any(|x| x.name == u.name));
        let needs_password = u.smb && on_system && in_samba == Some(false);
        let m = managed(&u.name);
        let mut badges = Vec::new();
        if !m {
            badges.push(UserBadge::Unmanaged);
        }
        if needs_password {
            badges.push(UserBadge::SmbPasswordReset);
        }
        if u.disabled {
            badges.push(UserBadge::Disabled);
        }
        if u.expires
            .as_deref()
            .and_then(rynas_services::validate::date_to_days)
            .is_some_and(|d| d <= today)
        {
            badges.push(UserBadge::Expired);
        }
        if m && !on_system {
            badges.push(UserBadge::Missing);
        }
        if u.admin {
            badges.push(UserBadge::Admin);
        }
        UserView {
            user: u.clone(),
            status: UserStatus {
                managed: m,
                on_system,
                in_samba,
                needs_password,
                pending,
                badges,
            },
        }
    };
    let mut out: Vec<UserView> = eff
        .users
        .iter()
        .map(|u| {
            view(
                u,
                pending_op(Some(u), cur.users.iter().find(|x| x.name == u.name)),
            )
        })
        .collect();
    for u in cur
        .users
        .iter()
        .filter(|u| !eff.users.iter().any(|x| x.name == u.name))
    {
        out.push(view(u, Some(PendingOp::Delete)));
    }
    // users of the system that the configuration does not know (created outside rynas)
    let doc = users::import_from(&sys, &acc.groups().map_err(internal)?, samba.as_deref());
    let known: BTreeSet<String> = out.iter().map(|x| x.user.name.clone()).collect();
    for u in doc.users.iter().filter(|u| !known.contains(&u.name)) {
        out.push(view(u, None));
    }
    out.sort_by(|a, b| a.user.name.cmp(&b.user.name));
    Ok(out)
}

fn user_view(s: &Services, owner: &str, name: &str) -> Res<UserView> {
    user_views(s, owner)?
        .into_iter()
        .find(|v| v.user.name == name)
        .ok_or_else(|| not_found(format!("user {name}")))
}

/// Stages `f(users document)`; drops `remove_homes` entries that no longer apply.
fn stage_users(s: &Services, owner: &str, f: impl FnOnce(&mut UsersConfig) -> Res<()>) -> Res<()> {
    let c = &s.ops.config;
    let (mut doc, cur, _): (UsersConfig, UsersConfig, _) = docs(c, owner, USERS_KEY)?;
    f(&mut doc)?;
    let names: BTreeSet<&str> = doc.users.iter().map(|u| u.name.as_str()).collect();
    let before: BTreeSet<&str> = cur.users.iter().map(|u| u.name.as_str()).collect();
    doc.remove_homes
        .retain(|n| before.contains(n.as_str()) && !names.contains(n.as_str()));
    stage(c, owner, USERS_KEY, &doc)
}

async fn users_list(State(st): State<SvcState>, h: HeaderMap) -> Res<Json<Vec<UserView>>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    Ok(Json(blocking(&s, move |s| user_views(s, &owner)).await?))
}

async fn user_get(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Res<Json<UserView>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    Ok(Json(
        blocking(&s, move |s| user_view(s, &owner, &name)).await?,
    ))
}

async fn user_create(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<LocalUser>, JsonRejection>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let mut u = json_body(body)?;
    u.name = u.name.trim().to_string();
    let loc = u.name.clone();
    let v = blocking(&s, move |s| {
        let name = u.name.clone();
        homes::default_home(s, &owner, &mut u)?;
        stage_users(s, &owner, |doc| {
            if doc.users.iter().any(|x| x.name == u.name) {
                return Err(conflict(
                    error_codes::ALREADY_EXISTS,
                    format!("user {} already exists", u.name),
                ));
            }
            doc.users.push(u);
            Ok(())
        })?;
        user_view(s, &owner, &name)
    })
    .await?;
    Ok(created("/api/v1/users", &loc, v))
}

async fn user_put(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<LocalUser>, JsonRejection>,
) -> Res<Json<UserView>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let mut u = json_body(body)?;
    if !u.name.is_empty() && u.name != name {
        return Err(Error::invalid("name", "a user cannot be renamed"));
    }
    u.name = name.clone();
    Ok(Json(
        blocking(&s, move |s| {
            // a user of the system that rynas does not manage yet is adopted by saving it
            let acc = s.users.accounts();
            let sys = users::import_from(
                &acc.users().map_err(internal)?,
                &acc.groups().map_err(internal)?,
                acc.samba_users().map_err(internal)?.as_deref(),
            );
            stage_users(s, &owner, |doc| {
                if !doc.users.iter().any(|x| x.name == name) {
                    users::adopt(doc, &sys, rynas_api_types::IdKind::User, std::slice::from_ref(&name))
                        .map_err(|_| not_found(format!("user {name}")))?;
                }
                let cur = doc
                    .users
                    .iter_mut()
                    .find(|x| x.name == name)
                    .ok_or_else(|| not_found(format!("user {name}")))?;
                if u.uid != 0 && u.uid != cur.uid {
                    return Err(Error::invalid("uid", "the uid cannot change"));
                }
                u.uid = cur.uid;
                if !u.home.is_empty() && u.home != cur.home {
                    return Err(Error::invalid(
                        "home",
                        "the home changes only by moving it (User homes)",
                    ));
                }
                if !u.home_volume.is_empty() && u.home_volume != cur.home_volume {
                    return Err(Error::invalid(
                        "home_volume",
                        "the home changes volume only by moving it (User homes)",
                    ));
                }
                u.home = cur.home.clone();
                u.home_volume = cur.home_volume.clone();
                *cur = u;
                Ok(())
            })?;
            user_view(s, &owner, &name)
        })
        .await?,
    ))
}

#[derive(Deserialize)]
struct DeleteUserQuery {
    #[serde(default)]
    remove_home: bool,
    confirm: Option<String>,
}

async fn user_delete(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
    Query(q): Query<DeleteUserQuery>,
) -> Res<Json<StagedChanges>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    if q.remove_home && q.confirm.as_deref() != Some(name.as_str()) {
        return Err(bad(
            error_codes::CONFIRMATION_REQUIRED,
            format!("removing the home directory needs confirm={name}"),
        ));
    }
    if name == owner {
        return Err(conflict(
            error_codes::CONFLICT,
            "you cannot delete your own account",
        ));
    }
    let staged = blocking(&s, move |s| {
        homes::check_no_waiting_move(s, &name)?;
        stage_users(s, &owner, |doc| {
            let before = doc.users.len();
            doc.users.retain(|u| u.name != name);
            if doc.users.len() == before {
                return Err(not_found(format!("user {name}")));
            }
            if q.remove_home {
                doc.remove_homes.push(name.clone());
            }
            Ok(())
        })?;
        let mut n = 1;
        n += drop_from_shares(s, &owner, PrincipalKind::User, &name)?;
        Ok(n)
    })
    .await?;
    Ok(Json(StagedChanges { staged }))
}

/// Removes a deleted user or group from the permission matrices (staged with it).
fn drop_from_shares(s: &Services, owner: &str, kind: PrincipalKind, name: &str) -> Res<u32> {
    let c = &s.ops.config;
    if c.current(SHARES_KEY)?.is_none() && c.pending_value(owner, SHARES_KEY)?.is_none() {
        return Ok(0);
    }
    let (mut doc, _, _): (SharesConfig, SharesConfig, _) = docs(c, owner, SHARES_KEY)?;
    if !drop_principal(&mut doc, kind, name) {
        return Ok(0);
    }
    stage(c, owner, SHARES_KEY, &doc)?;
    Ok(1)
}

async fn user_password(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<PasswordRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let req = json_body(body)?;
    let password = Secret::new(req.password.clone());
    drop(req);
    users::check_password(&name, password.expose()).map_err(|e| Error::invalid("password", e))?;
    let n = name.clone();
    let (smb, disabled) = blocking(&s, move |s| {
        let acc = s.users.accounts();
        let on_system = acc.users().map_err(internal)?.iter().any(|u| u.name == n);
        let (eff, cur, _): (UsersConfig, UsersConfig, _) = docs(&s.ops.config, &owner, USERS_KEY)?;
        let configured = cur.users.iter().find(|u| u.name == n);
        if !on_system {
            return Err(if eff.users.iter().any(|u| u.name == n) {
                conflict(
                    error_codes::NOT_APPLIED,
                    format!("user {n} is not created yet: apply the pending changes first"),
                )
            } else {
                not_found(format!("user {n}"))
            });
        }
        Ok(match configured {
            Some(u) => (u.smb, u.disabled),
            None => (true, false),
        })
    })
    .await?;
    let accounts = s.users.accounts().clone();
    let id = s
        .ops
        .tasks
        .spawn("user.password", &user(&h)?, move |ctx| async move {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || {
                let r = users::set_password(accounts.as_ref(), &name, &password, smb, disabled, &|l| {
                    c.log(l)
                });
                if r.is_ok() {
                    // paired phones keep access: say so, with their names
                    crate::devices::password_changed(&name);
                }
                r
            })
            .await
            .map_err(|e| e.to_string())?
        })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

// ---------------------------------------------------------------------------------------------
// groups
// ---------------------------------------------------------------------------------------------

fn group_views(s: &Services, owner: &str) -> Res<Vec<GroupView>> {
    let c = &s.ops.config;
    let (eff, cur, applied): (UsersConfig, UsersConfig, _) = docs(c, owner, USERS_KEY)?;
    let sys = s.users.accounts().groups().map_err(internal)?;
    let view = |g: &LocalGroup, pending| GroupView {
        group: g.clone(),
        members: eff
            .users
            .iter()
            .filter(|u| u.groups.contains(&g.name))
            .map(|u| u.name.clone())
            .collect(),
        managed: applied
            .as_ref()
            .is_some_and(|a: &UsersConfig| a.groups.iter().any(|x| x.name == g.name)),
        on_system: sys.iter().any(|x| x.name == g.name),
        pending,
    };
    let mut out: Vec<GroupView> = eff
        .groups
        .iter()
        .map(|g| {
            view(
                g,
                pending_op(Some(g), cur.groups.iter().find(|x| x.name == g.name)),
            )
        })
        .collect();
    for g in cur
        .groups
        .iter()
        .filter(|g| !eff.groups.iter().any(|x| x.name == g.name))
    {
        out.push(view(g, Some(PendingOp::Delete)));
    }
    // groups of the system that the configuration does not know (created outside rynas)
    let doc = users::import_from(&[], &sys, None);
    for g in doc.groups {
        if !out.iter().any(|x| x.group.name == g.name) {
            let members = sys
                .iter()
                .find(|x| x.name == g.name)
                .map(|x| x.members.clone())
                .unwrap_or_default();
            out.push(GroupView {
                members,
                ..view(&g, None)
            });
        }
    }
    out.sort_by(|a, b| a.group.name.cmp(&b.group.name));
    Ok(out)
}

fn group_view(s: &Services, owner: &str, name: &str) -> Res<GroupView> {
    group_views(s, owner)?
        .into_iter()
        .find(|v| v.group.name == name)
        .ok_or_else(|| not_found(format!("group {name}")))
}

fn set_members(doc: &mut UsersConfig, group: &str, members: &[String]) -> Res<()> {
    for m in members {
        if !doc.users.iter().any(|u| &u.name == m) {
            return Err(Error::invalid("members", format!("unknown user {m}")));
        }
    }
    for u in &mut doc.users {
        let want = members.contains(&u.name);
        let has = u.groups.iter().any(|g| g == group);
        if want && !has {
            u.groups.push(group.into());
        } else if !want && has {
            u.groups.retain(|g| g != group);
        }
    }
    Ok(())
}

async fn groups_list(State(st): State<SvcState>, h: HeaderMap) -> Res<Json<Vec<GroupView>>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    Ok(Json(blocking(&s, move |s| group_views(s, &owner)).await?))
}

async fn group_get(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Res<Json<GroupView>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    Ok(Json(
        blocking(&s, move |s| group_view(s, &owner, &name)).await?,
    ))
}

async fn group_create(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<GroupRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let mut req = json_body(body)?;
    req.group.name = req.group.name.trim().to_string();
    let loc = req.group.name.clone();
    let v = blocking(&s, move |s| {
        let name = req.group.name.clone();
        stage_users(s, &owner, |doc| {
            if doc.groups.iter().any(|g| g.name == name) {
                return Err(conflict(
                    error_codes::ALREADY_EXISTS,
                    format!("group {name} already exists"),
                ));
            }
            doc.groups.push(req.group);
            if let Some(m) = &req.members {
                set_members(doc, &name, m)?;
            }
            Ok(())
        })?;
        group_view(s, &owner, &name)
    })
    .await?;
    Ok(created("/api/v1/groups", &loc, v))
}

async fn group_put(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<GroupRequest>, JsonRejection>,
) -> Res<Json<GroupView>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let mut req = json_body(body)?;
    if !req.group.name.is_empty() && req.group.name != name {
        return Err(Error::invalid("name", "a group cannot be renamed"));
    }
    req.group.name = name.clone();
    Ok(Json(
        blocking(&s, move |s| {
            // a group of the system that rynas does not manage yet is adopted by saving it
            let sys = users::import_from(&[], &s.users.accounts().groups().map_err(internal)?, None);
            stage_users(s, &owner, |doc| {
                if !doc.groups.iter().any(|g| g.name == name) {
                    users::adopt(doc, &sys, rynas_api_types::IdKind::Group, std::slice::from_ref(&name))
                        .map_err(|_| not_found(format!("group {name}")))?;
                }
                let cur = doc
                    .groups
                    .iter_mut()
                    .find(|g| g.name == name)
                    .ok_or_else(|| not_found(format!("group {name}")))?;
                if req.group.gid != 0 && req.group.gid != cur.gid {
                    return Err(Error::invalid("gid", "the gid cannot change"));
                }
                cur.description = req.group.description.clone();
                if let Some(m) = &req.members {
                    set_members(doc, &name, m)?;
                }
                Ok(())
            })?;
            group_view(s, &owner, &name)
        })
        .await?,
    ))
}

async fn group_delete(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Res<Json<StagedChanges>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let staged = blocking(&s, move |s| {
        stage_users(s, &owner, |doc| {
            let before = doc.groups.len();
            doc.groups.retain(|g| g.name != name);
            if doc.groups.len() == before {
                return Err(not_found(format!("group {name}")));
            }
            for u in &mut doc.users {
                u.groups.retain(|g| *g != name);
            }
            Ok(())
        })?;
        Ok(1 + drop_from_shares(s, &owner, PrincipalKind::Group, &name)?)
    })
    .await?;
    Ok(Json(StagedChanges { staged }))
}

// ---------------------------------------------------------------------------------------------
// SMB and NFS settings
// ---------------------------------------------------------------------------------------------

fn unit_running(s: &Services, unit: &str) -> bool {
    s.ops.config.system().unit_active(unit).unwrap_or(false)
}

fn smb_state(s: &Services, owner: &str) -> Res<SmbState> {
    let (eff, cur, _): (SharesConfig, SharesConfig, _) = docs(&s.ops.config, owner, SHARES_KEY)?;
    Ok(SmbState {
        pending: eff.smb != cur.smb,
        settings: eff.smb,
        running: unit_running(s, SMBD),
    })
}

fn nfs_state(s: &Services, owner: &str) -> Res<NfsState> {
    let (eff, cur, _): (SharesConfig, SharesConfig, _) = docs(&s.ops.config, owner, SHARES_KEY)?;
    Ok(NfsState {
        pending: eff.nfs != cur.nfs,
        settings: eff.nfs,
        running: unit_running(s, NFS_SERVER),
    })
}

fn stage_shares(
    s: &Services,
    owner: &str,
    f: impl FnOnce(&mut SharesConfig) -> Res<()>,
) -> Res<()> {
    let c = &s.ops.config;
    let (mut doc, _, _): (SharesConfig, SharesConfig, _) = docs(c, owner, SHARES_KEY)?;
    f(&mut doc)?;
    check_principals(s, owner, &doc)?;
    stage(c, owner, SHARES_KEY, &doc)
}

async fn smb_get(State(st): State<SvcState>, h: HeaderMap) -> Res<Json<SmbState>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    Ok(Json(blocking(&s, move |s| smb_state(s, &owner)).await?))
}

async fn smb_put(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<SmbSettings>, JsonRejection>,
) -> Res<Json<SmbState>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let v = json_body(body)?;
    Ok(Json(
        blocking(&s, move |s| {
            stage_shares(s, &owner, |d| {
                d.smb = v;
                Ok(())
            })?;
            smb_state(s, &owner)
        })
        .await?,
    ))
}

async fn nfs_get(State(st): State<SvcState>, h: HeaderMap) -> Res<Json<NfsState>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    Ok(Json(blocking(&s, move |s| nfs_state(s, &owner)).await?))
}

async fn nfs_put(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<NfsSettings>, JsonRejection>,
) -> Res<Json<NfsState>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let v = json_body(body)?;
    Ok(Json(
        blocking(&s, move |s| {
            stage_shares(s, &owner, |d| {
                d.nfs = v;
                Ok(())
            })?;
            nfs_state(s, &owner)
        })
        .await?,
    ))
}

#[derive(Deserialize)]
struct CleanupQuery {
    share: Option<String>,
    /// Overrides the retention of the shares.
    days: Option<u32>,
}

/// Task `smb.recycle-cleanup`: empties the `#recycle` bins of the applied shares (or of one
/// share) of items older than their retention (or `days`).
async fn recycle_cleanup(
    State(st): State<SvcState>,
    h: HeaderMap,
    Query(q): Query<CleanupQuery>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let id = spawn_recycle_cleanup(&s.ops, &owner, q.share, q.days).await?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

/// Starts task `smb.recycle-cleanup` on the applied shares with a recycle bin (or one of
/// them); also run by the scheduler (internal action `recycle_cleanup`).
pub async fn spawn_recycle_cleanup(
    ops: &Arc<Ops>,
    owner: &str,
    share: Option<String>,
    days: Option<u32>,
) -> Res<String> {
    let cfg = ops.config.clone();
    let doc: SharesConfig = tokio::task::spawn_blocking(move || -> Res<SharesConfig> {
        from_value(cfg.applied(SHARES_KEY)?)
    })
    .await
    .map_err(internal)??;
    let targets: Vec<(String, String, u32)> = doc
        .shares
        .iter()
        .filter(|sh| share.as_ref().is_none_or(|n| *n == sh.name))
        .filter(|sh| sh.smb.enabled && sh.smb.recycle.enabled)
        .map(|sh| {
            (
                sh.name.clone(),
                sh.path.clone(),
                days.unwrap_or(sh.smb.recycle.retention_days),
            )
        })
        .collect();
    if let Some(n) = &share
        && targets.is_empty()
    {
        return Err(not_found(format!("applied share {n} with a recycle bin")));
    }
    let id = ops
        .tasks
        .spawn("smb.recycle-cleanup", owner, move |ctx| async move {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let now = (rynas_tasks::now_ms() / 1000) as i64;
                for (name, path, days) in targets {
                    if days == 0 {
                        c.log(&format!("{name}: retention 0, recycle bin kept"));
                        continue;
                    }
                    let st = recycle::cleanup(&path, days, now, &|| c.is_cancelled())?;
                    c.log(&format!(
                        "{name}: {} files and {} folders older than {days} days removed ({} bytes)",
                        st.files, st.dirs, st.bytes
                    ));
                }
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())?
        })?;
    Ok(id)
}

// ---------------------------------------------------------------------------------------------
// shares
// ---------------------------------------------------------------------------------------------

/// Every principal of the matrices must be a user or group of the (staged) configuration or
/// of the system.
fn check_principals(s: &Services, owner: &str, doc: &SharesConfig) -> Res<()> {
    let (users_doc, _, _): (UsersConfig, UsersConfig, _) = docs(&s.ops.config, owner, USERS_KEY)?;
    let acc = s.users.accounts();
    let sys_users = acc.users().map_err(internal)?;
    let sys_groups = acc.groups().map_err(internal)?;
    for sh in &doc.shares {
        for p in &sh.permissions {
            let known = match p.kind {
                PrincipalKind::User => {
                    users_doc.users.iter().any(|u| u.name == p.name)
                        || sys_users.iter().any(|u| u.name == p.name)
                }
                PrincipalKind::Group => {
                    users_doc.groups.iter().any(|g| g.name == p.name)
                        || sys_groups.iter().any(|g| g.name == p.name)
                }
            };
            if !known {
                return Err(Error::invalid(
                    "permissions",
                    format!(
                        "share {}: unknown {} {}",
                        sh.name,
                        if p.kind == PrincipalKind::User {
                            "user"
                        } else {
                            "group"
                        },
                        p.name
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn share_views(s: &Services, owner: &str) -> Res<Vec<ShareView>> {
    let (eff, cur, _): (SharesConfig, SharesConfig, _) = docs(&s.ops.config, owner, SHARES_KEY)?;
    let host = s.shares.host();
    // without the snapshots module (no key) there are no jobs
    let snaps: SnapshotsConfig = docs::<SnapshotsConfig>(&s.ops.config, owner, SNAPSHOTS_KEY).map(|d| d.0).unwrap_or_default();
    let pc_folders = s.ops.pc_backup.get().map(|pc| pc.snapshot_folders()).unwrap_or_default();
    let view = |sh: &Share, pending| {
        let info = host.probe(&sh.path);
        let mut items = s.shares.warning_items(&eff, sh);
        // snapshots of a folder without copy-on-write: they work, rewrites cost space
        let path = sh.path.trim_end_matches('/');
        if info.nocow == Some(true) && snaps.jobs.iter().any(|j| j.subvolume.trim_end_matches('/') == path) {
            items.push(ShareWarning {
                code: ShareWarningCode::NocowSnapshots,
                args: ShareWarningArgs { path: Some(sh.path.clone()), ..Default::default() },
            });
        }
        // over a PC's backups (made before rynas refused it): read-only in the file manager
        for (_, device) in crate::pc_backup::overlapping_shares(&pc_folders, &[(sh.name.clone(), sh.path.clone())]) {
            let at = pc_folders.iter().find(|(d, _)| *d == device).map(|(_, p)| p.clone());
            items.push(ShareWarning { code: ShareWarningCode::PcBackupFolder, args: ShareWarningArgs { path: at, key: Some(device), ..Default::default() } });
        }
        ShareView {
            share: sh.clone(),
            status: ShareStatus {
                pending,
                path_ok: info.exists && info.is_dir,
                fs_type: info.fs_type,
                subvolume: info.subvolume,
                nocow: info.nocow,
                warnings: items.iter().map(rynas_services::shares::warning_text).collect(),
                warning_items: items,
                crypt: sh.encrypted.then(|| s.shares.crypt().status(&sh.path)),
            },
        }
    };
    let mut out: Vec<ShareView> = eff
        .shares
        .iter()
        .map(|sh| {
            view(
                sh,
                pending_op(Some(sh), cur.shares.iter().find(|x| x.name == sh.name)),
            )
        })
        .collect();
    for sh in cur
        .shares
        .iter()
        .filter(|sh| !eff.shares.iter().any(|x| x.name == sh.name))
    {
        out.push(view(sh, Some(PendingOp::Delete)));
    }
    out.sort_by_key(|v| v.share.name.to_ascii_lowercase());
    Ok(out)
}

fn share_view(s: &Services, owner: &str, name: &str) -> Res<ShareView> {
    share_views(s, owner)?
        .into_iter()
        .find(|v| v.share.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| not_found(format!("share {name}")))
}

/// Refuses a share whose folder `path` is, holds or lies inside a PC's backup folder: the
/// file manager shows a backup read-only and a share writable, one folder cannot be both
/// (an empty path, made from the subvolume on apply, is a new folder of its own).
fn refuse_pc_folder(s: &Services, name: &str, path: &str) -> Res<()> {
    if path.is_empty() {
        return Ok(());
    }
    let Some(pc) = s.ops.pc_backup.get() else { return Ok(()) };
    let folders = pc.snapshot_folders();
    match crate::pc_backup::overlapping_shares(&folders, &[(name.to_string(), path.to_string())]).first() {
        Some((_, device)) => {
            let at = folders.iter().find(|(d, _)| d == device).map(|(_, p)| p.as_str()).unwrap_or_default();
            Err(Error::invalid(
                "path",
                format!("share {name}: {path} overlaps {at}, the backup folder of the PC {device}: a shared folder cannot hold a PC's backups nor lie inside them"),
            ))
        }
        None => Ok(()),
    }
}

async fn shares_list(State(st): State<SvcState>, h: HeaderMap) -> Res<Json<Vec<ShareView>>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    Ok(Json(blocking(&s, move |s| share_views(s, &owner)).await?))
}

async fn share_get(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Res<Json<ShareView>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    Ok(Json(
        blocking(&s, move |s| share_view(s, &owner, &name)).await?,
    ))
}

async fn share_create(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<Share>, JsonRejection>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let mut sh = json_body(body)?;
    sh.name = sh.name.trim().to_string();
    let loc = sh.name.clone();
    let v = blocking(&s, move |s| {
        let name = sh.name.clone();
        stage_shares(s, &owner, |doc| {
            if doc
                .shares
                .iter()
                .any(|x| x.name.eq_ignore_ascii_case(&sh.name))
            {
                return Err(conflict(
                    error_codes::ALREADY_EXISTS,
                    format!("share {} already exists", sh.name),
                ));
            }
            refuse_pc_folder(s, &sh.name, &sh.path)?;
            doc.shares.push(sh);
            Ok(())
        })?;
        share_view(s, &owner, &name)
    })
    .await?;
    Ok(created("/api/v1/shares", &loc, v))
}

async fn share_put(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<Share>, JsonRejection>,
) -> Res<Json<ShareView>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let mut sh = json_body(body)?;
    if !sh.name.is_empty() && !sh.name.eq_ignore_ascii_case(&name) {
        return Err(Error::invalid("name", "a share cannot be renamed"));
    }
    Ok(Json(
        blocking(&s, move |s| {
            stage_shares(s, &owner, |doc| {
                let cur = doc
                    .shares
                    .iter_mut()
                    .find(|x| x.name.eq_ignore_ascii_case(&name))
                    .ok_or_else(|| not_found(format!("share {name}")))?;
                sh.name = cur.name.clone();
                // a new path gets a new fsid (unless one is given explicitly), and never one
                // over a PC's backups
                if sh.path != cur.path {
                    refuse_pc_folder(s, &cur.name, &sh.path)?;
                    if sh.nfs.fsid == cur.nfs.fsid {
                        sh.nfs.fsid.clear();
                    }
                }
                *cur = sh;
                Ok(())
            })?;
            share_view(s, &owner, &name)
        })
        .await?,
    ))
}

#[derive(Deserialize)]
struct DeleteShareQuery {
    /// Also delete the folder, its snapshots and its recycle bin (on apply).
    #[serde(default)]
    remove_folder: bool,
    confirm: Option<String>,
}

/// Stages the deletion of a share; with `remove_folder=true&confirm=<name>` also the
/// deletion of its folder, snapshots and recycle bin at the end of the apply, and the
/// removal of its snapshot jobs (with their scheduler tasks).
async fn share_delete(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
    Query(q): Query<DeleteShareQuery>,
) -> Res<Json<StagedChanges>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    if q.remove_folder && q.confirm.as_deref() != Some(name.as_str()) {
        return Err(bad(
            error_codes::CONFIRMATION_REQUIRED,
            format!("deleting the folder of the share needs confirm={name}"),
        ));
    }
    let staged = blocking(&s, move |s| {
        let c = &s.ops.config;
        let (doc, _, _): (SharesConfig, SharesConfig, _) = docs(c, &owner, SHARES_KEY)?;
        let share = doc
            .shares
            .iter()
            .find(|x| x.name.eq_ignore_ascii_case(&name))
            .cloned()
            .ok_or_else(|| not_found(format!("share {name}")))?;
        let removal = if q.remove_folder {
            let (p, sources, jobs) = delete_plan(s, &owner, &share)?;
            if !p.allowed {
                return Err(conflict(
                    error_codes::CONFLICT,
                    format!(
                        "{} cannot be deleted: {}",
                        p.path,
                        p.blockers
                            .iter()
                            .map(|b| format!("{:?} {}", b.kind, b.name))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
            Some((
                FolderRemoval {
                    share: share.name.clone(),
                    path: share.path.clone(),
                    snapshots: sources,
                },
                jobs,
            ))
        } else {
            None
        };
        let mut n = 1;
        let destructive = removal.is_some();
        stage_shares(s, &owner, |doc| {
            doc.shares.retain(|x| x.name != share.name);
            if let Some((r, _)) = &removal {
                doc.remove_folders.retain(|x| x.share != r.share);
                doc.remove_folders.push(r.clone());
            }
            Ok(())
        })?;
        // the snapshot jobs of the folder go with it (their snapshots go with the folder)
        if let Some((_, jobs)) = removal.filter(|(_, j)| !j.is_empty()) {
            let (mut snaps, _, _): (SnapshotsConfig, SnapshotsConfig, _) =
                docs(c, &owner, SNAPSHOTS_KEY)?;
            snaps.jobs.retain(|j| !jobs.contains(&j.name));
            stage(c, &owner, SNAPSHOTS_KEY, &snaps)?;
            let (mut sched, _, _): (SchedulerConfig, SchedulerConfig, _) =
                docs(c, &owner, SCHEDULER_KEY)?;
            sched.tasks.retain(|t| match &t.action {
                TaskAction::Snapshot { job } | TaskAction::Replicate { job, .. } => {
                    !jobs.contains(job)
                }
                _ => true,
            });
            stage(c, &owner, SCHEDULER_KEY, &sched)?;
            n += 2;
        }
        // the deletion is irreversible: only the pending bar's full apply commits it, never
        // a dialog's Save of `shares` that would also split it from its jobs (review M1)
        if destructive {
            c.stage_group(&owner, &[SHARES_KEY, SNAPSHOTS_KEY, SCHEDULER_KEY], true)?;
        }
        Ok(n)
    })
    .await?;
    Ok(Json(StagedChanges { staged }))
}

// ---------------------------------------------------------------------------------------------
// folders: deletion plans, folders without a share
// ---------------------------------------------------------------------------------------------

/// Snapshot jobs (as the session user sees them) taking snapshots of `path`; for an
/// encrypted folder also those of its ciphertext directory (what its snapshots hold).
fn jobs_of(s: &Services, owner: &str, path: &str) -> Res<Vec<(String, SnapshotSource)>> {
    let (snaps, _, _): (SnapshotsConfig, SnapshotsConfig, _) =
        docs(&s.ops.config, owner, SNAPSHOTS_KEY)?;
    let cipher = rynas_services::crypt::cipher_dir(path)
        .filter(|_| s.shares.crypt().state(path) != rynas_api_types::CryptState::Missing);
    Ok(snaps
        .jobs
        .iter()
        .filter(|j| {
            let sub = j.subvolume.trim_end_matches('/');
            sub == path || cipher.as_deref() == Some(sub)
        })
        .map(|j| {
            (
                j.name.clone(),
                SnapshotSource {
                    dir: j.snapshot_dir.clone(),
                    prefix: j.name.clone(),
                },
            )
        })
        .collect())
}

/// Every snapshot directory of a folder: its jobs, the share's previous versions, the
/// share recorded when it was removed.
fn snapshot_sources(
    s: &Services,
    owner: &str,
    path: &str,
    share: Option<&Share>,
) -> Res<Vec<SnapshotSource>> {
    let mut v: Vec<SnapshotSource> = jobs_of(s, owner, path)?.into_iter().map(|j| j.1).collect();
    let recorded = s.shares.removed().find(path).map(|r| r.share);
    for sh in share.into_iter().chain(recorded.as_ref()) {
        let sc = &sh.smb.shadow_copy;
        if !sc.snapshot_dir.is_empty() {
            v.push(SnapshotSource {
                dir: sc.snapshot_dir.trim_end_matches('/').to_string(),
                prefix: rynas_services::smb::shadow_prefix(sh),
            });
        }
    }
    v.sort_by(|a, b| (&a.dir, &a.prefix).cmp(&(&b.dir, &b.prefix)));
    v.dedup();
    Ok(v)
}

/// Shares of the applied configuration and of the session user's changeset.
fn all_shares(s: &Services, owner: &str) -> Res<Vec<Share>> {
    let (eff, cur, _): (SharesConfig, SharesConfig, _) = docs(&s.ops.config, owner, SHARES_KEY)?;
    let mut v = eff.shares;
    let extra: Vec<Share> = cur
        .shares
        .into_iter()
        .filter(|x| !v.iter().any(|y| y.name == x.name))
        .collect();
    v.extend(extra);
    Ok(v)
}

/// `docker inspect` objects of the containers (`running`: only the running ones); empty when
/// Docker is not installed. One batch, else one by one: a container that went away between
/// the listing and the inspection is skipped, never the whole answer.
pub(crate) fn inspect_containers(running: bool) -> Result<Vec<Value>, String> {
    if !std::path::Path::new("/usr/bin/docker").exists() {
        return Ok(Vec::new());
    }
    let t = std::time::Duration::from_secs(10);
    // the engine and its socket both off: no container runs (and asking would fail)
    if let Ok(o) = rynas_tasks::Cmd::new("systemctl").args(["is-active", "docker.service", "docker.socket"]).timeout(t).output_blocking()
        && docker_off(&o.stdout)
    {
        return Ok(Vec::new());
    }
    let ids = rynas_tasks::Cmd::new("docker")
        .args(if running { ["ps", "-q"] } else { ["ps", "-aq"] })
        .timeout(t)
        .run_blocking()
        .map_err(|e| e.to_string())?;
    let ids: Vec<&str> = ids.stdout.split_whitespace().collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let parse = |s: &str| match serde_json::from_str::<Value>(s) {
        Ok(Value::Array(v)) => Ok(v),
        _ => Err("docker inspect answered something unexpected".to_string()),
    };
    if let Ok(o) = rynas_tasks::Cmd::new("docker").arg("inspect").args(ids.iter().copied()).timeout(t).run_blocking() {
        return parse(&o.stdout);
    }
    let mut out = Vec::new();
    for id in ids {
        if let Ok(o) = rynas_tasks::Cmd::new("docker").args(["inspect", id]).timeout(t).run_blocking() {
            out.extend(parse(&o.stdout)?);
        }
    }
    Ok(out)
}

/// `systemctl is-active docker.service docker.socket` output: both units are known and
/// neither is active (`inactive`, `failed`...). An unknown answer is not "off".
fn docker_off(out: &str) -> bool {
    let states: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    states.len() == 2 && states.iter().all(|s| !matches!(*s, "active" | "activating" | "reloading" | "deactivating" | "refreshing"))
}

/// Containers that mount `path` or something inside it, or a folder holding it (when Docker
/// is installed; a failure lists none: for the usages of a folder).
fn containers_using(path: &str, running: bool) -> Vec<String> {
    let list = inspect_containers(running).unwrap_or_default();
    users_in_inspect(&list, path)
}

pub(crate) fn container_name(c: &Value) -> Option<String> {
    Some(c.get("Name")?.as_str()?.trim_start_matches('/').to_string())
}

/// (source, propagation) of each mount of a container.
fn mounts_of(c: &Value) -> Vec<(String, String)> {
    c.get("Mounts")
        .and_then(Value::as_array)
        .map(|ms| {
            ms.iter()
                .map(|m| {
                    let get = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    (get("Source"), get("Propagation"))
                })
                .filter(|(s, _)| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The containers with a mount of `path`, inside it or holding it.
fn users_in_inspect(list: &[Value], path: &str) -> Vec<String> {
    list.iter()
        .filter(|c| mounts_of(c).iter().any(|(src, _)| folders::is_within(src, path) || folders::is_within(path, src)))
        .filter_map(container_name)
        .collect()
}

/// The running containers that would keep an encrypted folder's plaintext after its unmount.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Holders {
    /// A private bind (`rprivate`, `private`...): a copy of the mount it started with, which
    /// an unmount on the host does not reach. The lock is refused.
    pub blocking: Vec<String>,
    /// A bind with propagation `rslave`/`slave` of the folder itself or of a folder inside it:
    /// it follows later mounts, but one made while the folder was mounted keeps that mount
    /// after the host unmounts it. Stopped for the lock and started again: from then on it
    /// follows the folder.
    pub restart: Vec<String>,
}

/// [`Holders`] of `path` among `list` (`docker inspect` objects of running containers). A
/// follower bind of a folder strictly holding `path` loses the mount with the host: no
/// holder.
fn holders_in_inspect(list: &[Value], path: &str) -> Holders {
    let mut h = Holders::default();
    for c in list {
        let Some(name) = container_name(c) else { continue };
        let (mut block, mut restart) = (false, false);
        for (src, prop) in mounts_of(c) {
            let inside = folders::is_within(&src, path);
            let holding = folders::is_within(path, &src);
            if !inside && !holding {
                continue;
            }
            if !matches!(prop.as_str(), "rslave" | "slave") {
                block = true;
            } else if inside {
                restart = true;
            }
        }
        if block {
            h.blocking.push(name);
        } else if restart {
            h.restart.push(name);
        }
    }
    h
}

/// The running containers with a mount of `path` or of a folder inside it (a bind of a
/// folder holding it sees the new subvolume by itself); `Err` when Docker cannot be asked.
/// A share rollback restarts them around the subvolume swap, or they keep writing into the
/// old subvolume, renamed aside.
pub(crate) fn running_users(path: &str) -> Result<Vec<String>, String> {
    inspect_containers(true).map(|l| binders_in_inspect(&l, path))
}

fn binders_in_inspect(list: &[Value], path: &str) -> Vec<String> {
    list.iter().filter(|c| mounts_of(c).iter().any(|(src, _)| folders::is_within(src, path))).filter_map(container_name).collect()
}

/// [`Holders`] of `path` among the running containers; `Err` when Docker cannot be asked.
pub(crate) fn lock_holders(path: &str) -> Result<Holders, String> {
    inspect_containers(true).map(|l| holders_in_inspect(&l, path))
}

/// Whether a `/proc/<pid>/mountinfo` text holds a mount whose source is `cipher` (the
/// gocryptfs mount of an encrypted folder).
fn mountinfo_holds(text: &str, cipher: &str) -> bool {
    let unescape = |s: &str| s.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\");
    text.lines().any(|l| {
        let Some((_, right)) = l.split_once(" - ") else { return false };
        let mut it = right.split(' ');
        let (fstype, source) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        fstype.starts_with("fuse") && unescape(source) == cipher
    })
}

/// The running containers whose mount namespace still holds the gocryptfs mount of `cipher`
/// (from `/proc/<State.Pid>/mountinfo`), after an unmount; `Err` when Docker cannot be asked.
pub(crate) fn mount_holders(cipher: &str) -> Result<Vec<String>, String> {
    Ok(holders_by_mountinfo(&inspect_containers(true)?, cipher, &|pid| std::fs::read_to_string(format!("/proc/{pid}/mountinfo")).ok()))
}

fn holders_by_mountinfo(list: &[Value], cipher: &str, read: &dyn Fn(u64) -> Option<String>) -> Vec<String> {
    list.iter()
        .filter(|c| {
            let pid = c.pointer("/State/Pid").and_then(Value::as_u64).unwrap_or(0);
            // a container that stopped meanwhile holds nothing
            pid > 0 && read(pid).is_some_and(|t| mountinfo_holds(&t, cipher))
        })
        .filter_map(container_name)
        .collect()
}

/// `docker stop` / `docker start` of a container around a lock, logged.
pub(crate) fn docker_cmd(action: &str, name: &str) -> Result<(), String> {
    tracing::info!("encrypted folder lock: docker {action} {name}");
    rynas_tasks::Cmd::new("docker")
        .args([action, name])
        .timeout(std::time::Duration::from_secs(60))
        .run_blocking()
        .map(|_| ())
        .map_err(|e| {
            tracing::warn!("encrypted folder lock: docker {action} {name}: {e}");
            e.to_string()
        })
}

/// What else uses `path`; `share` is deleted with it (its own jobs go too).
fn usages(
    s: &Services,
    owner: &str,
    path: &str,
    share: Option<&Share>,
) -> Res<(Usages, Vec<String>)> {
    let jobs = jobs_of(s, owner, path)?;
    let (snaps, _, _): (SnapshotsConfig, SnapshotsConfig, _) =
        docs(&s.ops.config, owner, SNAPSHOTS_KEY)?;
    let share_jobs: Vec<String> = share
        .map(|sh| {
            snaps
                .jobs
                .iter()
                .filter(|j| j.share.as_deref() == Some(sh.name.as_str()))
                .map(|j| j.name.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut removed_jobs: Vec<String> = share_jobs;
    let mut kept = Vec::new();
    for (j, _) in jobs {
        if share.is_some() {
            if !removed_jobs.contains(&j) {
                removed_jobs.push(j);
            }
        } else {
            kept.push(j);
        }
    }
    let others: Vec<String> = all_shares(s, owner)?
        .into_iter()
        .filter(|x| share.is_none_or(|sh| sh.name != x.name))
        .filter(|x| folders::is_within(&x.path, path))
        .map(|x| x.name)
        .collect();
    let (sched, _, _): (SchedulerConfig, SchedulerConfig, _) =
        docs(&s.ops.config, owner, SCHEDULER_KEY)?;
    let tasks = sched
        .tasks
        .iter()
        .filter(|t| match &t.action {
            TaskAction::Command { command, .. } => command.contains(path),
            TaskAction::Script { script, .. } => script.contains(path),
            TaskAction::RecycleCleanup { share: Some(n), .. } => {
                share.is_some_and(|sh| &sh.name == n)
            }
            _ => false,
        })
        .map(|t| if t.name.is_empty() { t.id.clone() } else { t.name.clone() })
        .collect();
    Ok((
        Usages {
            share: share.map(|x| x.name.clone()),
            own_nfs: share.is_some_and(|x| x.nfs.enabled),
            shares: others,
            jobs_removed: removed_jobs.clone(),
            jobs_kept: kept,
            tasks,
            containers: containers_using(path, false),
        },
        removed_jobs,
    ))
}

/// The plan of deleting a share with its folder, its snapshot directories and the jobs
/// removed with it.
fn delete_plan(
    s: &Services,
    owner: &str,
    share: &Share,
) -> Res<(FolderDeletePlan, Vec<SnapshotSource>, Vec<String>)> {
    let sources = snapshot_sources(s, owner, &share.path, Some(share))?;
    let (u, jobs) = usages(s, owner, &share.path, Some(share))?;
    let p = folders::plan(
        s.shares.host().as_ref(),
        &s.shares.volumes(),
        &share.path,
        &sources,
        u,
    );
    Ok((p, sources, jobs))
}

async fn share_delete_plan(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<ShareDeletePlanRequest>, JsonRejection>,
) -> Res<Json<FolderDeletePlan>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let req = json_body(body)?;
    Ok(Json(
        blocking(&s, move |s| {
            let share = all_shares(s, &owner)?
                .into_iter()
                .find(|x| x.name.eq_ignore_ascii_case(&req.share))
                .ok_or_else(|| not_found(format!("share {}", req.share)))?;
            Ok(delete_plan(s, &owner, &share)?.0)
        })
        .await?,
    ))
}

#[derive(Deserialize)]
struct FoldersQuery {
    /// Measure the sizes that are unknown or older than an hour (never on a volume whose
    /// disks sleep); without it the list only has the sizes already known.
    #[serde(default)]
    sizes: bool,
}

async fn folders_list(
    State(st): State<SvcState>,
    h: HeaderMap,
    Query(q): Query<FoldersQuery>,
) -> Res<Json<Vec<UnsharedFolder>>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    Ok(Json(
        blocking(&s, move |s| {
            // the homes folder and rynas's own folders (Docker) count as used, like a share
            let mut paths: Vec<String> =
                all_shares(s, &owner)?.into_iter().map(|x| x.path).collect();
            paths.extend(homes::homes_folders(s));
            paths.extend(rynas_folders(s, &owner));
            paths.extend(pc_backup_folders(s));
            let removed = s.shares.removed().list();
            let (snaps, _, _): (SnapshotsConfig, SnapshotsConfig, _) =
                docs(&s.ops.config, &owner, SNAPSHOTS_KEY)?;
            let of = |p: &str| -> Vec<SnapshotSource> {
                let mut v: Vec<SnapshotSource> = snaps
                    .jobs
                    .iter()
                    .filter(|j| j.subvolume.trim_end_matches('/') == p)
                    .map(|j| SnapshotSource {
                        dir: j.snapshot_dir.clone(),
                        prefix: j.name.clone(),
                    })
                    .collect();
                if let Some(r) = removed.iter().rev().find(|r| r.share.path == p) {
                    let sc = &r.share.smb.shadow_copy;
                    if !sc.snapshot_dir.is_empty() {
                        v.push(SnapshotSource {
                            dir: sc.snapshot_dir.trim_end_matches('/').to_string(),
                            prefix: rynas_services::smb::shadow_prefix(&r.share),
                        });
                    }
                }
                v.sort_by(|a, b| (&a.dir, &a.prefix).cmp(&(&b.dir, &b.prefix)));
                v.dedup();
                v
            };
            let asleep = |m: &str| s.shares.asleep(m);
            let measure = folders::Measure {
                now_ms: rynas_tasks::now_ms(),
                asleep: &asleep,
            };
            Ok(folders::unshared(
                s.shares.host().as_ref(),
                &s.shares.volumes(),
                &paths,
                &removed,
                &of,
                s.shares.sizes(),
                q.sizes.then_some(&measure),
            ))
        })
        .await?,
    ))
}

/// A disk under the filesystem holding `path` (the longest mount point containing it)
/// sleeps: its files must not be read now (share sizes, the SSH page's key counts).
pub fn path_asleep(shares: &rynas_services::SharesDomain, path: &str) -> bool {
    let mounts = rynas_storage::mounts::parse_mountinfo(&crate::places::mountinfo());
    mounts
        .iter()
        .filter(|m| folders::is_within(path, &m.mountpoint))
        .max_by_key(|m| m.mountpoint.len())
        .is_some_and(|m| shares.asleep(&m.mountpoint))
}

/// `GET /v1/share-sizes`: the sizes known now; the unknown and old ones are measured in
/// the background ([`folders::ShareSizeMeter`]), the client asks again while `measuring`.
async fn share_sizes(State(st): State<SvcState>, h: HeaderMap) -> Res<Json<ShareSizes>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    Ok(Json(
        blocking(&s, move |s| {
            let host = s.shares.host();
            let list: Vec<folders::ShareFolder> = all_shares(s, &owner)?
                .into_iter()
                .filter_map(|sh| {
                    // an encrypted folder: its ciphertext, whether locked or not
                    let dir = if sh.encrypted {
                        rynas_services::crypt::cipher_dir(&sh.path)?
                    } else {
                        sh.path.clone()
                    };
                    let info = host.probe(&dir);
                    // a folder still to create, or gone: nothing to measure
                    (info.exists && info.is_dir && !info.symlink).then_some(folders::ShareFolder {
                        name: sh.name,
                        path: sh.path,
                        dir,
                        subvolume: info.subvolume,
                    })
                })
                .collect();
            let meter = s.shares.share_sizes();
            let (out, start) = meter.report(
                &s.shares.volumes(),
                &list,
                rynas_tasks::now_ms(),
                &|p: &str| path_asleep(&s.shares, p),
            );
            if start {
                let shares = s.shares.clone();
                meter.start(host.clone(), Arc::new(move |p: &str| path_asleep(&shares, p)));
            }
            Ok(out)
        })
        .await?,
    ))
}

/// An existing folder without a share: its path, or why not.
fn unshared_path(s: &Services, owner: &str, r: &FolderRef) -> Res<String> {
    let vols = s.shares.volumes();
    let (_, mnt) = vols
        .iter()
        .find(|(l, _)| *l == r.volume)
        .ok_or_else(|| not_found(format!("volume {}", r.volume)))?;
    if r.name.is_empty() || r.name.contains('/') || r.name == ".." {
        return Err(Error::invalid("name", "invalid folder name"));
    }
    let path = format!("{}/{}", mnt.trim_end_matches('/'), r.name);
    if folders::on_volume(&vols, &path).is_none() {
        return Err(Error::invalid(
            "name",
            format!("{} is a system folder", r.name),
        ));
    }
    let info = s.shares.host().probe(&path);
    if !info.exists || !info.is_dir {
        return Err(not_found(format!("folder {path}")));
    }
    if homes::homes_folders(s)
        .iter()
        .any(|h| folders::is_within(h, &path))
    {
        return Err(conflict(
            error_codes::CONFLICT,
            format!("{path} holds the users' homes: it is not deleted"),
        ));
    }
    if rynas_folders(s, owner)
        .iter()
        .any(|r| folders::is_within(r, &path))
    {
        return Err(conflict(
            error_codes::CONFLICT,
            format!("{path} holds Docker and the apps' data: it is not deleted"),
        ));
    }
    if pc_backup_folders(s).iter().any(|r| folders::is_within(r, &path)) {
        return Err(conflict(
            error_codes::CONFLICT,
            format!("{path} holds the PCs' backups: it is not deleted"),
        ));
    }
    if let Some(sh) = all_shares(s, owner)?
        .into_iter()
        .find(|x| folders::is_within(&x.path, &path))
    {
        return Err(conflict(
            error_codes::CONFLICT,
            format!("share {} uses {path}: delete the share instead", sh.name),
        ));
    }
    Ok(path)
}

/// The folders of the PCs' backups: the btrfs method's (received snapshots, one per PC,
/// e.g. `/volume1/pc-backups/<pc>`) and the file method's (its folder for new groups and
/// each group's repository). The top-level folder holding one is in use: never listed as
/// unshared nor deletable from there (from the registries only, no disk access).
fn pc_backup_folders(s: &Services) -> Vec<String> {
    let mut out: Vec<String> = s.ops.pc_backup.get().map(|pc| pc.snapshot_folders().into_iter().map(|(_, p)| p).collect()).unwrap_or_default();
    if let Some(a) = s.ops.pc_agent.get().and_then(std::sync::Weak::upgrade) {
        let r = a.snapshot();
        out.push(r.settings.folder.clone());
        out.extend(r.groups.values().map(|g| g.repo.clone()));
    }
    out.retain(|p| p.starts_with('/'));
    out
}

/// Folders rynas itself keeps on the volumes, never "unshared" nor deletable from there:
/// the Docker folder of the chosen volume (`/<vol>/docker`: engine, stacks, the apps' data)
/// and the engine's data-root when daemon.json puts it on a volume.
fn rynas_folders(s: &Services, owner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let vols = s.shares.volumes();
    let docker: Option<rynas_api_types::docker::DockerSettings> =
        docs(&s.ops.config, owner, crate::docker::DOCKER_KEY).ok().and_then(|(_, cur, applied)| applied.or(Some(cur)));
    if let Some(label) = docker.and_then(|d| d.volume)
        && let Some((_, mnt)) = vols.iter().find(|(l, _)| *l == label)
    {
        out.push(format!("{}/docker", mnt.trim_end_matches('/')));
    }
    let daemon = std::fs::read_to_string(rynas_docker::engine::DAEMON_JSON).ok();
    if let Some(dr) = rynas_docker::engine::data_root_of(daemon.as_deref()) {
        out.push(dr.trim_end_matches('/').to_string());
    }
    out
}

fn folder_plan(s: &Services, owner: &str, r: &FolderRef) -> Res<(FolderDeletePlan, Vec<SnapshotSource>)> {
    let path = unshared_path(s, owner, r)?;
    let sources = snapshot_sources(s, owner, &path, None)?;
    let (u, _) = usages(s, owner, &path, None)?;
    let p = folders::plan(
        s.shares.host().as_ref(),
        &s.shares.volumes(),
        &path,
        &sources,
        u,
    );
    Ok((p, sources))
}

async fn folder_delete_plan(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<FolderRef>, JsonRejection>,
) -> Res<Json<FolderDeletePlan>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let r = json_body(body)?;
    Ok(Json(
        blocking(&s, move |s| Ok(folder_plan(s, &owner, &r)?.0)).await?,
    ))
}

#[derive(Deserialize)]
struct ConfirmQuery {
    confirm: Option<String>,
}

/// Task `shares.folder-delete`: deletes a folder without a share, its snapshots and its
/// recycle bin (`?confirm=<name>`).
async fn folder_delete(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path((volume, name)): Path<(String, String)>,
    Query(q): Query<ConfirmQuery>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    if q.confirm.as_deref() != Some(name.as_str()) {
        return Err(bad(
            error_codes::CONFIRMATION_REQUIRED,
            format!("deleting a folder needs confirm={name}"),
        ));
    }
    let r = FolderRef { volume, name };
    let o = owner.clone();
    let (p, sources) = blocking(&s, move |s| folder_plan(s, &o, &r)).await?;
    if !p.allowed {
        return Err(conflict(
            error_codes::CONFLICT,
            format!(
                "{} cannot be deleted: {}",
                p.path,
                p.blockers
                    .iter()
                    .map(|b| format!("{:?} {}", b.kind, b.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let svc = s.clone();
    let id = s
        .ops
        .tasks
        .spawn("shares.folder-delete", &owner, move |ctx| async move {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let log = |l: &str| c.log(l);
                let r = folders::remove_folder(
                    svc.ops.config.system(),
                    svc.shares.host().as_ref(),
                    svc.shares.crypt().as_ref(),
                    &svc.shares.volumes(),
                    &p.path,
                    &sources,
                    &log,
                );
                svc.shares.sizes().clear();
                r?;
                svc.shares.removed().forget(&[&p.path])
            })
            .await
            .map_err(|e| e.to_string())?
        })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

/// Task `shares.nocow`: turns no copy-on-write on (empty folder only) or off for the folder
/// of an applied share. The folder is checked before the task starts and again inside it.
async fn share_nocow(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<ShareNocowRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let req = json_body(body)?;
    let o = owner.clone();
    let path = blocking(&s, move |s| {
        let (_, cur, _): (SharesConfig, SharesConfig, _) = docs(&s.ops.config, &o, SHARES_KEY)?;
        let sh = cur
            .shares
            .iter()
            .find(|x| x.name.eq_ignore_ascii_case(&name))
            .ok_or_else(|| not_found(format!("applied share {name}")))?;
        // the checks first, so the request answers 409 with the reason
        rynas_services::nocow::check(s.shares.host().as_ref(), &sh.path, req.enabled).map_err(|e| match e {
            rynas_services::nocow::Refusal::NotEmpty(m) => conflict(error_codes::FOLDER_NOT_EMPTY, m),
            rynas_services::nocow::Refusal::Invalid(m) => conflict(error_codes::CONFLICT, m),
        })?;
        Ok(sh.path.clone())
    })
    .await?;
    let svc = s.clone();
    let on = req.enabled;
    let id = s.ops.tasks.spawn("shares.nocow", &owner, move |ctx| async move {
        let c = ctx.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let log = |l: &str| c.log(l);
            rynas_services::nocow::change(svc.ops.config.system(), svc.shares.host().as_ref(), &path, on, &log)
        })
        .await
        .map_err(|e| e.to_string())?
    })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

#[derive(Deserialize)]
struct EffectiveQuery {
    principal: String,
}

async fn share_effective(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
    Query(q): Query<EffectiveQuery>,
) -> Res<Json<EffectivePermission>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    Ok(Json(
        blocking(&s, move |s| {
            let (doc, _, _): (SharesConfig, SharesConfig, _) =
                docs(&s.ops.config, &owner, SHARES_KEY)?;
            let sh = doc
                .shares
                .iter()
                .find(|x| x.name.eq_ignore_ascii_case(&name))
                .ok_or_else(|| not_found(format!("share {name}")))?;
            let who = Principal::parse(&q.principal);
            let groups = match who {
                Principal::User(u) => user_groups(s, &owner, u)?,
                Principal::Group(_) => Vec::new(),
            };
            Ok(effective(&sh.name, &sh.permissions, who, &groups))
        })
        .await?,
    ))
}

/// Groups of a user: the staged configuration plus the system (primary and supplementary).
fn user_groups(s: &Services, owner: &str, name: &str) -> Res<Vec<String>> {
    let (doc, _, _): (UsersConfig, UsersConfig, _) = docs(&s.ops.config, owner, USERS_KEY)?;
    let acc = s.users.accounts();
    let sys_users = acc.users().map_err(internal)?;
    let sys_groups = acc.groups().map_err(internal)?;
    let configured = doc.users.iter().find(|u| u.name == name);
    let on_system = sys_users.iter().find(|u| u.name == name);
    if configured.is_none() && on_system.is_none() {
        return Err(not_found(format!("user {name}")));
    }
    let mut g: BTreeSet<String> = BTreeSet::new();
    if let Some(u) = configured {
        g.extend(u.groups.iter().cloned());
    }
    if let Some(u) = on_system {
        for x in &sys_groups {
            if x.gid == u.gid || x.members.iter().any(|m| m == name) {
                g.insert(x.name.clone());
            }
        }
    }
    Ok(g.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect(name: &str, source: &str, propagation: &str, pid: u64) -> Value {
        serde_json::json!({ "Name": format!("/{name}"), "State": { "Pid": pid }, "Mounts": [
            { "Type": "bind", "Source": "/vol1/docker/stacks/x/config", "Destination": "/config", "Propagation": "rprivate" },
            { "Type": "bind", "Source": source, "Destination": "/shares/secret", "Propagation": propagation }
        ]})
    }

    fn list() -> Vec<Value> {
        vec![
            inspect("private", "/vol1/secret", "rprivate", 11),
            inspect("follows", "/vol1/secret", "rslave", 12),
            inspect("slave-sub", "/vol1/secret/sub", "slave", 13),
            inspect("parent-follows", "/vol1", "rslave", 14),
            inspect("parent-private", "/vol1", "rprivate", 15),
            inspect("elsewhere", "/vol1/other", "rprivate", 16),
        ]
    }

    #[test]
    fn lock_holders_by_propagation() {
        let h = holders_in_inspect(&list(), "/vol1/secret");
        // a private copy blocks; a follower of the folder itself or inside it is restarted
        // around the lock (a bind made while mounted keeps the mount); a follower of a folder
        // holding it loses the mount with the host
        assert_eq!(h.blocking, ["private", "parent-private"]);
        assert_eq!(h.restart, ["follows", "slave-sub"]);
        // the usages of a folder count every container that mounts it
        assert_eq!(users_in_inspect(&list(), "/vol1/secret"), ["private", "follows", "slave-sub", "parent-follows", "parent-private"]);
        // a rollback restarts the binds of the share and inside it, whatever the propagation
        assert_eq!(binders_in_inspect(&list(), "/vol1/secret"), ["private", "follows", "slave-sub"]);
    }

    #[test]
    fn docker_engine_and_socket_off_means_no_container() {
        assert!(docker_off("inactive\ninactive\n"));
        assert!(docker_off("failed\ninactive\n"));
        assert!(!docker_off("active\ninactive\n"));
        assert!(!docker_off("inactive\nactive\n"));
        assert!(!docker_off("inactive\nactivating\n"));
        // systemd did not answer as expected: ask Docker
        assert!(!docker_off(""));
        assert!(!docker_off("inactive\n"));
    }

    #[test]
    fn mountinfo_backstop() {
        let held = "36 25 0:32 / /config rw,relatime - ext4 /dev/sda1 rw\n\
                    41 36 0:45 / /shares/secret rw,nosuid,nodev,relatime shared:5 master:3 - fuse.gocryptfs /vol1/.rynas-crypt/secret rw,user_id=0\n";
        let gone = "36 25 0:32 / /config rw,relatime - ext4 /dev/sda1 rw\n41 36 0:44 / /shares/secret ro - btrfs /dev/md2 rw\n";
        let spaced = "41 36 0:45 /sub /shares/x rw - fuse.gocryptfs /vol1/.rynas-crypt/my\\040docs rw\n";
        assert!(mountinfo_holds(held, "/vol1/.rynas-crypt/secret"));
        assert!(!mountinfo_holds(gone, "/vol1/.rynas-crypt/secret"));
        assert!(!mountinfo_holds(held, "/vol1/.rynas-crypt/secret2"));
        assert!(mountinfo_holds(spaced, "/vol1/.rynas-crypt/my docs"));
        let read = |pid: u64| match pid {
            12 => Some(held.to_string()),
            13 => None,
            _ => Some(gone.to_string()),
        };
        assert_eq!(holders_by_mountinfo(&list(), "/vol1/.rynas-crypt/secret", &read), ["follows"]);
    }
}
