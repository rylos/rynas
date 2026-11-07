//! Adopted stacks (`docs/phase2/docker-apps.md` §5.7): compose projects rynas manages where
//! they are. The engine-side logic is in [`rynas_docker::adopt`]; this module holds the
//! routes, the record kept by [`DockerSvc`] and the move task.
//!
//! - `GET /v1/docker/stacks/{name}/adopt`: what adopting a running project would record;
//!   `POST` records it (nothing on disk changes).
//! - `POST /v1/docker/stacks/import/preview` and `/import`: a folder with compose files
//!   (restored from a backup, never started), registered where it is, started on request.
//! - `POST /v1/docker/stacks/{name}/release`: the record is deleted, nothing else.
//! - `POST /v1/docker/stacks/{name}/move`: task `docker.stack.move`, the folder copied into
//!   `/<vol>/docker/stacks/<name>` with its data.

use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use rynas_api_types::docker::*;
use rynas_api_types::error_codes;
use rynas_docker::adopt::{self, Adopted};
use rynas_docker::stacks::{ComposeAction, compose_cmd_files, compose_error, parse_config};
use rynas_docker::{Layout, under};
use rynas_tasks::{Cmd, TaskCtx};

use crate::docker::{
    DkState, DockerSvc, Extra, StackOp, accepted, audit, conflict, dsvc, mark_busy,
    start_compose,
};
use crate::ops::{Error, Res, user};

impl DockerSvc {
    /// The record of an adopted stack.
    pub(crate) fn adopted_get(&self, name: &str) -> Option<Adopted> {
        self.adopted.lock().unwrap().stacks.get(name).cloned()
    }

    /// Changes the records and saves them; the change is undone when saving fails.
    pub(crate) fn adopted_change(
        &self,
        f: impl FnOnce(&mut adopt::Registry),
    ) -> Result<(), String> {
        let mut g = self.adopted.lock().unwrap();
        let before = g.clone();
        f(&mut g);
        if let Err(e) = g.save(&self.state_dir) {
            *g = before;
            return Err(e);
        }
        Ok(())
    }

    /// The applied layout, or one rooted at the stack's own folder (no volume chosen: its
    /// rynas paths never exist, so no snapshots).
    pub(crate) fn layout_or(&self, dir: &str) -> Layout {
        self.layout().unwrap_or_else(|| Layout::new(dir))
    }

