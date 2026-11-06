//! User homes endpoints (`/v1/services/homes`): the setting (key `homes` plus the SMB
//! `[homes]` share of `shares`, staged together), where every local user's home is, and the
//! home move (plan, task, confirmation, undo). See [`rynas_services::homes`].

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rynas_api_types::{
    HOMES_KEY, HOMES_LINK, HomeMovePlan, HomeMoveRecord, HomeMoveRequest, HomesConfig,
    HomesSettings, HomesState, LOCAL_ID_MAX, LOCAL_ID_MIN, SharesConfig, SmbHomes, TaskStarted,
    UserHome, UsersConfig, error_codes,
};
use rynas_services::accounts::{AccountOp, SysUser};
use rynas_services::homes::{self, MoveContext};
use rynas_services::{SHARES_KEY, USERS_KEY};

use super::{
    Services, SvcState, all_shares, blocking, conflict, docs, internal, json_body, not_found,
    stage, svc,
};
use crate::ops::{Error, Res, user};

fn local(u: &SysUser) -> bool {
    (LOCAL_ID_MIN..=LOCAL_ID_MAX).contains(&u.uid)
        && rynas_services::validate::account_name(&u.name)
}

/// The homes document as `owner` sees it; disabled when the key is not known (tests).
pub(super) fn homes_doc(s: &Services, owner: &str) -> HomesConfig {
    docs::<HomesConfig>(&s.ops.config, owner, HOMES_KEY)
        .map(|d| d.0)
        .unwrap_or_default()
}

/// The folders the homes use (the link's target and the applied one): never "unshared".
pub(super) fn homes_folders(s: &Services) -> Vec<String> {
    let mut v: Vec<String> = s.homes.current_folder().into_iter().collect();
    if let Ok(Some(a)) = s.ops.config.applied(HOMES_KEY)
        && let Ok(c) = serde_json::from_value::<HomesConfig>(a)
        && !c.volume.is_empty()
    {
        v.push(homes::folder_of(&c.volume));
    }
    v.sort();
    v.dedup();
    v
}

fn state(s: &Services, owner: &str) -> Res<HomesState> {
    let c = &s.ops.config;
    let (eff, cur, _): (HomesConfig, HomesConfig, _) = docs(c, owner, HOMES_KEY)?;
    let (sh_eff, sh_cur, _): (SharesConfig, SharesConfig, _) = docs(c, owner, SHARES_KEY)?;
    let host = s.homes.host();
    let folder = if eff.volume.is_empty() {
        String::new()
    } else {
        homes::folder_of(&eff.volume)
    };
    let subvolume = !folder.is_empty() && host.probe(&folder).subvolume;
    let sys = s.users.accounts().users().map_err(internal)?;
    let moves = s.moves.list();
    let mut list: Vec<UserHome> = sys
        .iter()
        .filter(|u| local(u))
        .map(|u| {
            let (location, subvolume) = homes::location(host.as_ref(), &u.home);
            UserHome {
                user: u.name.clone(),
                uid: u.uid,
                home: u.home.clone(),
                location,
                subvolume,
                moved: moves.iter().find(|m| m.user == u.name).cloned(),
            }
        })
        .collect();
    list.sort_by(|a, b| a.user.cmp(&b.user));
    Ok(HomesState {
        pending: eff != cur || sh_eff.smb.homes != sh_cur.smb.homes,
        settings: HomesSettings {
            enabled: eff.enabled,
            volume: eff.volume,
            smb: sh_eff.smb.homes,
        },
        link: HOMES_LINK.into(),
        target: host.read_link(HOMES_LINK),
        folder,
        subvolume,
        volumes: homes::candidates(&host.mounts(), &s.shares.volumes()),
        homes: list,
    })
}

pub(super) async fn homes_get(State(st): State<SvcState>, h: HeaderMap) -> Res<Json<HomesState>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    Ok(Json(blocking(&s, move |s| state(s, &owner)).await?))
}

