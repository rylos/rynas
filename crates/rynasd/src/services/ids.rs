//! Adoption of accounts made outside rynas, UID/GID changes and the ownership remap
//! (`/v1/users/adopt`, `/v1/groups/adopt`, `/v1/ids/plan`, `/v1/ids/remap`). See
//! [`rynas_services::idremap`] and `docs/research/identity-quotas-domain.md` (§ Existing
//! accounts and id changes).

use std::path::{Component, PathBuf};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rynas_api_types::{
    AdoptRequest, HomeMoveBlockerCode, IdBlocker, IdBlockerCode, IdKind, IdRemapPlan,
    IdRemapRequest, LOCAL_ID_MAX, LOCAL_ID_MIN, StagedChanges, TaskStarted, UsersConfig,
    error_codes,
};
use rynas_services::idremap::{self, IdMap, RemapOpts};
use rynas_services::{USERS_KEY, homes, users};

use super::{Services, SvcState, blocking, conflict, docs, internal, json_body, stage, svc};
use crate::ops::{Error, Res, user};

/// Time the dry run of the plan may take (a longer walk shows "at least").
const PLAN_BUDGET: Duration = Duration::from_secs(5);
/// Folders outside the data volumes that may be walked.
const EXTRA_ROOTS: [&str; 2] = ["/home", "/srv"];

fn in_range(id: u32) -> bool {
    (LOCAL_ID_MIN..=LOCAL_ID_MAX).contains(&id)
}

// ---------------------------------------------------------------------------------------------
// adoption
// ---------------------------------------------------------------------------------------------

fn adopt_kind(s: &Services, owner: &str, kind: IdKind, names: &[String]) -> Res<u32> {
    let acc = s.users.accounts();
    let sys = users::import_from(
        &acc.users().map_err(internal)?,
        &acc.groups().map_err(internal)?,
        acc.samba_users().map_err(internal)?.as_deref(),
    );
    let c = &s.ops.config;
    let (mut doc, _, _): (UsersConfig, UsersConfig, _) = docs(c, owner, USERS_KEY)?;
    let added =
        users::adopt(&mut doc, &sys, kind, names).map_err(|e| Error::invalid("names", e))?;
    if added.is_empty() {
        return Ok(0);
    }
    stage(c, owner, USERS_KEY, &doc)?;
    Ok(1)
}

/// `POST /v1/users/adopt`: stages the users of the system (1000–59999) the configuration
/// does not have, as they are (no account is created or changed by the apply).
pub(super) async fn users_adopt(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<AdoptRequest>, JsonRejection>,
) -> Res<Json<StagedChanges>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let req = json_body(body)?;
    let staged = blocking(&s, move |s| adopt_kind(s, &owner, IdKind::User, &req.names)).await?;
    Ok(Json(StagedChanges { staged }))
}

/// `POST /v1/groups/adopt`: the same for groups.
pub(super) async fn groups_adopt(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<AdoptRequest>, JsonRejection>,
) -> Res<Json<StagedChanges>> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let req = json_body(body)?;
    let staged = blocking(&s, move |s| adopt_kind(s, &owner, IdKind::Group, &req.names)).await?;
    Ok(Json(StagedChanges { staged }))
}

// ---------------------------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------------------------

fn blocker(code: IdBlockerCode, detail: impl Into<String>) -> IdBlocker {
    IdBlocker {
        code,
        detail: detail.into(),
    }
}