    /// The listing with the adopted stacks: marked, and listed as `down` without
    /// containers.
    pub(crate) fn with_adopted(&self, mut v: Vec<StackSummary>) -> Vec<StackSummary> {
        let reg = self.adopted.lock().unwrap().stacks.clone();
        for (name, a) in reg {
            let now = adopt::files_hash(&self.root, &a.files, &a.env_files);
            let info = a.info(Some(&now));
            let editable = a.writable();
            match v.iter_mut().find(|x| x.name == name) {
                Some(x) => {
                    x.manager = StackManager::Rynas;
                    x.editable = editable;
                    x.snapshots = false;
                    x.working_dir = Some(a.dir.clone());
                    x.config_files = a.files.clone();
                    x.adopted = Some(info);
                }
                None => v.push(StackSummary {
                    name,
                    manager: StackManager::Rynas,
                    state: StackState::Down,
                    working_dir: Some(a.dir.clone()),
                    config_files: a.files.clone(),
                    editable,
                    adopted: Some(info),
                    ..Default::default()
                }),
            }
        }
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// What adopting or importing would record.
    #[allow(clippy::too_many_arguments)]
    fn preview(
        &self,
        name: &str,
        dir: &str,
        files: Vec<String>,
        env_files: Vec<String>,
        origin: Option<ManagerKind>,
        running: bool,
        name_in_use: bool,
    ) -> AdoptPreview {
        let missing = files.is_empty() || files.iter().any(|f| !adopt::is_file(&self.root, f));
        let texts: Vec<String> = files
            .iter()
            .filter_map(|f| adopt::read_abs(&self.root, f).ok())
            .collect();
        let facts = adopt::scan_texts(&texts);
        let warnings = adopt::warnings(&adopt::WarnInput {
            name,
            dir,
            files: &files,
            env_files: &env_files,
            facts: &facts,
            origin,
            name_in_use,
            missing,
        });
        AdoptPreview {
            name: name.to_string(),
            dir: dir.to_string(),
            files,
            env_files,
            profiles_found: facts.profiles.into_iter().collect(),
            external_networks: facts.external_networks.into_iter().collect(),
            name_in_file: facts.name,
            origin,
            read_only: origin == Some(ManagerKind::Portainer),
            running,
            warnings,
        }
    }

    /// The candidate of a running project from its containers' labels (Portainer's paths
    /// mapped to its data volume on the host when found).
    async fn candidate(&self, name: &str) -> Res<AdoptPreview> {
        let cs = self.containers().await?;
        let mine: Vec<_> = cs
            .iter()
            .filter(|x| x.c.stack.as_deref() == Some(name))
            .collect();
        if mine.is_empty() {
            return Err(Error::not_found(format!("no stack {name}")));
        }
        self.refuse_known(name)?;
        let wd = mine
            .iter()
            .find_map(|x| x.working_dir.clone())
            .ok_or_else(|| conflict(format!("{name} has no compose working directory label")))?;
        let mut dir = adopt::clean(&wd);
        let raw = mine
            .iter()
            .find(|x| !x.config_files.is_empty())
            .map(|x| x.config_files.clone())
            .unwrap_or_default();
        let mut files = adopt::resolve_files(&dir, &raw);
        let mut envs = adopt::resolve_files(
            &dir,
            &mine
                .iter()
                .find(|x| !x.env_files.is_empty())
                .map(|x| x.env_files.clone())
                .unwrap_or_default(),
        );
        let origin = adopt::origin_of(&dir, &files);
        if origin == Some(ManagerKind::Portainer) && dir.starts_with("/data/") {
            let vols: Vec<(String, String)> = match self.engine().await {
                Ok(e) => e
                    .volumes(None)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|v| (v.name, v.mountpoint))
                    .collect(),
                Err(_) => Vec::new(),
            };
            let root = self.root.clone();
            if let Some(host) =
                adopt::portainer_host_dir(&dir, &vols, &|p| adopt::dir_exists(&root, p))
            {
                files = adopt::rebase(&files, &dir, &host);
                envs = adopt::rebase(&envs, &dir, &host);
                dir = host;
            }
        }
        if files.is_empty() {
            let root = self.root.clone();
            files = adopt::scan_folder(&dir, &|p| adopt::is_file(&root, p));
        }
        Ok(self.preview(name, &dir, files, envs, origin, true, false))
    }

    /// 409 when `name` is a rynas stack already (its folder, or a record).
    fn refuse_known(&self, name: &str) -> Res<()> {
        if self.adopted_get(name).is_some() {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::ALREADY_EXISTS,
                format!("stack {name} is adopted already"),
            ));
        }
        if let Some(l) = self.layout()
            && under(&self.root, &l.stack(name)).exists()
        {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::ALREADY_EXISTS,
                format!("{} exists: it is a rynas stack", l.stack(name)),
            ));
        }
        Ok(())
    }

    /// Checks and records an adoption; the folder is never inside the rynas stacks folder.
    fn record(
        &self,
        name: &str,
        p: &AdoptPreview,
        profiles: Vec<String>,
    ) -> Res<Adopted> {
        adopt::check_dir(&p.dir).map_err(|m| Error::invalid("dir", m))?;
        if !adopt::dir_exists(&self.root, &p.dir) {
            return Err(Error::invalid("dir", format!("{} is not a folder", p.dir)));
        }
        if let Some(l) = self.layout()
            && rynas_docker::stacks::is_below(&p.dir, &l.stacks())
        {
            return Err(Error::invalid(
                "dir",
                "this folder is in the rynas stacks folder already",
            ));
        }
        adopt::check_paths("files", &p.files).map_err(|(f, m)| Error::invalid(f, m))?;
        adopt::check_paths("env_files", &p.env_files).map_err(|(f, m)| Error::invalid(f, m))?;
        adopt::check_profiles(&profiles).map_err(|m| Error::invalid("profiles", m))?;
        if p.files.is_empty() {
            return Err(Error::invalid("files", format!("no compose file in {}", p.dir)));
        }
        for f in p.files.iter().chain(&p.env_files) {
            if !adopt::is_file(&self.root, f) {
                return Err(Error::invalid(
                    if p.files.contains(f) { "files" } else { "env_files" },
                    format!("{f} is not a readable file (symbolic links are not followed)"),
                ));
            }
        }
        let a = Adopted {
            dir: p.dir.clone(),
            files: p.files.clone(),
            env_files: p.env_files.clone(),
            profiles,
            origin: p.origin,
            read_only: p.read_only,
            adopted_ms: rynas_tasks::now_ms(),
            hash: adopt::files_hash(&self.root, &p.files, &p.env_files),
        };
        let a2 = a.clone();
        self.adopted_change(|r| {
            r.stacks.insert(name.to_string(), a2);
        })
        .map_err(Error::internal)?;
        Ok(a)
    }
}