pub(super) async fn homes_put(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<HomesSettings>, JsonRejection>,
) -> Res<Json<HomesState>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let v = json_body(body)?;
    Ok(Json(
        blocking(&s, move |s| {
            let c = &s.ops.config;
            let doc = HomesConfig {
                enabled: v.enabled,
                volume: v.volume.clone(),
            };
            stage(c, &owner, HOMES_KEY, &doc)?;
            // the [homes] share follows the service: off with it
            let (mut sh, _, _): (SharesConfig, SharesConfig, _) = docs(c, &owner, SHARES_KEY)?;
            let want = if v.enabled {
                v.smb.clone()
            } else {
                SmbHomes {
                    enabled: false,
                    recycle: v.smb.recycle.clone(),
                }
            };
            if sh.smb.homes != want {
                sh.smb.homes = want;
                super::stage_shares(s, &owner, |d| {
                    d.smb.homes = sh.smb.homes.clone();
                    Ok(())
                })?;
            }
            state(s, &owner)
        })
        .await?,
    ))
}

/// The service is on in the applied configuration and the link points at its folder.
fn service_on(s: &Services) -> bool {
    let applied: Option<HomesConfig> = s
        .ops
        .config
        .applied(HOMES_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok());
    applied.is_some_and(|c| {
        c.enabled
            && s.homes.current_folder().as_deref() == Some(homes::folder_of(&c.volume).as_str())
    })
}

/// Plans for `users` (empty: every local user whose home is not in the homes folder).
fn plans(s: &Services, owner: &str, users: &[String]) -> Res<Vec<(SysUser, HomeMovePlan)>> {
    let sys = s.users.accounts().users().map_err(internal)?;
    let targets: Vec<SysUser> = if users.is_empty() {
        sys.into_iter()
            .filter(|u| local(u) && homes::link_user(&u.home).is_none())
            .collect()
    } else {
        let mut v = Vec::new();
        for n in users {
            let u = sys
                .iter()
                .find(|u| &u.name == n)
                .ok_or_else(|| not_found(format!("user {n}")))?;
            v.push(u.clone());
        }
        v
    };
    let shares: Vec<(String, String)> = all_shares(s, owner)?
        .into_iter()
        .map(|x| (x.name, x.path))
        .collect();
    let on = service_on(s);
    let host = s.homes.host();
    Ok(targets
        .into_iter()
        .map(|u| {
            let cx = MoveContext {
                service_on: on,
                shares: &shares,
                waiting: s.moves.get(&u.name).is_some(),
            };
            let p = homes::plan_move(host.as_ref(), &u, &cx);
            (u, p)
        })
        .collect())
}

pub(super) async fn move_plan(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<HomeMoveRequest>, JsonRejection>,
) -> Res<Json<Vec<HomeMovePlan>>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let req = json_body(body)?;
    Ok(Json(
        blocking(&s, move |s| {
            Ok(plans(s, &owner, &req.users)?
                .into_iter()
                .map(|(_, p)| p)
                .collect())
        })
        .await?,
    ))
}

/// Changes the passwd home of `user`: through the `users` key when rynas manages the user (a
/// revision, `usermod -d` by its reconcile), else `usermod -d` directly.
fn switch_home(
    s: &Services,
    author: &str,
    name: &str,
    home: &str,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    let c = &s.ops.config;
    let applied: Option<UsersConfig> = c
        .applied(USERS_KEY)
        .map_err(|e| e.to_string())?
        .and_then(|v| serde_json::from_value(v).ok());
    if let Some(mut doc) = applied
        && let Some(u) = doc.users.iter_mut().find(|u| u.name == name)
    {
        u.home = home.to_string();
        let v = serde_json::to_value(&doc).map_err(|e| e.to_string())?;
        c.apply_value(
            author,
            USERS_KEY,
            Some(v),
            &format!("home of {name}: {home}"),
            log,
        )
        .map_err(|e| e.to_string())?;
        return Ok(());
    }
    let op = AccountOp::UserMod {
        name: name.into(),
        full_name: None,
        shell: None,
        expire: None,
        home: Some(home.into()),
    };
    log(&op.describe());
    s.users.accounts().exec(&op)
}