/// Plain text of the blockers (errors, task logs).
pub fn blockers_text(b: &[IdBlocker]) -> String {
    b.iter()
        .map(|x| {
            let what = match x.code {
                IdBlockerCode::NotLocal => "not a local account (1000-59999)",
                IdBlockerCode::Unmanaged => "created outside rynas: adopt it first",
                IdBlockerCode::OutOfRange => "id out of range",
                IdBlockerCode::SameId => "the id is the current one",
                IdBlockerCode::IdInUse => "the id belongs to another account",
                IdBlockerCode::Processes => "running processes",
                IdBlockerCode::SmbSessions => "SMB sessions",
                IdBlockerCode::PathInvalid => "invalid folder",
                IdBlockerCode::BadRequest => "give either new_id or from_id",
                IdBlockerCode::Other => "not available",
            };
            if x.detail.is_empty() {
                what.to_string()
            } else {
                format!("{what} ({})", x.detail)
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// A folder that may be walked: absolute and normalized, a directory (not a link), inside
/// one of `allowed`.
fn path_ok(p: &str, allowed: &[String]) -> bool {
    let path = PathBuf::from(p);
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    let inside = allowed.iter().any(|a| path.starts_with(a));
    inside
        && std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir())
}

/// The plan of `req` and, when `scan`, its dry run.
fn plan(s: &Services, req: &IdRemapRequest, scan: bool) -> Res<IdRemapPlan> {
    let acc = s.users.accounts();
    let sys_users = acc.users().map_err(internal)?;
    let sys_groups = acc.groups().map_err(internal)?;
    let mut b = Vec::new();
    let (current, sys_user) = match req.kind {
        IdKind::User => {
            let u = sys_users
                .iter()
                .find(|u| u.name == req.name)
                .ok_or_else(|| super::not_found(format!("user {}", req.name)))?;
            (u.uid, Some(u.clone()))
        }
        IdKind::Group => {
            let g = sys_groups
                .iter()
                .find(|g| g.name == req.name)
                .ok_or_else(|| super::not_found(format!("group {}", req.name)))?;
            (g.gid, None)
        }
    };
    if !in_range(current) {
        b.push(blocker(IdBlockerCode::NotLocal, current.to_string()));
    }
    let owner_of = |id: u32| -> Option<String> {
        match req.kind {
            IdKind::User => sys_users.iter().find(|u| u.uid == id).map(|u| u.name.clone()),
            IdKind::Group => sys_groups
                .iter()
                .find(|g| g.gid == id)
                .map(|g| g.name.clone()),
        }
    };
    let (from, to, change) = match (req.new_id, req.from_id) {
        (Some(new), None) => {
            if !in_range(new) {
                b.push(blocker(IdBlockerCode::OutOfRange, new.to_string()));
            } else if new == current {
                b.push(blocker(IdBlockerCode::SameId, new.to_string()));
            } else if let Some(n) = owner_of(new) {
                b.push(blocker(IdBlockerCode::IdInUse, n));
            }
            let applied: Option<UsersConfig> = s
                .ops
                .config
                .applied(USERS_KEY)?
                .and_then(|v| serde_json::from_value(v).ok());
            let managed = applied.is_some_and(|a| match req.kind {
                IdKind::User => a.users.iter().any(|u| u.name == req.name),
                IdKind::Group => a.groups.iter().any(|g| g.name == req.name),
            });
            if !managed && in_range(current) {
                b.push(blocker(IdBlockerCode::Unmanaged, ""));
            }
            if let Some(u) = &sys_user {
                for x in homes::live_blockers(s.homes.host().as_ref(), u) {
                    b.push(blocker(
                        match x.code {
                            HomeMoveBlockerCode::Processes => IdBlockerCode::Processes,
                            HomeMoveBlockerCode::SmbSessions => IdBlockerCode::SmbSessions,
                            _ => IdBlockerCode::Other,
                        },
                        x.detail,
                    ));
                }
            }
            (current, new, true)
        }
        (None, Some(old)) => {
            // ids of system accounts and nobody are never taken over
            if !(LOCAL_ID_MIN..65534).contains(&old) {
                b.push(blocker(IdBlockerCode::OutOfRange, old.to_string()));
            } else if old == current {
                b.push(blocker(IdBlockerCode::SameId, old.to_string()));
            } else if let Some(n) = owner_of(old) {
                b.push(blocker(IdBlockerCode::IdInUse, n));
            }
            (old, current, false)
        }
        _ => {
            b.push(blocker(IdBlockerCode::BadRequest, ""));
            (current, current, false)
        }
    };

    // suggested folders: the data volumes, and the home when it is elsewhere
    let mut roots = super::homes::volume_paths(s);
    if let Some(u) = &sys_user {
        let h = homes::folder_of_home(s.homes.host().as_ref(), u);
        if s.homes.host().probe(&h).is_dir && !roots.iter().any(|r| h.starts_with(r.as_str())) {
            roots.push(h);
        }
    }
    roots.sort();
    roots.dedup();
    let mut allowed = super::homes::volume_paths(s);
    allowed.extend(EXTRA_ROOTS.iter().map(|x| x.to_string()));
    let paths: Vec<String> = if req.paths.is_empty() {
        roots.clone()
    } else {
        req.paths
            .iter()
            .map(|p| p.trim_end_matches('/').to_string())
            .map(|p| if p.is_empty() { "/".into() } else { p })
            .collect()
    };
    for p in &paths {
        if !path_ok(p, &allowed) {
            b.push(blocker(IdBlockerCode::PathInvalid, p.clone()));
        }
    }
    if paths.is_empty() {
        b.push(blocker(IdBlockerCode::PathInvalid, ""));
    }
    let fs = s.users.remap_fs().cloned();
    if fs.is_none() {
        b.push(blocker(IdBlockerCode::Other, "ownership remap"));
    }
    let allowed_now = b.is_empty();
    let scan = match (allowed_now && scan, fs) {
        (true, Some(fs)) => {
            let roots: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
            Some(idremap::remap(
                fs.as_ref(),
                &roots,
                &RemapOpts {
                    map: id_map(req.kind, from, to),
                    acls: req.acls,
                    dry_run: true,
                    deadline: Some(Instant::now() + PLAN_BUDGET),
                    cancelled: &|| false,
                    tick: &|_| {},
                },
            ))
        }
        _ => None,
    };
    Ok(IdRemapPlan {
        kind: req.kind,
        name: req.name.clone(),
        from,
        to,
        change,
        roots,
        paths,
        allowed: allowed_now,
        blockers: b,
        scan,
    })
}

fn id_map(kind: IdKind, from: u32, to: u32) -> IdMap {
    match kind {
        IdKind::User => IdMap {
            uid: Some((from, to)),
            gid: None,
        },
        IdKind::Group => IdMap {
            uid: None,
            gid: Some((from, to)),
        },
    }
}

/// `POST /v1/ids/plan`: checks and dry-run counts of an id change or remap (at most
/// [`PLAN_BUDGET`] of walking; `scan.partial` beyond).
pub(super) async fn ids_plan(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<IdRemapRequest>, JsonRejection>,
) -> Res<Json<IdRemapPlan>> {
    let s = svc(&st)?;
    user(&h)?;
    let req = json_body(body)?;
    Ok(Json(blocking(&s, move |s| plan(s, &req, true)).await?))
}

// ---------------------------------------------------------------------------------------------
// task
// ---------------------------------------------------------------------------------------------

/// The account's id in the configuration, applied (reconcile runs `usermod -u`/`groupmod -g`).
fn change_id(
    s: &Services,
    author: &str,
    kind: IdKind,
    name: &str,
    from: u32,
    to: u32,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    let _allow = s.users.allow_id_change(kind, name, from, to);
    let what = match kind {
        IdKind::User => format!("uid of {name}: {from} → {to}"),
        IdKind::Group => format!("gid of {name}: {from} → {to}"),
    };
    let mut found = false;
    s.ops
        .config
        .apply_update(author, USERS_KEY, &what, log, |cur| {
            let Some(mut doc) = cur.and_then(|v| serde_json::from_value::<UsersConfig>(v).ok())
            else {
                return Ok(None);
            };
            match kind {
                IdKind::User => {
                    if let Some(u) = doc.users.iter_mut().find(|u| u.name == name && u.uid == from)
                    {
                        u.uid = to;
                        found = true;
                    }
                }
                IdKind::Group => {
                    if let Some(g) = doc
                        .groups
                        .iter_mut()
                        .find(|g| g.name == name && g.gid == from)
                    {
                        g.gid = to;
                        found = true;
                    }
                }
            }
            if !found {
                return Ok(None);
            }
            serde_json::to_value(&doc).map(Some).map_err(|e| e.to_string())
        })
        .map_err(|e| e.to_string())?;
    if !found {
        return Err(format!("{name} with id {from} is not in the configuration"));
    }
    Ok(())
}

/// `POST /v1/ids/remap`: task `ids.remap`. With `new_id` the account's id changes first
/// (applied revision), then the files on the folders follow; with `from_id` only the files
/// change. Cancelling stops the walk: files already changed keep the new id and running it
/// again finishes the rest (the account keeps its new id).
pub(super) async fn ids_remap(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<IdRemapRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, owner) = (svc(&st)?, user(&h)?);
    let req = json_body(body)?;
    let r2 = req.clone();
    let p = blocking(&s, move |s| plan(s, &r2, false)).await?;
    if !p.allowed {
        return Err(conflict(
            error_codes::CONFLICT,
            format!("cannot run now: {}", blockers_text(&p.blockers)),
        ));
    }
    let svc2 = s.clone();
    let author = owner.clone();
    let id = s
        .ops
        .tasks
        .spawn("ids.remap", &owner, move |ctx| async move {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let s = svc2;
                let log = |l: &str| c.log(l);
                if p.change {
                    // checked again: a login since the plan stops it before anything changes
                    let again = plan(&s, &req, false).map_err(|e| e.2.clone())?;
                    if !again.allowed {
                        return Err(format!(
                            "cannot run now: {}",
                            blockers_text(&again.blockers)
                        ));
                    }
                    change_id(&s, &author, p.kind, &p.name, p.from, p.to, &log)?;
                }
                let fs = s
                    .users
                    .remap_fs()
                    .cloned()
                    .ok_or("the ownership remap is not available")?;
                log(&format!(
                    "files owned by {} {} become {}'s ({}) in {}",
                    if p.kind == IdKind::User { "uid" } else { "gid" },
                    p.from,
                    p.name,
                    p.to,
                    p.paths.join(", ")
                ));
                let expected = req.expected.unwrap_or(0);
                let tick = |n: u64| {
                    if let Some(p) = n.saturating_mul(100).checked_div(expected) {
                        c.progress(p.min(99) as u8);
                    }
                };
                let roots: Vec<PathBuf> = p.paths.iter().map(PathBuf::from).collect();
                let counts = idremap::remap(
                    fs.as_ref(),
                    &roots,
                    &RemapOpts {
                        map: id_map(p.kind, p.from, p.to),
                        acls: req.acls,
                        dry_run: false,
                        deadline: None,
                        cancelled: &|| c.is_cancelled(),
                        tick: &tick,
                    },
                );
                log(&idremap::counts_text(&counts));
                for e in &counts.error_samples {
                    log(e);
                }
                if counts.cancelled {
                    return Err(format!(
                        "cancelled after {} entries: run it again to finish (changed files keep the new id)",
                        counts.scanned
                    ));
                }
                if counts.errors > 0 {
                    return Err(format!(
                        "{} entries could not change (see the log)",
                        counts.errors
                    ));
                }
                c.progress(100);
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())?
        })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}