/// The compose command line of an adopted stack (paths below the configuration root).
pub(crate) fn cmd_of(
    root: &Path,
    name: &str,
    a: &Adopted,
) -> impl Fn(ComposeAction) -> Cmd + Send + Sync + 'static {
    let real = |p: &String| under(root, p).to_string_lossy().into_owned();
    let dir = real(&a.dir);
    let files: Vec<String> = a.files.iter().map(real).collect();
    let envs: Vec<String> = a.env_files.iter().map(real).collect();
    let (name, profiles) = (name.to_string(), a.profiles.clone());
    move |act| compose_cmd_files(&name, &dir, &files, &envs, &profiles, act)
}

/// 409 `conflict`: the files changed since the editor read them.
pub(crate) fn changed_on_disk() -> Error {
    conflict("the files changed on disk since they were opened: reload them before saving")
        .field("hash")
}

/// The editor's view of an adopted stack: the first compose file and the env file.
pub(crate) fn read_files(root: &Path, a: &Adopted) -> Res<StackFiles> {
    let main = a
        .files
        .first()
        .ok_or_else(|| Error::not_found("the stack has no compose file"))?;
    let compose = adopt::read_abs(root, main).map_err(Error::internal)?;
    let env = adopt::read_abs(root, &a.env_path()).unwrap_or_default();
    let include = adopt::scan_texts(std::slice::from_ref(&compose)).include;
    let name = main.rsplit('/').next().unwrap_or(main).to_string();
    Ok(StackFiles {
        hash: adopt::editor_hash(&name, &compose, &env),
        compose,
        env,
        compose_file: name,
        other_files: a.files.iter().skip(1).cloned().collect(),
        read_only: !a.writable() || include,
    })
}

/// `PUT /stacks/{name}/files` of an adopted stack: the hash checked, validated with its
/// other files, written in place (owner and mode kept), the record's hash updated.
pub(crate) async fn write_files(
    s: &Arc<DockerSvc>,
    who: &str,
    name: &str,
    a: Adopted,
    l: Layout,
    req: StackFiles,
) -> Res<StackValidation> {
    let _busy = s.take_busy(name)?;
    let (root, a2) = (s.root.clone(), a.clone());
    let now = tokio::task::spawn_blocking(move || read_files(&root, &a2))
        .await
        .map_err(Error::internal)??;
    if now.read_only {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::STACK_NOT_MANAGED,
            format!("the files of {name} are not edited here"),
        ));
    }
    if !req.hash.is_empty() && req.hash != now.hash {
        return Err(changed_on_disk());
    }
    let extra = Extra {
        files: a.files.iter().skip(1).cloned().collect(),
        env_files: a.env_files.iter().skip(1).cloned().collect(),
        profiles: a.profiles.clone(),
    };
    let v = s
        .validate_with(name, &a.dir, &req.compose, &req.env, &l.volume, &extra)
        .await?;
    if !v.ok {
        return Err(Error::invalid("compose", v.error.clone().unwrap_or_default()));
    }
    audit(who, "stack.edit", name);
    let (root, a2, n2) = (s.root.clone(), a.clone(), name.to_string());
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let main = a2.files.first().ok_or("no compose file")?;
        adopt::write_abs(&root, main, &req.compose, rynas_docker::stacks::COMPOSE_MODE)?;
        let env = a2.env_path();
        if !req.env.is_empty() || adopt::is_file(&root, &env) {
            adopt::write_abs(&root, &env, &req.env, rynas_docker::stacks::ENV_MODE)?;
        }
        let _ = n2;
        Ok(())
    })
    .await
    .map_err(Error::internal)?
    .map_err(Error::internal)?;
    let hash = adopt::files_hash(&s.root, &a.files, &a.env_files);
    s.adopted_change(|r| {
        if let Some(x) = r.stacks.get_mut(name) {
            x.hash = hash;
        }
    })
    .map_err(Error::internal)?;
    s.changed();
    Ok(v)
}