/// Moves one home (task body, blocking).
fn move_one(
    s: &Services,
    author: &str,
    name: &str,
    log: &dyn Fn(&str),
    progress: &dyn Fn(u8),
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    let mut found = plans(s, author, &[name.to_string()]).map_err(|e| e.2.clone())?;
    let (u, plan) = found
        .pop()
        .ok_or_else(|| format!("user {name} not found"))?;
    let host = s.homes.host();
    homes::copy_home(host.as_ref(), &plan, &u, log, progress, cancelled)?;
    let live = homes::live_blockers(host.as_ref(), &u);
    let undo_copy = |why: String| -> String {
        let _ = host.remove(&plan.target, plan.subvolume, log);
        why
    };
    if !live.is_empty() {
        return Err(undo_copy(format!(
            "{name}: the user started using the home: {}; the copy was removed",
            homes::blockers_text(&live)
        )));
    }
    if cancelled() {
        return Err(undo_copy(format!(
            "{name}: cancelled; the copy was removed"
        )));
    }
    if let Err(e) = switch_home(s, author, name, &plan.to, log) {
        return Err(undo_copy(format!(
            "{name}: the passwd home could not change ({e}); the copy was removed"
        )));
    }
    let now = rynas_tasks::now_ms();
    let kept = match homes::keep_old(host.as_ref(), &plan.from, now, log) {
        Ok(k) => k,
        Err(e) => {
            log(&format!(
                "{name}: the old home could not be renamed ({e}): it stays at {}",
                plan.from
            ));
            plan.from.clone()
        }
    };
    s.moves.put(HomeMoveRecord {
        user: name.into(),
        from: plan.from.clone(),
        to: plan.to.clone(),
        kept: kept.clone(),
        moved_ms: now,
    })?;
    log(&format!(
        "{name}: home moved to {} ({}); the old one is kept as {kept} until the move is confirmed",
        plan.to, plan.target
    ));
    Ok(())
}

/// Task `homes.move`: the homes of `users` (empty: every one that can move), one after the
/// other. Blocked homes are reported and skipped; the task fails if any home did not move.
pub(super) async fn move_start(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<HomeMoveRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let req = json_body(body)?;
    let o = owner.clone();
    let list = blocking(&s, move |s| plans(s, &o, &req.users)).await?;
    let allowed: Vec<String> = list
        .iter()
        .filter(|(_, p)| p.allowed)
        .map(|(_, p)| p.user.clone())
        .collect();
    let blocked: Vec<String> = list
        .iter()
        .filter(|(_, p)| !p.allowed)
        .map(|(_, p)| format!("{}: {}", p.user, homes::blockers_text(&p.blockers)))
        .collect();
    if allowed.is_empty() {
        return Err(conflict(
            error_codes::CONFLICT,
            if blocked.is_empty() {
                "no home to move".to_string()
            } else {
                format!("no home can move now: {}", blocked.join("; "))
            },
        ));
    }
    let svc = s.clone();
    let author = owner.clone();
    let id = s
        .ops
        .tasks
        .spawn("homes.move", &owner, move |ctx| async move {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                for b in &blocked {
                    c.log(&format!("skipped {b}"));
                }
                let n = allowed.len();
                let mut failed = Vec::new();
                for (i, name) in allowed.iter().enumerate() {
                    let base = (i * 100 / n) as u8;
                    let span = (100 / n) as u8;
                    let progress = |p: u8| c.progress(base + (p as u32 * span as u32 / 100) as u8);
                    if let Err(e) = move_one(&svc, &author, name, &|l| c.log(l), &progress, &|| {
                        c.is_cancelled()
                    }) {
                        c.log(&e);
                        failed.push(name.clone());
                        if c.is_cancelled() {
                            break;
                        }
                    }
                }
                if failed.is_empty() && blocked.is_empty() {
                    Ok(())
                } else if failed.is_empty() {
                    Err(format!("{} homes skipped (see the log)", blocked.len()))
                } else {
                    Err(format!("not moved: {}", failed.join(", ")))
                }
            })
            .await
            .map_err(|e| e.to_string())?
        })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

