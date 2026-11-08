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
    ShareDeletePlanRequest, ShareNocowRequest, ShareStatus, ShareWarning, ShareWarningArgs, ShareWarningCode, ShareView, SharesConfig, SmbSettings, SmbState,
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
        .route("/v1/shares/plan/delete", post(share_delete_plan))
        .route("/v1/folders", get(folders_list))
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
    let samba = acc.samba_users().map_err(internal)?;
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
                users::set_password(accounts.as_ref(), &name, &password, smb, disabled, &|l| {
                    c.log(l)
                })
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
                // a new path gets a new fsid (unless one is given explicitly)
                if sh.path != cur.path && sh.nfs.fsid == cur.nfs.fsid {
                    sh.nfs.fsid.clear();
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

/// Snapshot jobs (as the session user sees them) taking snapshots of `path`.
fn jobs_of(s: &Services, owner: &str, path: &str) -> Res<Vec<(String, SnapshotSource)>> {
    let (snaps, _, _): (SnapshotsConfig, SnapshotsConfig, _) =
        docs(&s.ops.config, owner, SNAPSHOTS_KEY)?;
    Ok(snaps
        .jobs
        .iter()
        .filter(|j| j.subvolume.trim_end_matches('/') == path)
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

/// Containers that mount `path` or something inside it (when Docker is installed).
fn containers_using(path: &str) -> Vec<String> {
    if !std::path::Path::new("/usr/bin/docker").exists() {
        return Vec::new();
    }
    let t = std::time::Duration::from_secs(10);
    let Ok(ids) = rynas_tasks::Cmd::new("docker")
        .args(["ps", "-aq"])
        .timeout(t)
        .run_blocking()
    else {
        return Vec::new();
    };
    let ids: Vec<&str> = ids.stdout.split_whitespace().collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let Ok(o) = rynas_tasks::Cmd::new("docker")
        .args(["inspect", "-f", "{{.Name}}{{range .Mounts}} {{.Source}}{{end}}"])
        .args(ids)
        .timeout(t)
        .run_blocking()
    else {
        return Vec::new();
    };
    o.stdout
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let name = it.next()?.trim_start_matches('/').to_string();
            it.any(|src| folders::is_within(src, path) || folders::is_within(path, src))
                .then_some(name)
        })
        .collect()
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
            containers: containers_using(path),
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
            // the homes folder counts as used, like a share
            let mut paths: Vec<String> =
                all_shares(s, &owner)?.into_iter().map(|x| x.path).collect();
            paths.extend(homes::homes_folders(s));
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