/// Drops the record of `name` after `remove` (`down`); its files stay.
pub(crate) fn forget(s: &DockerSvc, ctx: &TaskCtx, name: &str) {
    if s.adopted_get(name).is_none() {
        return;
    }
    match s.adopted_change(|r| {
        r.stacks.remove(name);
    }) {
        Ok(()) => ctx.log(&format!("stack {name} released: its files were left in place")),
        Err(e) => ctx.log(&format!("releasing {name} failed: {e}")),
    }
}

fn check_name(name: &str) -> Res<()> {
    if !valid_stack_name(name) {
        return Err(Error::bad_request("invalid stack name"));
    }
    Ok(())
}

pub(crate) async fn adopt_preview(
    State(st): State<DkState>,
    UrlPath(name): UrlPath<String>,
) -> Res<Json<AdoptPreview>> {
    let s = dsvc(&st)?;
    check_name(&name)?;
    Ok(Json(s.candidate(&name).await?))
}

pub(crate) async fn adopt(
    State(st): State<DkState>,
    hd: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: axum::body::Bytes,
) -> Res<Json<StackAdopted>> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker stacks")?;
    check_name(&name)?;
    let req: AdoptRequest = crate::docker::optional_body(&body)?;
    let mut p = s.candidate(&name).await?;
    if let Some(d) = req.dir {
        adopt::check_dir(&d).map_err(|m| Error::invalid("dir", m))?;
        let d = adopt::clean(&d);
        // the labels' files follow the folder they were in
        p.files = adopt::rebase(&p.files, &p.dir, &d);
        p.env_files = adopt::rebase(&p.env_files, &p.dir, &d);
        p.dir = d;
    }
    if !req.files.is_empty() {
        p.files = req.files;
    }
    if !req.env_files.is_empty() {
        p.env_files = req.env_files;
    }
    let _busy = s.take_busy(&name)?;
    audit(&who, "stack.adopt", &name);
    let a = s.record(&name, &p, req.profiles)?;
    s.changed();
    let info = a.info(None);
    Ok(Json(StackAdopted {
        name,
        adopted: info,
        task_id: None,
    }))
}

/// The import candidate of a folder.
fn folder_candidate(s: &DockerSvc, dir: &str, cs: &[rynas_docker::model::Seen]) -> Res<AdoptPreview> {
    adopt::check_dir(dir).map_err(|m| Error::invalid("dir", m))?;
    let dir = adopt::clean(dir);
    if !adopt::dir_exists(&s.root, &dir) {
        return Err(Error::invalid("dir", format!("{dir} is not a folder")));
    }
    let root = s.root.clone();
    let files = adopt::scan_folder(&dir, &|p| adopt::is_file(&root, p));
    if files.is_empty() {
        return Err(Error::invalid("dir", format!("no compose file in {dir}")));
    }
    let texts: Vec<String> = files
        .iter()
        .filter_map(|f| adopt::read_abs(&s.root, f).ok())
        .collect();
    let in_file = adopt::scan_texts(&texts).name;
    let name = in_file
        .clone()
        .filter(|n| valid_stack_name(n))
        .or_else(|| adopt::name_from_dir(&dir))
        .unwrap_or_else(|| "stack".into());
    Ok(name_facts(s, &name, &dir, files, Vec::new(), cs))
}

fn name_facts(
    s: &DockerSvc,
    name: &str,
    dir: &str,
    files: Vec<String>,
    envs: Vec<String>,
    cs: &[rynas_docker::model::Seen],
) -> AdoptPreview {
    let mine: Vec<_> = cs
        .iter()
        .filter(|x| x.c.stack.as_deref() == Some(name))
        .collect();
    let same = |x: &&rynas_docker::model::Seen| {
        x.working_dir.as_deref().map(adopt::clean).as_deref() == Some(dir)
    };
    let running = mine.iter().any(same);
    let in_use = mine.iter().any(|x| !same(x));
    let origin = adopt::origin_of(dir, &files);
    s.preview(name, dir, files, envs, origin, running, in_use)
}