fn record(s: &Services, name: &str) -> Res<HomeMoveRecord> {
    s.moves
        .get(name)
        .ok_or_else(|| not_found(format!("a home move of {name} waiting for confirmation")))
}

/// Task `homes.move-finish`: the move is confirmed, the old home is deleted.
pub(super) async fn move_finish(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let n = name.clone();
    let r = blocking(&s, move |s| record(s, &n)).await?;
    let svc = s.clone();
    let id = s
        .ops
        .tasks
        .spawn("homes.move-finish", &owner, move |ctx| async move {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                homes::finish_move(svc.homes.host().as_ref(), &r, &|l| c.log(l))?;
                svc.moves.remove(&r.user)
            })
            .await
            .map_err(|e| e.to_string())?
        })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

/// Task `homes.move-undo`: the old home goes back in place and becomes the passwd home again;
/// the copy is kept aside in the homes folder (it may hold newer files).
pub(super) async fn move_undo(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let n = name.clone();
    let r = blocking(&s, move |s| {
        let r = record(s, &n)?;
        let sys = s.users.accounts().users().map_err(internal)?;
        let u = sys
            .iter()
            .find(|u| u.name == n)
            .ok_or_else(|| not_found(format!("user {n}")))?;
        let live = homes::live_blockers(s.homes.host().as_ref(), u);
        if !live.is_empty() {
            return Err(conflict(
                error_codes::CONFLICT,
                format!("{n} is using the home: {}", homes::blockers_text(&live)),
            ));
        }
        Ok(r)
    })
    .await?;
    let svc = s.clone();
    let author = owner.clone();
    let id = s
        .ops
        .tasks
        .spawn("homes.move-undo", &owner, move |ctx| async move {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let log = |l: &str| c.log(l);
                let host = svc.homes.host();
                let aside = homes::undo_move(host.as_ref(), &r, rynas_tasks::now_ms(), &log)?;
                if let Err(e) = switch_home(&svc, &author, &r.user, &r.from, &log) {
                    return Err(format!(
                        "the old home is back at {} but the passwd home could not change: {e}",
                        r.from
                    ));
                }
                if let Some(a) = aside {
                    log(&format!(
                        "the copy is kept as {a}: delete it when it is no longer needed"
                    ));
                }
                svc.moves.remove(&r.user)
            })
            .await
            .map_err(|e| e.to_string())?
        })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

/// Default home of a user created now: in the homes folder when the service is on (staged or
/// applied for `owner`). An explicit home there is refused while the service is off.
pub(super) fn default_home(
    s: &Services,
    owner: &str,
    u: &mut rynas_api_types::LocalUser,
) -> Res<()> {
    let on = homes_doc(s, owner).enabled;
    if u.home.is_empty() {
        if on {
            u.home = homes::home_of(&u.name);
        }
    } else if homes::link_user(&u.home).is_some() && !on {
        return Err(Error::invalid(
            "home",
            "the homes folder is used only while the user homes service is on",
        ));
    }
    Ok(())
}

/// A user whose home move waits for confirmation cannot be deleted.
pub(super) fn check_no_waiting_move(s: &Services, name: &str) -> Res<()> {
    if s.moves.get(name).is_some() {
        return Err(conflict(
            error_codes::CONFLICT,
            format!("the home move of {name} waits for confirmation: confirm or undo it first"),
        ));
    }
    Ok(())
}