pub(crate) async fn import_preview(
    State(st): State<DkState>,
    body: Result<Json<ImportPreviewRequest>, JsonRejection>,
) -> Res<Json<AdoptPreview>> {
    let s = dsvc(&st)?;
    let Json(req) = body?;
    // no engine: nothing runs, the folder alone is enough
    let cs = s.containers().await.unwrap_or_default();
    Ok(Json(folder_candidate(&s, &req.dir, &cs)?))
}

pub(crate) async fn import(
    State(st): State<DkState>,
    hd: HeaderMap,
    body: Result<Json<ImportStackRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker stacks")?;
    let Json(req) = body?;
    let cs = s.containers().await.unwrap_or_default();
    let mut p = folder_candidate(&s, &req.dir, &cs)?;
    if let Some(n) = &req.name {
        rynas_docker::stacks::check_name(n)?;
        p = name_facts(&s, n, &p.dir, p.files.clone(), p.env_files.clone(), &cs);
    }
    if !req.files.is_empty() {
        p.files = req.files.clone();
    }
    if !req.env_files.is_empty() {
        p.env_files = req.env_files.clone();
    }
    let name = p.name.clone();
    s.refuse_known(&name)?;
    if p.warnings.contains(&AdoptWarning::NameInUse) {
        return Err(conflict(format!(
            "containers of another folder use the project name {name}: choose another name"
        ))
        .field("name"));
    }
    let _busy = s.take_busy(&name)?;
    audit(&who, "stack.import", &name);
    let a = s.record(&name, &p, req.profiles.clone())?;
    drop(_busy);
    s.changed();
    let task_id = if req.start && !a.read_only {
        Some(start_compose(
            &s,
            &who,
            &name,
            s.layout_or(&a.dir),
            StackOp::Compose(ComposeAction::Up),
        )?)
    } else {
        None
    };
    Ok(crate::error::created(
        "/api/v1/docker/stacks",
        &name,
        StackAdopted {
            name: name.clone(),
            adopted: a.info(None),
            task_id,
        },
    ))
}

pub(crate) async fn release(
    State(st): State<DkState>,
    hd: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Res<StatusCode> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker stacks")?;
    check_name(&name)?;
    if s.adopted_get(&name).is_none() {
        return Err(Error::not_found(format!("{name} is not an adopted stack")));
    }
    let _busy = s.take_busy(&name)?;
    audit(&who, "stack.release", &name);
    s.adopted_change(|r| {
        r.stacks.remove(&name);
    })
    .map_err(Error::internal)?;
    s.changed();
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn move_route(
    State(st): State<DkState>,
    hd: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<MoveStackRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker stacks")?;
    check_name(&name)?;
    let Json(req) = body?;
    let a = s
        .adopted_get(&name)
        .ok_or_else(|| Error::not_found(format!("{name} is not an adopted stack")))?;
    if req.confirm != name {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            error_codes::CONFIRMATION_REQUIRED,
            "moving the stack needs the stack name as confirm".into(),
        )
        .field("confirm"));
    }
    let l = s
        .layout()
        .ok_or_else(|| Error::invalid("volume", "choose a volume for Docker first (settings)"))?;
    if under(&s.root, &l.stack(&name)).exists() {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::ALREADY_EXISTS,
            format!("{} exists", l.stack(&name)),
        ));
    }
    let running = s
        .containers()
        .await
        .map(|cs| cs.iter().any(|x| x.c.stack.as_deref() == Some(name.as_str())))
        .unwrap_or(false);
    audit(&who, "stack.move", &name);
    let guard = mark_busy(&s, &name)?;
    let s2 = s.clone();
    let id = s.ops.tasks.spawn("docker.stack.move", &who, move |ctx| async move {
        let _guard = guard;
        move_task(&s2, &ctx, &name, a, l, running).await
    })?;
    Ok(accepted(id))
}

/// The binds of a compose model (`config`), or the task's error.
async fn binds_of(
    s: &DockerSvc,
    ctx: &TaskCtx,
    cmd: Cmd,
) -> Result<Vec<rynas_docker::stacks::Bind>, String> {
    let out = s.run(ctx, &cmd).await?;
    if !out.success() {
        return Err(format!(
            "the compose files are not valid: {}",
            compose_error(&out.stderr)
        ));
    }
    // compose saw real paths: back to system paths below a dry root
    let root = s.root.to_string_lossy().trim_end_matches('/').to_string();
    Ok(parse_config(&out.stdout)?
        .into_iter()
        .flat_map(|x| x.binds)
        .map(|mut b| {
            if !root.is_empty()
                && let Some(r) = b.source.strip_prefix(&root)
                && r.starts_with('/')
            {
                b.source = r.to_string();
            }
            b
        })
        .collect())
}

/// `docker.stack.move`: `down` (when it has containers), the whole folder copied into a
/// new rynas stack folder (subvolume), the binds compared, the record moved (or dropped
/// for a plain stack), the old folder renamed `<dir>.moved-<stamp>` (Portainer's left
/// alone), `up` again when it ran.
async fn move_task(
    s: &Arc<DockerSvc>,
    ctx: &TaskCtx,
    name: &str,
    a: Adopted,
    l: Layout,
    running: bool,
) -> Result<(), String> {
    let target = l.stack(name);
    let new = adopt::moved(&a, &target);
    ctx.log(&format!("moving {name}: {} → {target}", a.dir));
    if s.dry {
        ctx.log("dry system: the folder is not copied, the record is not changed");
        return Ok(());
    }
    let old_cmd = cmd_of(&s.root, name, &a);
    let old_binds = binds_of(s, ctx, old_cmd(ComposeAction::Config)).await?;
    if running {
        let out = s.run(ctx, &old_cmd(ComposeAction::Down)).await?;
        if !out.success() {
            return Err(format!("docker compose down failed: {}", compose_error(&out.stderr)));
        }
    }
    let restart_old = |why: String| async move {
        if running {
            let _ = s.run(ctx, &old_cmd(ComposeAction::Up)).await;
        }
        Err::<(), String>(why)
    };
    let fs = s.fs(None);
    let made = fs
        .ensure_stacks_dir(&l, &|m| ctx.log(m))
        .and_then(|_| fs.make_subvolume(&target, &|m| ctx.log(m)));
    if let Err(e) = made {
        return restart_old(e).await;
    }
    let from = under(&s.root, &a.dir).to_string_lossy().into_owned();
    let to = under(&s.root, &target).to_string_lossy().into_owned();
    let copied = s.run(ctx, &crate::docker::copy_cmd(&from, &to)).await;
    let checked = match copied {
        Ok(o) if o.success() => {
            let new_binds = binds_of(s, ctx, cmd_of(&s.root, name, &new)(ComposeAction::Config)).await;
            match new_binds {
                Ok(nb) => {
                    let bad = adopt::check_moved_binds(&old_binds, &nb, &a.dir, &target);
                    if bad.is_empty() {
                        Ok(())
                    } else {
                        Err(format!(
                            "bind mounts would point elsewhere after the move ({}); use absolute paths for folders outside the stack folder",
                            bad.join("; ")
                        ))
                    }
                }
                Err(e) => Err(e),
            }
        }
        Ok(o) => Err(format!("copy failed: {}", compose_error(&o.stderr))),
        Err(e) => Err(e),
    };
    if let Err(e) = checked {
        ctx.log(&format!("{e}: removing the copy {target}"));
        let _ = rynas_docker::stacks::remove_tree(&under(&s.root, &target), &|m| ctx.log(m));
        return restart_old(e).await;
    }
    let plain = adopt::is_plain(&new);
    let mut rec = new.clone();
    rec.hash = adopt::files_hash(&s.root, &rec.files, &rec.env_files);
    s.adopted_change(|r| {
        if plain {
            r.stacks.remove(name);
        } else {
            r.stacks.insert(name.to_string(), rec);
        }
    })?;
    ctx.log(if plain {
        "the stack is now a plain rynas stack"
    } else {
        "the stack stays adopted in its new folder (several files, env files or profiles)"
    });
    if !a.read_only {
        let aside = format!("{}.moved-{}", adopt::clean(&a.dir), rynas_docker::stamp(rynas_docker::now_s()));
        match std::fs::rename(under(&s.root, &a.dir), under(&s.root, &aside)) {
            Ok(()) => ctx.log(&format!("the old folder is kept as {aside}: delete it once the stack works")),
            Err(e) => ctx.log(&format!("the old folder {} stays where it was ({e})", a.dir)),
        }
    } else {
        ctx.log(&format!("Portainer's folder {} was left alone", a.dir));
    }
    if running {
        crate::docker::compose(s, ctx, name, &l, ComposeAction::Up).await?;
    }
    s.changed();
    Ok(())
}
