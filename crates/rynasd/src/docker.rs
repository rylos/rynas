//! Docker (`/v1/docker/...`, public under `/api/v1/docker/...`): phase 2 wave D1, the API
//! of `docs/phase2/docker-apps.md` §5. The engine side lives in [`rynas_docker`]; this module
//! holds the `docker` configuration key, the routes, the tasks, the event watcher and the
//! stats sampler.
//!
//! - Every route needs the administrator role (rynasd checks it again, [`crate::roles`]):
//!   the Docker socket is root on the box. Only whitelisted operations exist; nothing is
//!   forwarded to the engine as is.
//! - Every change runs as a task `docker.*` (user, time and log in the task history) and is
//!   written to the audit log (`rynas::audit` log target).
//! - Without Docker `GET /v1/docker/status` answers `installed: false`; the other routes
//!   answer 409 `docker_not_installed` (503 `docker_unavailable` when the daemon is down).
//! - `--dry-system`: reads go to the engine when its socket answers; changes (compose,
//!   start/stop, apt, systemctl, copies) are only logged, folders go below the dry root.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt, stream};
use rynas_api_types::docker::*;
use rynas_api_types::notify::{NewEvent, NotifyCategory, NotifySeverity};
use rynas_api_types::{TaskStarted, error_codes};
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile};
use rynas_docker::client::{DockerError, Engine, is_unavailable};
use rynas_docker::detect::{InstallFacts, api_older, install_facts};
use rynas_docker::events::{Alert, Backoff, DiskWatch, Tracker, UpdateWatch, parse_event};
use rynas_docker::model::Seen;
use rynas_docker::stacks::{
    ComposeAction, StackError, StackFs, compose_cmd, compose_error, parse_config,
};
use rynas_docker::{Layout, Runner, engine as eng, install, model, snapshots, stats, updates};
use rynas_notify::Notifier;
use rynas_notify::event::{code, field, firing, oneshot, resolved};
use rynas_tasks::{Cmd, CmdOutput, TaskCtx};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{broadcast, watch};

use crate::ops::{Error, Ops, Res, user};
use crate::services::{docs, stage};

pub const DOCKER_KEY: &str = "docker";
/// How long the install facts (packages, compose version) are reused.
const FACTS_TTL: Duration = Duration::from_secs(30);
/// Period of the stats sampler while somebody listens.
pub const STATS_TICK: Duration = Duration::from_secs(5);
/// Period of the event watcher's sweep (crash loops over, data-root fill level).
const SWEEP: Duration = Duration::from_secs(60);
/// Longest wait between reconnections of the event watcher.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How long the watcher gathers list-changing events into one `docker_changed`.
const HINT_DELAY: Duration = Duration::from_secs(1);
/// Longest data-root copy of a migration.
const COPY_TIMEOUT: Duration = Duration::from_secs(12 * 3600);
const SHORT: Duration = Duration::from_secs(60);
/// How long the `docker system df` answer is reused.
const DF_TTL: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------------------------
// the configuration domain
// ---------------------------------------------------------------------------------------------

/// `docker`: the engine configuration on a volume (merged `daemon.json`, containerd root,
/// mount drop-ins) and the preferences of the module. An apply that moves the engine's
/// folders restarts containerd and the engine, and is refused while containers run.
pub struct DockerDomain {
    pub tx: watch::Sender<DockerSettings>,
    /// Folders are plain folders (no subvolumes) under `--dry-system`.
    pub dry: bool,
}

impl DockerDomain {
    pub fn new(dry: bool) -> (Self, watch::Receiver<DockerSettings>) {
        let (tx, rx) = watch::channel(DockerSettings::default());
        (Self { tx, dry }, rx)
    }
}

fn settings_of(v: &Value) -> Result<DockerSettings, Invalid> {
    serde_json::from_value(v.clone())
        .map_err(|e| Invalid::from(format!("invalid docker document: {e}")))
}

/// Checks a settings document.
pub fn normalize(s: DockerSettings) -> Result<DockerSettings, Invalid> {
    if let Some(v) = &s.volume
        && !rynas_api_types::validate::volume_label(v)
    {
        return Err(Invalid::at("volume", "not a valid volume label"));
    }
    eng::check_pool(&s.address_pool).map_err(|m| Invalid::at("address_pool.base", m))?;
    let range = |f: &str, v: u32, lo: u32, hi: u32| {
        if (lo..=hi).contains(&v) {
            Ok(())
        } else {
            Err(Invalid::at(f, format!("must be between {lo} and {hi}")))
        }
    };
    range("keep_snapshots", s.keep_snapshots, 1, 50)?;
    range(
        "data_root_full_pct",
        u32::from(s.data_root_full_pct),
        50,
        99,
    )?;
    range("crash_restarts", s.crash_restarts, 2, 100)?;
    range("crash_window_min", s.crash_window_min, 1, 120)?;
    if let Some(v) = &s.apps_ssd_volume
        && !rynas_api_types::validate::volume_label(v)
    {
        return Err(Invalid::at("apps_ssd_volume", "not a valid volume label"));
    }
    Ok(s)
}

/// The volume label of an engine folder `/<label>/docker/engine`.
fn label_of_data_root(p: &str) -> Option<String> {
    let rest = p.strip_prefix('/')?.strip_suffix("/docker/engine")?;
    rynas_api_types::validate::volume_label(rest).then(|| rest.to_string())
}

/// Running containers according to `docker ps -q` (0 when the engine does not answer).
fn running_by_cli(ctx: &ApplyCtx) -> usize {
    ctx.system
        .run(
            &Cmd::new("docker")
                .args(["ps", "-q"])
                .env_clear()
                .timeout(SHORT),
        )
        .map(|o| o.stdout.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

impl Applier for DockerDomain {
    fn key(&self) -> &str {
        DOCKER_KEY
    }

    /// After the storage volumes.
    fn order(&self) -> i32 {
        40
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let s = normalize(settings_of(&value)?)?;
        serde_json::to_value(s).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        let s = settings_of(value).map_err(|e| e.message)?;
        let read = |p: &str| ctx.read(Path::new(p));
        // the pool rynas wrote last may be replaced; any other one is the administrator's
        let previous = self.tx.borrow().address_pool.clone();
        Ok(eng::engine_files(&s, Some(&previous), &read)?
            .into_iter()
            .map(|(path, content)| {
                // merged files keep the rest of their content: no "do not edit" header
                let header = path.ends_with(".conf");
                ManagedFile::whole(path, content, header)
            })
            .collect())
    }

    fn reconcile(
        &self,
        from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        let old = from
            .map(settings_of)
            .transpose()
            .map_err(|e| e.message)?
            .unwrap_or_default();
        let new = to
            .map(settings_of)
            .transpose()
            .map_err(|e| e.message)?
            .unwrap_or_default();
        if let Some(label) = &new.volume {
            let l = Layout::of_label(label);
            let fs = StackFs {
                root: ctx.root,
                runner: &rynas_docker::SystemRunner,
                subvolumes: !self.dry,
                owner: None,
            };
            let mounted = if ctx.root == Path::new("/") {
                rynas_volumes().iter().any(|(l, _)| l == label)
            } else {
                fs.real(&l.volume).is_dir()
            };
            if !mounted {
                return Err(format!("volume {label} is not mounted"));
            }
            for p in l.subvolumes() {
                fs.make_subvolume(&p, ctx.log)?;
            }
        }
        if old.volume != new.volume {
            ctx.system.daemon_reload()?;
            if ctx.system.unit_active("docker.service").unwrap_or(false) {
                let n = running_by_cli(ctx);
                if n > 0 {
                    return Err(format!(
                        "{n} containers are running: the engine folders cannot move now (stop them, or use the migration)"
                    ));
                }
                (ctx.log)("restarting containerd and Docker on the new folders");
                ctx.system.restart_unit("containerd.service")?;
                ctx.system.restart_unit("docker.service")?;
            }
        }
        (ctx.log)("docker: settings handed to the module");
        self.tx.send_replace(new);
        Ok(())
    }

    fn import(&self, ctx: &ApplyCtx) -> Result<Option<Value>, String> {
        let dr = eng::data_root_of(ctx.read(Path::new(eng::DAEMON_JSON)).as_deref());
        let Some(label) = dr.as_deref().and_then(label_of_data_root) else {
            return Ok(None);
        };
        let s = DockerSettings {
            volume: Some(label),
            ..Default::default()
        };
        serde_json::to_value(s).map(Some).map_err(|e| e.to_string())
    }
}

/// The applied settings at start.
pub fn initial_settings(cfg: &rynas_config::Config) -> DockerSettings {
    cfg.current(DOCKER_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// the service
// ---------------------------------------------------------------------------------------------

/// What the Docker endpoints work on.
pub struct DockerSvc {
    pub ops: Arc<Ops>,
    pub notifier: Arc<dyn Notifier>,
    /// The configuration root (`/`, or the dry root).
    pub root: PathBuf,
    pub dry: bool,
    /// The engine's socket; `None`: the engine is never contacted.
    pub socket: Option<String>,
    /// `<state>/docker`: the update report, validation scratch files.
    pub state_dir: PathBuf,
    pub(crate) runner: Arc<dyn Runner>,
    /// Where detection reads binaries and os-release (`/`; a fake tree in tests).
    pub(crate) host_root: PathBuf,
    settings: watch::Receiver<DockerSettings>,
    engine: tokio::sync::Mutex<Option<Engine>>,
    facts: Mutex<Option<(Instant, InstallFacts)>>,
    /// A running install or migration.
    engine_task: Mutex<Option<String>>,
    /// Stacks with an action in progress.
    busy: Mutex<HashSet<String>>,
    report: Mutex<UpdateReport>,
    update_watch: Mutex<UpdateWatch>,
    generation: AtomicU64,
    live: broadcast::Sender<(&'static str, Arc<str>)>,
    last_stats: Mutex<Option<Arc<str>>>,
    df_cache: tokio::sync::Mutex<Option<(Instant, Arc<DfCache>)>>,
    /// Tests: task commands go to [`DockerSvc::runner`] instead of the system (a fake that
    /// answers by command line), with the dry system off.
    pub exec_via_runner: std::sync::atomic::AtomicBool,
}

pub type DkState = Option<Arc<DockerSvc>>;

/// Events of the task stream (with `?docker=1`).
pub const EV_STATS: &str = "docker_stats";
pub const EV_CHANGED: &str = "docker_changed";
pub const EV_RESYNC: &str = "docker_resync";

pub(crate) fn not_installed() -> Error {
    Error::new(
        StatusCode::CONFLICT,
        error_codes::DOCKER_NOT_INSTALLED,
        "Docker is not installed".into(),
    )
}

fn unavailable(msg: impl Into<String>) -> Error {
    Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        error_codes::DOCKER_UNAVAILABLE,
        msg.into(),
    )
}

fn not_managed(name: &str) -> Error {
    Error::new(
        StatusCode::CONFLICT,
        error_codes::STACK_NOT_MANAGED,
        format!("stack {name} is not in the rynas stacks folder: its files are managed elsewhere"),
    )
}

pub(crate) fn conflict(msg: impl Into<String>) -> Error {
    Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, msg.into())
}

pub(crate) fn busy(msg: impl Into<String>) -> Error {
    Error::new(StatusCode::CONFLICT, error_codes::BUSY, msg.into())
}

impl From<DockerError> for Error {
    fn from(e: DockerError) -> Self {
        match e {
            DockerError::Unavailable(m) => unavailable(m),
            DockerError::NotFound(m) => Error::not_found(m),
            DockerError::Conflict(m) => conflict(m),
            DockerError::Failed(m) => Error::internal(m),
        }
    }
}

impl From<StackError> for Error {
    fn from(e: StackError) -> Self {
        match e {
            StackError::Invalid(f, m) => Error::invalid(f, m),
            StackError::AlreadyExists(m) => {
                Error::new(StatusCode::CONFLICT, error_codes::ALREADY_EXISTS, m)
            }
            StackError::NotFound(m) => Error::not_found(m),
            StackError::Failed(m) => Error::internal(m),
        }
    }
}

fn dsvc(st: &DkState) -> Res<Arc<DockerSvc>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "Docker is not enabled in this rynasd".into(),
        )
    })
}

/// A line of the audit log.
fn audit(who: &str, action: &str, target: &str) {
    tracing::info!(target: "rynas::audit", "docker {action} {target} by {who}");
}

pub(crate) fn accepted(id: String) -> Response {
    (StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response()
}

fn now_ms() -> u64 {
    rynas_tasks::now_ms()
}

/// Microseconds of a monotonic clock (stats deltas).
fn mono_us() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

impl DockerSvc {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ops: Arc<Ops>,
        notifier: Arc<dyn Notifier>,
        root: PathBuf,
        dry: bool,
        socket: Option<String>,
        state_dir: PathBuf,
        settings: watch::Receiver<DockerSettings>,
    ) -> Arc<Self> {
        Self::with_host(
            ops,
            notifier,
            root,
            dry,
            socket,
            state_dir,
            settings,
            PathBuf::from("/"),
            Arc::new(rynas_docker::SystemRunner),
        )
    }

    /// [`DockerSvc::new`] with the detection's host tree and command runner (tests).
    #[allow(clippy::too_many_arguments)]
    pub fn with_host(
        ops: Arc<Ops>,
        notifier: Arc<dyn Notifier>,
        root: PathBuf,
        dry: bool,
        socket: Option<String>,
        state_dir: PathBuf,
        settings: watch::Receiver<DockerSettings>,
        host_root: PathBuf,
        runner: Arc<dyn Runner>,
    ) -> Arc<Self> {
        let (live, _) = broadcast::channel(32);
        let report: UpdateReport = std::fs::read_to_string(state_dir.join("updates.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let mut uw = UpdateWatch::default();
        for i in &report.images {
            if let (true, Some(r)) = (i.update_available, &i.remote_digest) {
                uw.is_new(&i.image, r);
            }
        }
        Arc::new(Self {
            ops,
            notifier,
            root,
            dry,
            socket,
            state_dir,
            runner,
            host_root,
            settings,
            engine: tokio::sync::Mutex::new(None),
            facts: Mutex::new(None),
            engine_task: Mutex::new(None),
            busy: Mutex::new(HashSet::new()),
            report: Mutex::new(report),
            update_watch: Mutex::new(uw),
            generation: AtomicU64::new(1),
            live,
            last_stats: Mutex::new(None),
            df_cache: tokio::sync::Mutex::new(None),
            exec_via_runner: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn settings(&self) -> DockerSettings {
        self.settings.borrow().clone()
    }

    /// The layout of the applied volume.
    pub fn layout(&self) -> Option<Layout> {
        self.settings().volume.as_deref().map(Layout::of_label)
    }

    /// Live events of the task stream: `docker_stats`, `docker_changed`.
    pub fn subscribe(&self) -> broadcast::Receiver<(&'static str, Arc<str>)> {
        self.live.subscribe()
    }

    pub fn latest_stats(&self) -> Option<Arc<str>> {
        self.last_stats.lock().unwrap().clone()
    }

    /// Something changed: lists should be read again.
    pub fn changed(&self) {
        let g = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let j: Arc<str> = serde_json::to_string(&DockerChanged { generation: g })
            .unwrap_or_default()
            .into();
        let _ = self.live.send((EV_CHANGED, j));
    }

    pub(crate) fn fs(&self, owner: Option<(u32, u32)>) -> StackFs<'_> {
        StackFs {
            root: &self.root,
            runner: self.runner.as_ref(),
            subvolumes: !self.dry,
            owner,
        }
    }

    /// The install facts (cached for [`FACTS_TTL`]); detection only reads the host.
    pub async fn facts(&self, fresh: bool) -> InstallFacts {
        if !fresh
            && let Some((t, f)) = self.facts.lock().unwrap().as_ref()
            && t.elapsed() < FACTS_TTL
        {
            return f.clone();
        }
        let (runner, host) = (self.runner.clone(), self.host_root.clone());
        let f = tokio::task::spawn_blocking(move || install_facts(&host, runner.as_ref()))
            .await
            .unwrap_or_default();
        *self.facts.lock().unwrap() = Some((Instant::now(), f.clone()));
        f
    }

    fn invalidate(&self) {
        *self.facts.lock().unwrap() = None;
    }

    /// A connection to the engine when its socket answers.
    async fn engine_opt(&self) -> Result<Engine, DockerError> {
        let Some(sock) = &self.socket else {
            return Err(DockerError::Unavailable(
                "the Docker socket is disabled (--docker-socket)".into(),
            ));
        };
        let mut g = self.engine.lock().await;
        if let Some(e) = g.as_ref() {
            return Ok(e.clone());
        }
        let e = Engine::connect(sock).await?;
        *g = Some(e.clone());
        Ok(e)
    }

    /// Forgets the connection after a failure (the next call reconnects).
    async fn drop_engine(&self) {
        *self.engine.lock().await = None;
    }

    /// The engine, or 409 `docker_not_installed` / 503 `docker_unavailable`.
    pub async fn engine(&self) -> Res<Engine> {
        match self.engine_opt().await {
            Ok(e) => Ok(e),
            Err(e) => {
                if !self.facts(false).await.installed {
                    return Err(not_installed());
                }
                Err(unavailable(e.to_string()))
            }
        }
    }

    /// Runs `f` on the engine; a connection failure drops the cached client.
    async fn call<T, F, Fut>(&self, f: F) -> Res<T>
    where
        F: FnOnce(Engine) -> Fut,
        Fut: std::future::Future<Output = Result<T, DockerError>>,
    {
        let e = self.engine().await?;
        match f(e).await {
            Ok(v) => Ok(v),
            Err(err) => {
                if is_unavailable(&err) {
                    self.drop_engine().await;
                }
                Err(err.into())
            }
        }
    }

    /// The containers (every state).
    pub async fn containers(&self) -> Res<Vec<Seen>> {
        self.call(|e| async move { e.containers().await }).await
    }

    /// The stack folders in the stacks folder: names with a compose file, and those that
    /// are subvolumes.
    pub fn folders(&self) -> (BTreeSet<String>, BTreeSet<String>) {
        let mut names = BTreeSet::new();
        let mut subs = BTreeSet::new();
        let Some(l) = self.layout() else {
            return (names, subs);
        };
        let dir = rynas_docker::under(&self.root, &l.stacks());
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Some(n) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let p = e.path();
            if !valid_stack_name(&n) || rynas_docker::stacks::find_compose(&p).is_none() {
                continue;
            }
            if rynas_docker::stacks::is_subvolume(&p) {
                subs.insert(n.clone());
            }
            names.insert(n);
        }
        (names, subs)
    }

    /// The stacks.
    pub async fn stacks(&self) -> Res<Vec<StackSummary>> {
        let cs = self.containers().await?;
        let (f, s) = self.folders();
        Ok(model::group_stacks(&cs, self.layout().as_ref(), &f, &s))
    }

    /// A rynas stack: its layout and folder; 409 `stack_not_managed` for another one, 404
    /// for none.
    pub(crate) async fn managed(&self, name: &str) -> Res<Layout> {
        if !valid_stack_name(name) {
            return Err(Error::bad_request("invalid stack name"));
        }
        if let Some(l) = self.layout() {
            let dir = rynas_docker::under(&self.root, &l.stack(name));
            if rynas_docker::stacks::find_compose(&dir).is_some() {
                return Ok(l);
            }
        }
        let known = self
            .containers()
            .await
            .map(|cs| cs.iter().any(|s| s.c.stack.as_deref() == Some(name)))
            .unwrap_or(false);
        if known {
            Err(not_managed(name))
        } else {
            Err(Error::not_found(format!("no stack {name}")))
        }
    }

    /// The `docker-apps` (uid, gid) below the root, `None` under `--dry-system` or when the
    /// account does not exist.
    pub(crate) fn apps_owner(&self) -> Option<(u32, u32)> {
        if self.dry {
            return None;
        }
        let read = |p: &str| {
            std::fs::read_to_string(rynas_docker::under(&self.root, p)).unwrap_or_default()
        };
        match install::apps_ids(&read("/etc/passwd"), &read("/etc/group")) {
            (Some(u), Some(g)) => Some((u, g)),
            _ => None,
        }
    }

    /// Runs a command of a task: logged and run, or only logged under `--dry-system`.
    pub(crate) async fn run(&self, ctx: &TaskCtx, cmd: &Cmd) -> Result<CmdOutput, String> {
        if self.exec_via_runner.load(Ordering::Relaxed) {
            ctx.log(&format!("$ {}", cmd.display()));
            let (r, c) = (self.runner.clone(), cmd.clone());
            return tokio::task::spawn_blocking(move || r.run(&c))
                .await
                .map_err(|e| e.to_string())?;
        }
        if self.dry {
            ctx.log(&format!("$ {} (dry system: not run)", cmd.display()));
            return Ok(CmdOutput {
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        ctx.run(cmd)
            .await
            .map_err(|e| format!("{}: {e}", cmd.display()))
    }

    /// Makes sure the `docker-apps` account exists (before folders get its owner).
    pub(crate) async fn ensure_apps_account(&self, ctx: &TaskCtx) -> Result<(), String> {
        let read = |p: &str| {
            std::fs::read_to_string(rynas_docker::under(&self.root, p)).unwrap_or_default()
        };
        let (u, g) = install::apps_ids(&read("/etc/passwd"), &read("/etc/group"));
        for c in install::apps_account_cmds(g.is_some(), u.is_some()) {
            self.run(ctx, &c).await?;
        }
        Ok(())
    }

    fn take_busy(&self, name: &str) -> Res<BusyGuard<'_>> {
        let mut b = self.busy.lock().unwrap();
        if !b.insert(name.to_string()) {
            return Err(busy(format!("an action on stack {name} is in progress")));
        }
        Ok(BusyGuard(self, name.to_string()))
    }
}

/// Clears the busy mark of a stack when dropped.
struct BusyGuard<'a>(&'a DockerSvc, String);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.busy.lock().unwrap().remove(&self.1);
    }
}

/// An owned busy mark that a task carries.
pub(crate) struct OwnedBusy(Arc<DockerSvc>, String);

impl Drop for OwnedBusy {
    fn drop(&mut self) {
        self.0.busy.lock().unwrap().remove(&self.1);
        self.0.changed();
    }
}

pub(crate) fn mark_busy(s: &Arc<DockerSvc>, name: &str) -> Res<OwnedBusy> {
    if !s.busy.lock().unwrap().insert(name.to_string()) {
        return Err(busy(format!("an action on stack {name} is in progress")));
    }
    Ok(OwnedBusy(s.clone(), name.to_string()))
}

// ---------------------------------------------------------------------------------------------
// status and settings
// ---------------------------------------------------------------------------------------------

fn fs_usage(path: &str) -> Option<FsUsage> {
    let s = rynas_storage::overview::statvfs(path)?;
    Some(FsUsage {
        total_bytes: s.size,
        used_bytes: s.used,
        avail_bytes: s.avail,
    })
}

fn rynas_volumes() -> Vec<(String, String)> {
    crate::storage_ops::volumes_in(
        &std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default(),
    )
}

/// Mount points of the system (`/proc/self/mountinfo`).
pub(crate) fn mount_points() -> Vec<String> {
    rynas_storage::mounts::parse_mountinfo(
        &std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default(),
    )
    .into_iter()
    .map(|m| m.mountpoint)
    .collect()
}

impl DockerSvc {
    /// Is `/<label>` a mounted rynas volume? On the real system the mount table decides (an
    /// unmounted volume's empty folder, or `/lib`, never counts); below a dry or test root,
    /// an existing folder there.
    pub(crate) fn volume_mounted(&self, label: &str) -> bool {
        if !rynas_api_types::validate::volume_label(label) {
            return false;
        }
        if self.root == Path::new("/") {
            rynas_volumes().iter().any(|(l, _)| l == label)
        } else {
            rynas_docker::under(&self.root, &format!("/{label}")).is_dir()
        }
    }

    /// The mount points at or below which nothing may be deleted recursively (the real
    /// system only; a dry or test root has none).
    pub(crate) fn mounts(&self) -> Vec<String> {
        if self.root == Path::new("/") {
            mount_points()
        } else {
            Vec::new()
        }
    }
}

impl DockerSvc {
    pub async fn status(&self) -> DockerStatus {
        let f = self.facts(false).await;
        let mut st = DockerStatus {
            installed: f.installed,
            source: f.source,
            packages: f.packages.clone(),
            compose_version: f.compose_version.clone(),
            buildx_version: f.buildx_version.clone(),
            install_block: f.install_block,
            task_id: self.engine_task.lock().unwrap().clone(),
            read_at_ms: now_ms(),
            ..Default::default()
        };
        if let Ok(e) = self.engine_opt().await {
            let (v, i, cs) = tokio::join!(e.version(), e.info(), e.containers());
            match (v, i) {
                (Ok(v), Ok(i)) => {
                    st.running = true;
                    st.installed = true;
                    st.version = v.version;
                    st.api_version = v.api_version;
                    st.min_api_version = v.min_api_version;
                    st.containerd_snapshotter = i.driver_status.iter().flatten().any(|kv| {
                        kv.len() == 2
                            && kv[0] == "driver-type"
                            && kv[1].contains("containerd.snapshotter")
                    });
                    st.storage_driver = i.driver;
                    st.data_root = i.docker_root_dir;
                    st.cgroup_driver = i.cgroup_driver.map(|c| c.to_string());
                    st.live_restore = i.live_restore_enabled;
                    st.logging_driver = i.logging_driver;
                    let n = |x: Option<i64>| x.unwrap_or(0).max(0) as u32;
                    st.counts = DockerCounts {
                        containers: n(i.containers),
                        running: n(i.containers_running),
                        paused: n(i.containers_paused),
                        stopped: n(i.containers_stopped),
                        images: n(i.images),
                    };
                    if let Ok(cs) = cs {
                        st.managers = model::detected_managers(&cs);
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    tracing::debug!("docker status: {e}");
                    self.drop_engine().await;
                }
            }
        }
        if let Some(dr) = &st.data_root {
            let p = dr.clone();
            st.data_root_usage = tokio::task::spawn_blocking(move || fs_usage(&p))
                .await
                .ok()
                .flatten();
        }
        let set = self.settings();
        let mut w = Vec::new();
        if st.installed && !st.running {
            w.push(DockerWarning::DaemonDown);
        }
        if st.installed && st.compose_version.is_none() {
            w.push(DockerWarning::ComposeMissing);
        }
        if let Some(dr) = &st.data_root {
            let vols = rynas_volumes();
            if !vols
                .iter()
                .any(|(_, m)| rynas_docker::stacks::is_below(dr, m))
            {
                w.push(DockerWarning::DataRootOnSystem);
            }
        }
        if let Some(u) = st.data_root_usage
            && u.total_bytes > 0
            && (u.used_bytes as f64 * 100.0 / u.total_bytes as f64)
                >= f64::from(set.data_root_full_pct)
        {
            w.push(DockerWarning::DataRootAlmostFull);
        }
        if st.source == EngineSource::DockerCe
            && !rynas_docker::under(&self.root, install::PREFERENCES).exists()
        {
            w.push(DockerWarning::NotPinned);
        }
        if st
            .api_version
            .as_deref()
            .is_some_and(|v| api_older(v, (1, 44)))
        {
            w.push(DockerWarning::EngineTooOld);
        }
        st.warnings = w;
        st
    }

    fn settings_state(&self, owner: &str) -> Res<DockerSettingsState> {
        let (eff, cur, _): (DockerSettings, DockerSettings, Option<DockerSettings>) =
            docs(&self.ops.config, owner, DOCKER_KEY)?;
        let volumes = rynas_volumes()
            .into_iter()
            .map(|(label, path)| VolumeChoice {
                avail_bytes: rynas_storage::overview::statvfs(&path).map(|s| s.avail),
                label,
                path,
            })
            .collect();
        Ok(DockerSettingsState {
            stacks_dir: cur.volume.as_deref().map(|v| Layout::of_label(v).stacks()),
            pending: (eff != cur).then_some(eff),
            applied: cur,
            volumes,
        })
    }
}

async fn status(State(st): State<DkState>) -> Res<Json<DockerStatus>> {
    Ok(Json(dsvc(&st)?.status().await))
}

async fn settings_get(State(st): State<DkState>, hd: HeaderMap) -> Res<Json<DockerSettingsState>> {
    let (s, owner) = (dsvc(&st)?, user(&hd)?);
    let r = tokio::task::spawn_blocking(move || s.settings_state(&owner))
        .await
        .map_err(Error::internal)??;
    Ok(Json(r))
}

async fn settings_put(
    State(st): State<DkState>,
    hd: HeaderMap,
    body: Result<Json<DockerSettings>, JsonRejection>,
) -> Res<Json<DockerSettingsState>> {
    let (s, owner) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&owner, "Docker settings")?;
    let Json(v) = body?;
    let v = normalize(v).map_err(|e| match e.field {
        Some(f) => Error::invalid(f, e.message),
        None => Error::invalid("volume", e.message),
    })?;
    if v.volume != s.settings().volume
        && let Ok(e) = s.engine_opt().await
        && let Ok(i) = e.info().await
        && (i.containers.unwrap_or(0) > 0 || i.images.unwrap_or(0) > 0)
    {
        return Err(conflict(
            "the engine holds containers or images: move it with the migration (POST /api/v1/docker/engine/migrate)",
        )
        .field("volume"));
    }
    audit(&owner, "settings", "stage");
    let r = tokio::task::spawn_blocking(move || {
        stage(&s.ops.config, &owner, DOCKER_KEY, &v)?;
        s.settings_state(&owner)
    })
    .await
    .map_err(Error::internal)??;
    Ok(Json(r))
}

/// What applying `settings` writes and runs (no side effects).
async fn plan_engine(
    State(st): State<DkState>,
    body: Result<Json<DockerSettings>, JsonRejection>,
) -> Res<Json<EnginePlan>> {
    let s = dsvc(&st)?;
    let Json(v) = body?;
    let v = normalize(v)
        .map_err(|e| Error::invalid(e.field.unwrap_or_else(|| "volume".into()), e.message))?;
    let root = s.root.clone();
    let old = s.settings();
    let plan = tokio::task::spawn_blocking(move || -> Result<EnginePlan, String> {
        let read = |p: &str| std::fs::read_to_string(rynas_docker::under(&root, p)).ok();
        let files = eng::engine_files(&v, Some(&old.address_pool), &read)?
            .into_iter()
            .map(|(p, after)| PlannedFile {
                path: p.into(),
                before: read(p),
                after,
            })
            .filter(|f| f.before.as_deref() != Some(f.after.as_str()))
            .collect();
        let create = v
            .volume
            .as_deref()
            .map(|l| {
                Layout::of_label(l)
                    .subvolumes()
                    .into_iter()
                    .filter(|p| !rynas_docker::under(&root, p).exists())
                    .collect()
            })
            .unwrap_or_default();
        let restart = old.volume != v.volume && v.volume.is_some();
        let commands = if restart {
            vec![
                "systemctl daemon-reload".into(),
                "systemctl restart containerd.service".into(),
                "systemctl restart docker.service".into(),
            ]
        } else {
            Vec::new()
        };
        Ok(EnginePlan {
            files,
            create,
            commands,
            restart,
        })
    })
    .await
    .map_err(Error::internal)?
    .map_err(|e| Error::invalid("volume", e))?;
    Ok(Json(plan))
}

// ---------------------------------------------------------------------------------------------
// install and migration
// ---------------------------------------------------------------------------------------------

pub(crate) fn optional_body<T: serde::de::DeserializeOwned + Default>(
    body: &axum::body::Bytes,
) -> Res<T> {
    if body.iter().all(u8::is_ascii_whitespace) {
        Ok(T::default())
    } else {
        rynas_http::from_slice(body).map_err(Error::from)
    }
}

pub(crate) fn check_volume(label: &str) -> Res<()> {
    if !rynas_api_types::validate::volume_label(label) {
        return Err(Error::invalid("volume", "not a valid volume label"));
    }
    Ok(())
}

async fn install_route(
    State(st): State<DkState>,
    hd: HeaderMap,
    body: axum::body::Bytes,
) -> Res<Response> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker install")?;
    let req: InstallRequest = optional_body(&body)?;
    if let Some(v) = &req.volume {
        check_volume(v)?;
    }
    let f = s.facts(true).await;
    match f.install_block {
        Some(InstallBlock::Installed) => {
            return Err(conflict(
                "Docker is already installed: it is adopted, not reinstalled",
            ));
        }
        Some(b) => {
            return Err(Error::new(
                StatusCode::NOT_IMPLEMENTED,
                error_codes::NOT_AVAILABLE,
                format!(
                    "Docker cannot be installed here ({b:?}, release {:?}, architecture {:?})",
                    f.codename, f.arch
                ),
            ));
        }
        None => {}
    }
    let mut slot = s.engine_task.lock().unwrap();
    if slot.is_some() {
        return Err(busy("an install or migration is running"));
    }
    audit(&who, "install", req.volume.as_deref().unwrap_or("-"));
    let (s2, who2) = (s.clone(), who.clone());
    let id = s
        .ops
        .tasks
        .spawn("docker.install", &who, move |ctx| async move {
            let r = install_task(&s2, &ctx, &who2, f, req).await;
            *s2.engine_task.lock().unwrap() = None;
            s2.invalidate();
            s2.drop_engine().await;
            s2.changed();
            r
        })?;
    *slot = Some(id.clone());
    Ok(accepted(id))
}

fn write_root_file(root: &Path, path: &str, content: &str, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let p = rynas_docker::under(root, path);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let tmp = p.with_extension("rynas-new");
    std::fs::write(&tmp, content).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("{}: {e}", p.display()))
}

async fn install_task(
    s: &Arc<DockerSvc>,
    ctx: &TaskCtx,
    who: &str,
    f: InstallFacts,
    req: InstallRequest,
) -> Result<(), String> {
    let (Some(code), Some(arch)) = (f.codename.clone(), f.arch.clone()) else {
        return Err("unknown Debian release or architecture".into());
    };
    ctx.log(&format!(
        "installing Docker from {} ({code}, {arch})",
        install::REPO_URL
    ));
    let steps = install::repo_steps(&code, &arch);
    let n = steps.len() + 4;
    for (i, step) in steps.into_iter().enumerate() {
        if ctx.is_cancelled() {
            return Err("cancelled".into());
        }
        match step {
            install::InstallStep::Run(c) => {
                s.run(ctx, &c).await?;
            }
            install::InstallStep::CheckKey { staged, dest } => {
                let p = rynas_docker::under(&s.root, staged);
                if s.dry && !p.exists() {
                    ctx.log(&format!(
                        "checking the key fingerprint {} (dry system: not downloaded)",
                        install::KEY_FINGERPRINT
                    ));
                } else {
                    let text = std::fs::read_to_string(&p).unwrap_or_default();
                    let _ = std::fs::remove_file(&p);
                    install::check_key(&text)?;
                    ctx.log(&format!(
                        "key fingerprint {} checked, writing {dest}",
                        install::KEY_FINGERPRINT
                    ));
                    write_root_file(&s.root, dest, &text, 0o644)?;
                }
            }
            install::InstallStep::Write {
                path,
                content,
                mode,
            } => {
                ctx.log(&format!("writing {path}"));
                write_root_file(&s.root, path, &content, mode)?;
            }
        }
        ctx.progress(((i + 1) * 100 / n) as u8);
    }
    // the pin to the major version apt offers now
    let policy = install::policy_cmd();
    let out = if s.dry {
        String::new()
    } else {
        ctx.log(&format!("$ {}", policy.display()));
        policy
            .run(Some(ctx.token()), None)
            .await
            .map(|o| o.stdout)
            .unwrap_or_default()
    };
    match install::candidate_major(&out) {
        Some(major) => {
            ctx.log(&format!(
                "pinning docker-ce to major version {major} ({})",
                install::PREFERENCES
            ));
            write_root_file(
                &s.root,
                install::PREFERENCES,
                &install::preferences_content(major),
                0o644,
            )?;
        }
        None if s.dry => ctx.log("no candidate version (dry system: the repository was not added)"),
        None => return Err("apt offers no docker-ce version for this system".into()),
    }
    if let Some(vol) = req.volume.clone() {
        ctx.log(&format!(
            "engine on volume {vol}: applying the docker key before the packages"
        ));
        let (cfg, who, log_ctx) = (s.ops.config.clone(), who.to_string(), ctx.clone());
        tokio::task::spawn_blocking(move || {
            cfg.apply_update(
                &who,
                DOCKER_KEY,
                "Docker engine on the volume (install)",
                &|l| log_ctx.log(l),
                |old| {
                    let mut v: DockerSettings = old
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or_default();
                    v.volume = Some(vol);
                    serde_json::to_value(v).map(Some).map_err(|e| e.to_string())
                },
            )
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    }
    ctx.progress(80);
    s.run(ctx, &install::install_cmd()).await?;
    s.ensure_apps_account(ctx).await?;
    ctx.progress(100);
    ctx.log("Docker installed");
    Ok(())
}

async fn migrate_route(
    State(st): State<DkState>,
    hd: HeaderMap,
    body: Result<Json<MigrateRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker migration")?;
    let Json(req) = body?;
    check_volume(&req.volume)?;
    if !s.volume_mounted(&req.volume) {
        return Err(Error::invalid(
            "volume",
            format!("volume {} is not mounted", req.volume),
        ));
    }
    let e = s.engine().await?;
    let running = e
        .info()
        .await
        .map_err(Error::from)?
        .containers_running
        .unwrap_or(0);
    if running > 0 {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONTAINERS_RUNNING,
            format!("{running} containers are running: stop them before moving the engine"),
        ));
    }
    if s.settings().volume.as_deref() == Some(req.volume.as_str()) {
        return Err(conflict(format!("the engine is already on {}", req.volume)).field("volume"));
    }
    let mut slot = s.engine_task.lock().unwrap();
    if slot.is_some() {
        return Err(busy("an install or migration is running"));
    }
    audit(&who, "migrate", &req.volume);
    let (s2, who2) = (s.clone(), who.clone());
    let id = s
        .ops
        .tasks
        .spawn("docker.migrate", &who, move |ctx| async move {
            let r = migrate_task(&s2, &ctx, &who2, &req.volume).await;
            *s2.engine_task.lock().unwrap() = None;
            s2.drop_engine().await;
            s2.changed();
            r
        })?;
    *slot = Some(id.clone());
    Ok(accepted(id))
}

/// `cp -a --reflink=auto <from>/. <to>/`.
pub fn copy_cmd(from: &str, to: &str) -> Cmd {
    Cmd::new("cp")
        .args([
            "-a",
            "--reflink=auto",
            &format!("{from}/."),
            &format!("{to}/"),
        ])
        .env_clear()
        .timeout(COPY_TIMEOUT)
}

/// The engine's units, in start order.
const ENGINE_UNITS_START: [&str; 3] = ["containerd.service", "docker.socket", "docker.service"];

fn systemctl(verb: &str, units: &[&str]) -> Cmd {
    Cmd::new("systemctl")
        .arg(verb)
        .args(units)
        .env_clear()
        .timeout(Duration::from_secs(180))
}

/// Bytes `du -sx` counts in the folders (`None`: unknown, e.g. a dry run).
fn du_bytes(out: &CmdOutput) -> Option<u64> {
    let mut sum = 0u64;
    let mut any = false;
    for l in out.stdout.lines() {
        if let Some(n) = l.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()) {
            sum += n;
            any = true;
        }
    }
    any.then_some(sum)
}

/// `docker.migrate` (review 2026-11-07, M4): the volume must be mounted (mount table), the
/// copy must fit (`du` of the old folders against the free space of the volume, plus 10 %),
/// and once the engine is stopped, **any** failure starts it again on its old folders (the
/// configuration is only switched after a complete copy; its apply rolls back by itself).
pub async fn migrate_task(
    s: &Arc<DockerSvc>,
    ctx: &TaskCtx,
    who: &str,
    volume: &str,
) -> Result<(), String> {
    let l = Layout::of_label(volume);
    let (old_data, old_ctd) = eng::effective_roots(&s.root);
    ctx.log(&format!(
        "moving the engine: data-root {old_data} → {}, containerd {old_ctd} → {}",
        l.engine(),
        l.containerd()
    ));
    if !s.volume_mounted(volume) {
        return Err(format!("volume {volume} is not mounted: nothing was changed"));
    }
    if let Ok(e) = s.engine_opt().await
        && e.info()
            .await
            .ok()
            .and_then(|i| i.containers_running)
            .unwrap_or(0)
            > 0
    {
        return Err("containers are running: stop them first".into());
    }
    let copies: Vec<(String, String)> = [
        (old_data.clone(), l.engine()),
        (old_ctd.clone(), l.containerd()),
    ]
    .into_iter()
    .filter(|(from, to)| from != to && rynas_docker::under(&s.root, from).is_dir())
    .collect();
    // the space check runs before anything stops
    if !copies.is_empty() {
        let du = Cmd::new("du")
            .args(["-sx", "--block-size=1"])
            .args(copies.iter().map(|(f, _)| rynas_docker::under(&s.root, f)))
            .env_clear()
            .timeout(Duration::from_secs(30 * 60));
        let need = s.run(ctx, &du).await.ok().filter(|o| o.success()).as_ref().and_then(du_bytes);
        let avail = rynas_storage::overview::statvfs(
            &rynas_docker::under(&s.root, &l.volume).to_string_lossy(),
        )
        .map(|v| v.avail);
        match (need, avail) {
            (Some(n), Some(a)) if n.saturating_add(n / 10) > a => {
                return Err(format!(
                    "not enough space on {volume}: the engine's folders hold {} MiB, {} MiB are free (10 % margin): nothing was changed",
                    n >> 20,
                    a >> 20
                ));
            }
            (Some(n), Some(a)) => ctx.log(&format!(
                "space: {} MiB to copy, {} MiB free on {volume}",
                n >> 20,
                a >> 20
            )),
            _ => ctx.log("space: could not be measured, copying anyway"),
        }
    }
    s.drop_engine().await;
    s.run(
        ctx,
        &systemctl("stop", &["docker.socket", "docker.service", "containerd.service"]),
    )
    .await?;
    let moved = migrate_stopped(s, ctx, who, volume, &l, &copies).await;
    // started in every case: on the new folders after a success, on the old ones otherwise
    let started = s.run(ctx, &systemctl("start", &ENGINE_UNITS_START)).await;
    let started = match started {
        Ok(o) if o.success() => Ok(()),
        Ok(o) => Err(compose_error(&o.stderr)),
        Err(e) => Err(e),
    };
    match (moved, started) {
        (Ok(()), Ok(())) => {
            ctx.progress(100);
            ctx.log(&format!(
                "done: the old folders {old_data} and {old_ctd} are kept; delete them once the containers work"
            ));
            Ok(())
        }
        (Ok(()), Err(e)) => Err(format!(
            "the engine moved to {volume} but did not start: {e} (see journalctl -u docker)"
        )),
        (Err(e), Ok(())) => {
            ctx.log(&format!(
                "the move failed; the engine was started again on {old_data}"
            ));
            Err(format!(
                "{e}. The engine runs on its old folders again; the partial copy stays in {} and {} (delete it before trying again)",
                l.engine(),
                l.containerd()
            ))
        }
        (Err(e), Err(e2)) => Err(format!(
            "{e}; starting the engine again failed too: {e2} (systemctl start containerd docker)"
        )),
    }
}

/// The part of the migration that runs with the engine stopped.
async fn migrate_stopped(
    s: &Arc<DockerSvc>,
    ctx: &TaskCtx,
    who: &str,
    volume: &str,
    l: &Layout,
    copies: &[(String, String)],
) -> Result<(), String> {
    {
        let fs = s.fs(None);
        for p in l.subvolumes() {
            fs.make_subvolume(&p, &|m| ctx.log(m))?;
        }
    }
    ctx.progress(10);
    for (from, to) in copies {
        let c = copy_cmd(
            &rynas_docker::under(&s.root, from).to_string_lossy(),
            &rynas_docker::under(&s.root, to).to_string_lossy(),
        );
        let o = s.run(ctx, &c).await?;
        if !o.success() {
            return Err(format!(
                "copying {from} to {to} failed: {}",
                compose_error(&o.stderr)
            ));
        }
    }
    ctx.progress(80);
    let (cfg, who, log_ctx, vol) = (
        s.ops.config.clone(),
        who.to_string(),
        ctx.clone(),
        volume.to_string(),
    );
    tokio::task::spawn_blocking(move || {
        cfg.apply_update(
            &who,
            DOCKER_KEY,
            "Docker engine moved to the volume",
            &|m| log_ctx.log(m),
            |old| {
                let mut v: DockerSettings = old
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                v.volume = Some(vol);
                serde_json::to_value(v).map(Some).map_err(|e| e.to_string())
            },
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// listings
// ---------------------------------------------------------------------------------------------

async fn containers(State(st): State<DkState>) -> Res<Json<Vec<ContainerSummary>>> {
    let s = dsvc(&st)?;
    let mut v: Vec<ContainerSummary> = s.containers().await?.into_iter().map(|x| x.c).collect();
    v.sort_by(|a, b| (&a.stack, &a.name).cmp(&(&b.stack, &b.name)));
    Ok(Json(v))
}

async fn stacks(State(st): State<DkState>) -> Res<Json<Vec<StackSummary>>> {
    Ok(Json(dsvc(&st)?.stacks().await?))
}

async fn images(State(st): State<DkState>) -> Res<Json<Vec<ImageSummary>>> {
    let s = dsvc(&st)?;
    let mut v = s.call(|e| async move { e.images().await }).await?;
    v.sort_by(|a, b| a.tags.first().cmp(&b.tags.first()));
    Ok(Json(v))
}

async fn volumes(State(st): State<DkState>) -> Res<Json<Vec<VolumeSummary>>> {
    let s = dsvc(&st)?;
    // sizes from the cached `df` (a failure only leaves them out)
    let df = s.df().await.ok();
    Ok(Json(
        s.call(|e| async move { e.volumes(df.as_ref().map(|d| &d.volumes)).await })
            .await?,
    ))
}

async fn networks(State(st): State<DkState>) -> Res<Json<Vec<NetworkSummary>>> {
    let s = dsvc(&st)?;
    Ok(Json(s.call(|e| async move { e.networks().await }).await?))
}

async fn disk_usage(State(st): State<DkState>) -> Res<Json<DockerDiskUsage>> {
    let s = dsvc(&st)?;
    Ok(Json(s.df().await?.usage))
}

/// What rynas keeps of `docker system df`: the totals and the volume sizes (the full
/// answer, with every image and container, is dropped at once).
pub(crate) struct DfCache {
    usage: DockerDiskUsage,
    volumes: HashMap<String, (i64, i64)>,
}

impl DockerSvc {
    /// `docker system df`, at most once per [`DF_TTL`] (review 2026-11-07, M6): it walks
    /// every volume's files, takes seconds and spins up sleeping disks. Concurrent callers
    /// (the volumes list and the disk-usage card of one page load) share one call.
    pub(crate) async fn df(&self) -> Res<Arc<DfCache>> {
        let mut g = self.df_cache.lock().await;
        if let Some((t, d)) = g.as_ref()
            && t.elapsed() < DF_TTL
        {
            return Ok(d.clone());
        }
        let df = self.call(|e| async move { e.df().await }).await?;
        let d = Arc::new(DfCache {
            usage: model::disk_usage_of(&df),
            volumes: model::df_volumes(&df),
        });
        *g = Some((Instant::now(), d.clone()));
        Ok(d)
    }
}

async fn stats_get(State(st): State<DkState>) -> Res<Response> {
    let s = dsvc(&st)?;
    if let Some(j) = s.latest_stats() {
        return Ok(([("content-type", "application/json")], j.to_string()).into_response());
    }
    // nobody listened yet: one sample now (CPU needs two: 0 % until the next)
    let cs = s.containers().await?;
    let sample = sample_once(&mut stats::Sampler::new("/sys/fs/cgroup"), &cs);
    Ok(Json(sample).into_response())
}

// ---------------------------------------------------------------------------------------------
// containers: actions and logs
// ---------------------------------------------------------------------------------------------

/// Resolves a route's container reference (id, id prefix of 12+, or name).
async fn resolve(s: &DockerSvc, r: &str) -> Res<ContainerSummary> {
    if !valid_container_ref(r) {
        return Err(Error::bad_request("invalid container reference"));
    }
    let cs = s.containers().await?;
    if let Some(c) = cs.iter().find(|x| x.c.id == r || x.c.name == r) {
        return Ok(c.c.clone());
    }
    let pre: Vec<&Seen> = if r.len() >= 12 {
        cs.iter().filter(|x| x.c.id.starts_with(r)).collect()
    } else {
        Vec::new()
    };
    match pre.as_slice() {
        [one] => Ok(one.c.clone()),
        [] => Err(Error::not_found(format!("no container {r}"))),
        _ => Err(Error::bad_request(format!(
            "{r} matches several containers"
        ))),
    }
}

async fn container_action(
    State(st): State<DkState>,
    hd: HeaderMap,
    UrlPath((r, action)): UrlPath<(String, String)>,
) -> Res<Response> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker containers")?;
    if !matches!(action.as_str(), "start" | "stop" | "restart") {
        return Err(Error::not_found("no such container action"));
    }
    let c = resolve(&s, &r).await?;
    audit(&who, &format!("container.{action}"), &c.name);
    let s2 = s.clone();
    let id = s.ops.tasks.spawn(
        &format!("docker.container.{action}"),
        &who,
        move |ctx| async move {
            ctx.log(&format!(
                "{action} container {} ({})",
                c.name,
                &c.id[..c.id.len().min(12)]
            ));
            let r = if s2.dry {
                ctx.log("(dry system: not done)");
                Ok(())
            } else {
                let e = s2.engine().await.map_err(|e| e.2)?;
                match action.as_str() {
                    "start" => e.start(&c.id).await,
                    "stop" => e.stop(&c.id).await,
                    _ => e.restart(&c.id).await,
                }
                .map_err(|e| e.to_string())
            };
            s2.changed();
            r
        },
    )?;
    Ok(accepted(id))
}

#[derive(Deserialize, Default)]
struct LogQuery {
    tail: Option<u32>,
    timestamps: Option<String>,
}

fn flag(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("1" | "true"))
}

/// The last lines of a container's log (`?tail=N`, default 200, max 5000;
/// `?timestamps=1`).
async fn logs(
    State(st): State<DkState>,
    hd: HeaderMap,
    UrlPath(r): UrlPath<String>,
    Query(q): Query<LogQuery>,
) -> Res<Json<Vec<LogLine>>> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker logs")?;
    let c = resolve(&s, &r).await?;
    let e = s.engine().await?;
    let mut st = Box::pin(e.logs(&c.id, q.tail.unwrap_or(200), false, flag(&q.timestamps)));
    let mut out = Vec::new();
    while let Some(l) = st.next().await {
        out.push(l.map_err(Error::from)?);
    }
    Ok(Json(out))
}

/// SSE: the last `tail` lines (default 100), then new lines as they come (`log`, data
/// `LogLine`); `end` (data `{}`) when the container stops logging.
async fn logs_stream(
    State(st): State<DkState>,
    hd: HeaderMap,
    UrlPath(r): UrlPath<String>,
    Query(q): Query<LogQuery>,
) -> Res<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker logs")?;
    let c = resolve(&s, &r).await?;
    let e = s.engine().await?;
    let lines = e
        .logs(&c.id, q.tail.unwrap_or(100), true, flag(&q.timestamps))
        .map(|l| match l {
            Ok(l) => Ok(Event::default()
                .event("log")
                .data(serde_json::to_string(&l).unwrap_or_default())),
            Err(e) => Ok(Event::default()
                .event("error")
                .data(serde_json::json!({"message": e.to_string()}).to_string())),
        });
    let end = stream::once(async { Ok(Event::default().event("end").data("{}")) });
    Ok(Sse::new(lines.chain(end)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

// ---------------------------------------------------------------------------------------------
// stacks
// ---------------------------------------------------------------------------------------------

impl DockerSvc {
    /// Validates compose files with `docker compose config` (read-only): `compose`/`env`
    /// are written to a scratch folder, relative paths resolve against `dir`.
    ///
    /// The text comes from an administrator but compose runs as root, so the run is kept
    /// narrow (review 2026-11-07, M1): at most [`rynas_docker::stacks::MAX_FILE`] each, a
    /// top-level `include:` is refused (it reads, or downloads, other compose files),
    /// services' `env_file:` are not read (`--no-env-resolution`: a root file never ends up
    /// in the error text), 30 s timeout, the scratch files are `0600` in rynasd's state and
    /// removed afterwards; `config` writes nothing in the project folder.
    async fn validate(
        &self,
        project: &str,
        dir: &str,
        compose: &str,
        env: &str,
        volume: &str,
    ) -> Res<StackValidation> {
        let f = self.facts(false).await;
        if !f.installed {
            return Err(not_installed());
        }
        if f.compose_version.is_none() {
            return Err(Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                error_codes::TOOL_MISSING,
                "the docker compose plugin is not installed".into(),
            ));
        }
        let max = rynas_docker::stacks::MAX_FILE;
        if compose.len() > max || env.len() > max {
            return Err(Error::invalid(
                if compose.len() > max { "compose" } else { "env" },
                format!("at most {} KiB", max >> 10),
            ));
        }
        if let Some(e) = refused_compose_keys(compose) {
            return Ok(StackValidation {
                ok: false,
                error: Some(e),
                ..Default::default()
            });
        }
        let scratch = self
            .state_dir
            .join(format!("validate-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&scratch).map_err(Error::internal)?;
        let _ = std::fs::set_permissions(&scratch, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let cf = scratch.join("compose.yaml");
        let ef = scratch.join(".env");
        let w = crate::tls::write_atomic(&cf, compose.as_bytes(), 0o600, None)
            .and_then(|_| crate::tls::write_atomic(&ef, env.as_bytes(), 0o600, None));
        if let Err(e) = w {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(Error::internal(e));
        }
        let cmd = |narrow: bool| {
            let c = compose_cmd(
                project,
                &rynas_docker::under(&self.root, dir).to_string_lossy(),
                &cf.to_string_lossy(),
                Some(&ef.to_string_lossy()),
                ComposeAction::Config,
            );
            if narrow { c.arg("--no-env-resolution") } else { c }
        };
        let mut out = cmd(true).run(None, None).await;
        if out
            .as_ref()
            .is_ok_and(|o| !o.success() && o.stderr.contains("--no-env-resolution"))
        {
            // compose older than 2.24 (2024): no such flag
            tracing::info!("docker compose has no --no-env-resolution: validating without it");
            out = cmd(false).run(None, None).await;
        }
        let _ = std::fs::remove_dir_all(&scratch);
        let out = out.map_err(|e| Error::internal(format!("docker compose config: {e}")))?;
        if !out.success() {
            return Ok(StackValidation {
                ok: false,
                error: Some(compose_error(&out.stderr)),
                ..Default::default()
            });
        }
        let services = parse_config(&out.stdout).map_err(Error::internal)?;
        let used = match self.containers().await {
            Ok(cs) => rynas_docker::stacks::used_ports(
                cs.iter()
                    .flat_map(|s| s.c.ports.iter().map(move |p| (&s.c.stack, p))),
                Some(project),
            ),
            Err(_) => BTreeSet::new(),
        };
        let root = self.root.clone();
        Ok(rynas_docker::stacks::validation(
            &services,
            volume,
            &used,
            &move |p| rynas_docker::under(&root, p).exists(),
        ))
    }

    /// The layout for a new stack: the request's volume or the applied one.
    fn layout_for(&self, volume: Option<&str>) -> Res<Layout> {
        match volume {
            Some(v) => {
                check_volume(v)?;
                Ok(Layout::of_label(v))
            }
            None => self.layout().ok_or_else(|| {
                Error::invalid("volume", "choose a volume for Docker first (settings)")
            }),
        }
    }
}

/// Compose features a validation refuses before running compose: a top-level `include:`
/// (reads or downloads other compose files as root). A plain line check: the key sits at
/// column 0 of a YAML mapping, quoted or not.
fn refused_compose_keys(compose: &str) -> Option<String> {
    compose
        .lines()
        .any(|l| {
            let k = l.trim_end();
            ["include:", "\"include\":", "'include':"]
                .iter()
                .any(|p| k.starts_with(p))
        })
        .then(|| "include: is not supported in a rynas stack (put the services in this file)".into())
}

async fn stack_create(
    State(st): State<DkState>,
    hd: HeaderMap,
    body: Result<Json<CreateStackRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker stacks")?;
    let Json(req) = body?;
    rynas_docker::stacks::check_name(&req.name)?;
    let l = s.layout_for(req.volume.as_deref())?;
    let _busy = s.take_busy(&req.name)?;
    let validation = s
        .validate(
            &req.name,
            &l.stack(&req.name),
            &req.compose,
            &req.env,
            &l.volume,
        )
        .await?;
    if !validation.ok {
        return Err(Error::invalid(
            "compose",
            validation.error.clone().unwrap_or_default(),
        ));
    }
    audit(&who, "stack.create", &req.name);
    let (s2, l2, r2) = (s.clone(), l.clone(), req.clone());
    let path = tokio::task::spawn_blocking(move || {
        let owner = s2.apps_owner();
        s2.fs(owner)
            .create_stack(&l2, &r2.name, &r2.compose, &r2.env, &r2.files, &|m| {
                tracing::info!("docker: {m}")
            })
    })
    .await
    .map_err(Error::internal)??;
    drop(_busy);
    s.changed();
    let task_id = if req.start {
        Some(start_compose(
            &s,
            &who,
            &req.name,
            l,
            StackOp::Compose(ComposeAction::Up),
        )?)
    } else {
        None
    };
    let body = StackCreated {
        name: req.name.clone(),
        path,
        task_id,
        validation,
    };
    Ok(crate::error::created(
        "/api/v1/docker/stacks",
        &req.name,
        body,
    ))
}

async fn stack_validate(
    State(st): State<DkState>,
    body: Result<Json<ValidateStackRequest>, JsonRejection>,
) -> Res<Json<StackValidation>> {
    let s = dsvc(&st)?;
    let Json(req) = body?;
    let name = req.name.clone().unwrap_or_else(|| "new-stack".into());
    rynas_docker::stacks::check_name(&name)?;
    let l = s.layout_for(None)?;
    Ok(Json(
        s.validate(&name, &l.stack(&name), &req.compose, &req.env, &l.volume)
            .await?,
    ))
}

async fn stack_get(
    State(st): State<DkState>,
    UrlPath(name): UrlPath<String>,
) -> Res<Json<StackDetail>> {
    let s = dsvc(&st)?;
    if !valid_stack_name(&name) {
        return Err(Error::bad_request("invalid stack name"));
    }
    let cs = s.containers().await?;
    let (f, sv) = s.folders();
    let l = s.layout();
    let summary = model::group_stacks(&cs, l.as_ref(), &f, &sv)
        .into_iter()
        .find(|x| x.name == name)
        .ok_or_else(|| Error::not_found(format!("no stack {name}")))?;
    let snapshots = match (&l, summary.manager) {
        (Some(l), StackManager::Rynas) => snapshots::list(&s.root, l, &name),
        _ => Vec::new(),
    };
    let containers = cs
        .into_iter()
        .filter(|x| x.c.stack.as_deref() == Some(name.as_str()))
        .map(|x| x.c)
        .collect();
    Ok(Json(StackDetail {
        summary,
        containers,
        snapshots,
    }))
}

async fn stack_files_get(
    State(st): State<DkState>,
    hd: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Res<Json<StackFiles>> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    // .env holds secrets
    s.ops.admins.require_admin(&who, "Docker stack files")?;
    let l = s.managed(&name).await?;
    let s2 = s.clone();
    let f = tokio::task::spawn_blocking(move || s2.fs(None).read_files(&l, &name))
        .await
        .map_err(Error::internal)??;
    Ok(Json(f))
}

async fn stack_files_put(
    State(st): State<DkState>,
    hd: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<StackFiles>, JsonRejection>,
) -> Res<Json<StackValidation>> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker stack files")?;
    let Json(req) = body?;
    let l = s.managed(&name).await?;
    let _busy = s.take_busy(&name)?;
    let v = s
        .validate(&name, &l.stack(&name), &req.compose, &req.env, &l.volume)
        .await?;
    if !v.ok {
        return Err(Error::invalid(
            "compose",
            v.error.clone().unwrap_or_default(),
        ));
    }
    audit(&who, "stack.edit", &name);
    let s2 = s.clone();
    tokio::task::spawn_blocking(move || {
        s2.fs(s2.apps_owner())
            .write_files(&l, &name, &req.compose, &req.env)
    })
    .await
    .map_err(Error::internal)??;
    s.changed();
    Ok(Json(v))
}

/// What a stack task does.
#[derive(Debug, Clone)]
enum StackOp {
    Compose(ComposeAction),
    /// Snapshot (optional), pull, up -d; rolled back on failure.
    Update {
        snapshot: bool,
        stop_first: bool,
    },
    /// Snapshot (optional) and pull only.
    Pull {
        snapshot: bool,
        stop_first: bool,
    },
    Remove {
        delete_files: bool,
    },
    Rollback {
        snapshot: String,
    },
}

impl StackOp {
    fn task(&self) -> String {
        match self {
            StackOp::Compose(a) => format!("docker.stack.{}", a.as_str()),
            StackOp::Update { .. } => "docker.stack.update".into(),
            StackOp::Pull { .. } => "docker.stack.pull".into(),
            StackOp::Remove { .. } => "docker.stack.remove".into(),
            StackOp::Rollback { .. } => "docker.stack.rollback".into(),
        }
    }
}

fn start_compose(s: &Arc<DockerSvc>, who: &str, name: &str, l: Layout, op: StackOp) -> Res<String> {
    let guard = mark_busy(s, name)?;
    let (s2, n2) = (s.clone(), name.to_string());
    let id = s.ops.tasks.spawn(&op.task(), who, move |ctx| async move {
        let _guard = guard;
        stack_task(&s2, &ctx, &n2, &l, op).await
    })?;
    Ok(id)
}

/// Runs one compose action of a rynas stack.
pub(crate) async fn compose(
    s: &DockerSvc,
    ctx: &TaskCtx,
    name: &str,
    l: &Layout,
    a: ComposeAction,
) -> Result<(), String> {
    let dir = rynas_docker::under(&s.root, &l.stack(name));
    let file = rynas_docker::stacks::find_compose(&dir)
        .ok_or_else(|| format!("stack {name} has no compose file"))?;
    let dir_s = dir.to_string_lossy().into_owned();
    if a == ComposeAction::Up {
        prepare_up(s, ctx, name, &dir_s, file, l).await?;
    }
    let out = s.run(ctx, &compose_cmd(name, &dir_s, file, None, a)).await?;
    // a failing pull or up must fail the task (and trigger the update's rollback)
    if !out.success() {
        return Err(format!(
            "docker compose {} failed: {}",
            a.as_str(),
            compose_error(&out.stderr)
        ));
    }
    Ok(())
}

/// Before `up`: the `docker-apps` account, and the missing bind folders below the volume
/// created with its owner (Docker would make them as root); binds outside the volume are
/// warned about.
async fn prepare_up(
    s: &DockerSvc,
    ctx: &TaskCtx,
    name: &str,
    dir: &str,
    file: &str,
    l: &Layout,
) -> Result<(), String> {
    s.ensure_apps_account(ctx).await?;
    let cmd = compose_cmd(name, dir, file, None, ComposeAction::Config);
    let out = if s.exec_via_runner.load(Ordering::Relaxed) {
        s.run(ctx, &cmd).await?
    } else {
        cmd.run(Some(ctx.token()), None)
            .await
            .map_err(|e| e.to_string())?
    };
    if !out.success() {
        return Err(format!(
            "the compose file is not valid: {}",
            compose_error(&out.stderr)
        ));
    }
    let services = parse_config(&out.stdout)?;
    let binds: Vec<_> = services.iter().flat_map(|s| s.binds.clone()).collect();
    for b in &binds {
        if !rynas_docker::stacks::is_below(&b.source, &l.volume) {
            ctx.log(&format!(
                "warning: service {} binds {} outside the volume {}",
                b.service, b.source, l.volume
            ));
        }
    }
    let owner = s.apps_owner();
    let fs = s.fs(owner);
    fs.create_bind_dirs(&binds, &l.volume, &|m| ctx.log(m))?;
    Ok(())
}

/// The images the stack's services run now.
pub(crate) async fn current_images(s: &DockerSvc, name: &str) -> Vec<ServiceImage> {
    let Ok(e) = s.engine().await else {
        return Vec::new();
    };
    let (cs, imgs) = tokio::join!(e.containers(), e.images());
    let (Ok(cs), Ok(imgs)) = (cs, imgs) else {
        return Vec::new();
    };
    let mine: Vec<&ContainerSummary> = cs
        .iter()
        .filter(|x| x.c.stack.as_deref() == Some(name))
        .map(|x| &x.c)
        .collect();
    model::service_images(&mine, &imgs)
}

/// A pre-update snapshot (stack stopped first when asked); `None` when the folder is not a
/// subvolume.
pub(crate) async fn pre_snapshot(
    s: &DockerSvc,
    ctx: &TaskCtx,
    name: &str,
    l: &Layout,
    reason: &str,
    stop_first: bool,
) -> Result<Option<StackSnapshot>, String> {
    if !snapshots::supported(&s.root, l, name) {
        ctx.log("no snapshot: the stack folder is not a btrfs subvolume");
        return Ok(None);
    }
    let images = current_images(s, name).await;
    if stop_first {
        compose(s, ctx, name, l, ComposeAction::Stop).await?;
    }
    let (root, l2, n2, r2, keep) = (
        s.root.clone(),
        l.clone(),
        name.to_string(),
        reason.to_string(),
        s.settings().keep_snapshots,
    );
    let log = ctx.clone();
    let snap = tokio::task::spawn_blocking(move || {
        let snap = snapshots::take(&root, &l2, &n2, &r2, images, rynas_docker::now_s())?;
        for id in snapshots::prune(&root, &l2, &n2, keep as usize, &|m| log.log(m)) {
            log.log(&format!("old snapshot {id} deleted (keep {keep})"));
        }
        Ok::<_, String>(snap)
    })
    .await
    .map_err(|e| e.to_string())?;
    let r = match snap {
        Ok(sn) => {
            ctx.log(&format!("snapshot {} of {name} taken", sn.id));
            Ok(Some(sn))
        }
        Err(e) => Err(e),
    };
    if stop_first {
        compose(s, ctx, name, l, ComposeAction::Start).await?;
    }
    r
}

/// Restores the folder from a snapshot, puts the images back and starts the stack.
pub(crate) async fn do_rollback(
    s: &DockerSvc,
    ctx: &TaskCtx,
    name: &str,
    l: &Layout,
    id: &str,
) -> Result<(), String> {
    let snap = snapshots::get(&s.root, l, name, id)
        .ok_or_else(|| format!("no snapshot {id} of {name}"))?;
    let now = current_images(s, name).await;
    compose(s, ctx, name, l, ComposeAction::Down).await?;
    if s.dry {
        ctx.log(&format!(
            "restore of {} from snapshot {id} (dry system: not done)",
            l.stack(name)
        ));
    } else {
        let (root, l2, n2, i2, log) = (
            s.root.clone(),
            l.clone(),
            name.to_string(),
            id.to_string(),
            ctx.clone(),
        );
        tokio::task::spawn_blocking(move || {
            snapshots::rollback(&root, &l2, &n2, &i2, now, rynas_docker::now_s(), &|m| {
                log.log(m)
            })
        })
        .await
        .map_err(|e| e.to_string())??;
    }
    let mut present = HashSet::new();
    if let Ok(e) = s.engine().await {
        for i in &snap.images {
            if e.image_exists(&i.image_id).await {
                present.insert(i.image_id.clone());
            }
        }
    }
    for c in snapshots::retag_cmds(&snap.images, &|id| present.contains(id)) {
        s.run(ctx, &c).await?;
    }
    compose(s, ctx, name, l, ComposeAction::Up).await
}

async fn stack_task(
    s: &Arc<DockerSvc>,
    ctx: &TaskCtx,
    name: &str,
    l: &Layout,
    op: StackOp,
) -> Result<(), String> {
    match op {
        StackOp::Compose(a) => compose(s, ctx, name, l, a).await,
        StackOp::Pull {
            snapshot,
            stop_first,
        } => {
            if snapshot {
                pre_snapshot(s, ctx, name, l, "pull", stop_first).await?;
            }
            compose(s, ctx, name, l, ComposeAction::Pull).await
        }
        StackOp::Update {
            snapshot,
            stop_first,
        } => {
            let snap = if snapshot {
                pre_snapshot(s, ctx, name, l, "update", stop_first).await?
            } else {
                None
            };
            let r = async {
                compose(s, ctx, name, l, ComposeAction::Pull).await?;
                compose(s, ctx, name, l, ComposeAction::Up).await
            }
            .await;
            match (r, snap) {
                (Ok(()), _) => Ok(()),
                (Err(e), Some(sn)) => {
                    ctx.log(&format!(
                        "update failed ({e}): rolling back to snapshot {}",
                        sn.id
                    ));
                    match do_rollback(s, ctx, name, l, &sn.id).await {
                        Ok(()) => Err(format!("update failed and was rolled back: {e}")),
                        Err(e2) => Err(format!(
                            "update failed ({e}) and the rollback failed too: {e2}"
                        )),
                    }
                }
                (Err(e), None) => Err(e),
            }
        }
        StackOp::Rollback { snapshot } => do_rollback(s, ctx, name, l, &snapshot).await,
        StackOp::Remove { delete_files } => {
            compose(s, ctx, name, l, ComposeAction::Down).await?;
            if delete_files {
                if s.dry {
                    ctx.log(&format!(
                        "deleting {} (dry system: not done)",
                        l.stack(name)
                    ));
                    return Ok(());
                }
                let (s2, l2, n2, log) = (s.clone(), l.clone(), name.to_string(), ctx.clone());
                tokio::task::spawn_blocking(move || {
                    s2.fs(None).remove_stack_dir(&l2, &n2, &|m| log.log(m))
                })
                .await
                .map_err(|e| e.to_string())??;
            }
            Ok(())
        }
    }
}

/// start/stop/restart of a stack rynas does not manage: per container, through the engine.
async fn foreign_action(
    s: &Arc<DockerSvc>,
    who: &str,
    name: &str,
    action: ComposeAction,
) -> Res<String> {
    let cs = s.containers().await?;
    let ids: Vec<(String, String)> = cs
        .iter()
        .filter(|x| x.c.stack.as_deref() == Some(name))
        .map(|x| (x.c.id.clone(), x.c.name.clone()))
        .collect();
    if ids.is_empty() {
        return Err(Error::not_found(format!("no stack {name}")));
    }
    let guard = mark_busy(s, name)?;
    let s2 = s.clone();
    let id = s.ops.tasks.spawn(
        &format!("docker.stack.{}", action.as_str()),
        who,
        move |ctx| async move {
            let _guard = guard;
            let e = s2.engine().await.map_err(|e| e.2)?;
            for (id, n) in ids {
                ctx.log(&format!("{} {n}", action.as_str()));
                if s2.dry {
                    continue;
                }
                match action {
                    ComposeAction::Start => e.start(&id).await,
                    ComposeAction::Stop => e.stop(&id).await,
                    _ => e.restart(&id).await,
                }
                .map_err(|e| format!("{n}: {e}"))?;
            }
            Ok(())
        },
    )?;
    Ok(id)
}

async fn stack_action(
    State(st): State<DkState>,
    hd: HeaderMap,
    UrlPath((name, action)): UrlPath<(String, String)>,
    body: axum::body::Bytes,
) -> Res<Response> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker stacks")?;
    if !valid_stack_name(&name) {
        return Err(Error::bad_request("invalid stack name"));
    }
    let set = s.settings();
    let op = match action.as_str() {
        "up" | "down" | "restart" | "stop" | "start" => {
            let a = ComposeAction::parse(&action).expect("listed");
            match s.managed(&name).await {
                Ok(_) => StackOp::Compose(a),
                Err(e)
                    if e.1 == error_codes::STACK_NOT_MANAGED
                        && matches!(
                            a,
                            ComposeAction::Start | ComposeAction::Stop | ComposeAction::Restart
                        ) =>
                {
                    audit(&who, &format!("stack.{action}"), &name);
                    return Ok(accepted(foreign_action(&s, &who, &name, a).await?));
                }
                Err(e) => return Err(e),
            }
        }
        "pull" | "update" => {
            let r: UpdateStackRequest = optional_body(&body)?;
            let snapshot = r.snapshot.unwrap_or(set.snapshot_before_update);
            let stop_first = r.stop_first.unwrap_or(set.stop_for_snapshot);
            if action == "pull" {
                StackOp::Pull {
                    snapshot,
                    stop_first,
                }
            } else {
                StackOp::Update {
                    snapshot,
                    stop_first,
                }
            }
        }
        "remove" => {
            let r: RemoveStackRequest = optional_body(&body)?;
            if r.delete_files && r.confirm.as_deref() != Some(name.as_str()) {
                return Err(Error::new(
                    StatusCode::BAD_REQUEST,
                    error_codes::CONFIRMATION_REQUIRED,
                    "deleting the stack folder needs the stack name as confirm".into(),
                )
                .field("confirm"));
            }
            StackOp::Remove {
                delete_files: r.delete_files,
            }
        }
        "rollback" => {
            let r: RollbackRequest = rynas_http::from_slice(&body).map_err(Error::from)?;
            if rynas_docker::parse_stamp(&r.snapshot).is_none() {
                return Err(Error::invalid("snapshot", "not a snapshot id"));
            }
            StackOp::Rollback {
                snapshot: r.snapshot,
            }
        }
        _ => return Err(Error::not_found("no such stack action")),
    };
    let l = s.managed(&name).await?;
    if let StackOp::Rollback { snapshot } = &op
        && snapshots::get(&s.root, &l, &name, snapshot).is_none()
    {
        return Err(Error::not_found(format!(
            "no snapshot {snapshot} of {name}"
        )));
    }
    audit(&who, &format!("stack.{action}"), &name);
    Ok(accepted(start_compose(&s, &who, &name, l, op)?))
}

async fn snapshots_get(
    State(st): State<DkState>,
    UrlPath(name): UrlPath<String>,
) -> Res<Json<Vec<StackSnapshot>>> {
    let s = dsvc(&st)?;
    let l = s.managed(&name).await?;
    Ok(Json(snapshots::list(&s.root, &l, &name)))
}

async fn snapshot_take(
    State(st): State<DkState>,
    hd: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Res<Response> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker snapshots")?;
    let l = s.managed(&name).await?;
    if !snapshots::supported(&s.root, &l, &name) {
        return Err(conflict(format!(
            "{} is not a btrfs subvolume: no snapshots",
            l.stack(&name)
        )));
    }
    let _busy = s.take_busy(&name)?;
    audit(&who, "stack.snapshot", &name);
    let images = current_images(&s, &name).await;
    let (root, n2) = (s.root.clone(), name.clone());
    let snap = tokio::task::spawn_blocking(move || {
        snapshots::take(&root, &l, &n2, "manual", images, rynas_docker::now_s())
    })
    .await
    .map_err(Error::internal)?
    .map_err(Error::internal)?;
    Ok(crate::error::created(
        &format!("/api/v1/docker/stacks/{name}/snapshots"),
        &snap.id.clone(),
        snap,
    ))
}

async fn snapshot_delete(
    State(st): State<DkState>,
    hd: HeaderMap,
    UrlPath((name, id)): UrlPath<(String, String)>,
) -> Res<StatusCode> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker snapshots")?;
    let l = s.managed(&name).await?;
    if snapshots::get(&s.root, &l, &name, &id).is_none() {
        return Err(Error::not_found(format!("no snapshot {id} of {name}")));
    }
    audit(&who, "stack.snapshot-delete", &format!("{name}/{id}"));
    let root = s.root.clone();
    tokio::task::spawn_blocking(move || {
        snapshots::delete(&root, &l, &name, &id, &|m| tracing::info!("docker: {m}"))
    })
    .await
    .map_err(Error::internal)?
    .map_err(Error::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// updates
// ---------------------------------------------------------------------------------------------

async fn updates_get(State(st): State<DkState>) -> Res<Json<UpdateReport>> {
    let s = dsvc(&st)?;
    let r = s.report.lock().unwrap().clone();
    Ok(Json(r))
}

async fn updates_check(State(st): State<DkState>, hd: HeaderMap) -> Res<Response> {
    let (s, who) = (dsvc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Docker update check")?;
    s.engine().await?;
    audit(&who, "update-check", "-");
    let s2 = s.clone();
    let id = s
        .ops
        .tasks
        .spawn("docker.update-check", &who, move |ctx| async move {
            check_updates(&s2, &ctx).await
        })?;
    Ok(accepted(id))
}

/// Compares the registry digest of every image in use with the local one (no pull).
pub async fn check_updates(s: &Arc<DockerSvc>, ctx: &TaskCtx) -> Result<(), String> {
    let e = s.engine().await.map_err(|e| e.2)?;
    let cs = e.containers().await.map_err(|e| e.to_string())?;
    let imgs = e.images().await.map_err(|e| e.to_string())?;
    let digests: HashMap<&str, &[String]> = imgs
        .iter()
        .map(|i| (i.id.as_str(), i.digests.as_slice()))
        .collect();
    let empty: Vec<String> = Vec::new();
    let rows = cs.iter().map(|x| {
        (
            x.c.image.as_str(),
            digests
                .get(x.c.image_id.as_str())
                .copied()
                .unwrap_or(empty.as_slice()),
            x.c.stack.as_deref(),
            x.c.name.as_str(),
        )
    });
    let used = updates::in_use(rows);
    let n = used.len().max(1);
    let mut out = Vec::new();
    for (i, u) in used.into_iter().enumerate() {
        if ctx.is_cancelled() {
            return Err("cancelled".into());
        }
        let remote = e.registry_digest(&u.image).await;
        let line = updates::compare(u, remote);
        match (&line.error, line.update_available) {
            (Some(err), _) => ctx.log(&format!("{}: {err}", line.image)),
            (None, true) => ctx.log(&format!("{}: update available", line.image)),
            (None, false) => ctx.log(&format!("{}: up to date", line.image)),
        }
        out.push(line);
        ctx.progress(((i + 1) * 100 / n) as u8);
    }
    for l in &out {
        if let (true, Some(r)) = (l.update_available, &l.remote_digest)
            && s.update_watch.lock().unwrap().is_new(&l.image, r)
        {
            s.notifier.emit(update_event(l));
        }
    }
    let report = UpdateReport {
        checked_at_ms: now_ms(),
        images: out,
    };
    let _ = std::fs::create_dir_all(&s.state_dir);
    let _ = std::fs::write(
        s.state_dir.join("updates.json"),
        serde_json::to_string(&report).unwrap_or_default(),
    );
    *s.report.lock().unwrap() = report;
    s.changed();
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// notifications
// ---------------------------------------------------------------------------------------------

fn with_stack(ev: NewEvent, stack: &Option<String>) -> NewEvent {
    match stack {
        Some(s) => field(ev, "stack", s),
        None => ev,
    }
}

/// The notification of an event transition.
pub fn alert_event(a: &Alert) -> NewEvent {
    const SRC: &str = "docker";
    match a {
        Alert::Unhealthy { name, stack } => {
            let mut ev = firing(
                NotifyCategory::App,
                NotifySeverity::Warning,
                SRC,
                format!("docker:health:{name}"),
                format!("Container {name} is unhealthy"),
            );
            ev.body = "Its health check fails. Look at its logs on the Docker page.".into();
            with_stack(
                field(code(ev, "docker.unhealthy"), "container", name),
                stack,
            )
        }
        Alert::Recovered { name, stack } => {
            let ev = resolved(
                NotifyCategory::App,
                SRC,
                format!("docker:health:{name}"),
                format!("Container {name} is healthy again"),
            );
            with_stack(
                field(code(ev, "docker.recovered"), "container", name),
                stack,
            )
        }
        Alert::Oom { name, stack } => {
            let mut ev = oneshot(
                NotifyCategory::App,
                NotifySeverity::Warning,
                SRC,
                format!("docker:oom:{name}"),
                format!("Container {name} ran out of memory"),
            );
            ev.body =
                "The kernel stopped a process of the container because it used up its memory."
                    .into();
            with_stack(field(code(ev, "docker.oom"), "container", name), stack)
        }
        Alert::CrashLoop {
            name,
            stack,
            restarts,
            minutes,
        } => {
            let mut ev = firing(
                NotifyCategory::App,
                NotifySeverity::Error,
                SRC,
                format!("docker:loop:{name}"),
                format!("Container {name} keeps restarting"),
            );
            ev.body = format!(
                "{restarts} restarts in {minutes} minutes. Look at its logs on the Docker page."
            );
            let ev = field(
                field(code(ev, "docker.crash_loop"), "container", name),
                "restarts",
                restarts,
            );
            with_stack(field(ev, "minutes", minutes), stack)
        }
        Alert::CrashLoopOver { name, stack } => {
            let ev = resolved(
                NotifyCategory::App,
                SRC,
                format!("docker:loop:{name}"),
                format!("Container {name} stopped restarting"),
            );
            with_stack(
                field(code(ev, "docker.crash_loop_over"), "container", name),
                stack,
            )
        }
    }
}

fn disk_event(full: bool, path: &str, used_pct: f64, limit: u8) -> NewEvent {
    if full {
        let mut ev = firing(
            NotifyCategory::Storage,
            NotifySeverity::Warning,
            "docker",
            "docker:disk",
            format!("Docker storage {used_pct:.0}% full"),
        );
        ev.body = format!(
            "The filesystem of {path} is almost full: images and containers may stop working. Remove unused images or move Docker to a larger volume."
        );
        let ev = field(
            field(code(ev, "docker.disk_full"), "path", path),
            "used_pct",
            format!("{used_pct:.0}"),
        );
        field(ev, "limit_pct", limit)
    } else {
        let ev = resolved(
            NotifyCategory::Storage,
            "docker",
            "docker:disk",
            format!("Docker storage back under {limit}%"),
        );
        field(
            field(code(ev, "docker.disk_ok"), "path", path),
            "limit_pct",
            limit,
        )
    }
}

fn update_event(u: &ImageUpdate) -> NewEvent {
    let mut ev = oneshot(
        NotifyCategory::Update,
        NotifySeverity::Info,
        "docker",
        format!("docker:update:{}", u.image),
        format!("Image update available: {}", u.image),
    );
    ev.body = if u.stacks.is_empty() {
        "A newer image is in the registry. Update from the Docker page.".into()
    } else {
        format!(
            "Used by {}. Update from the Docker page (a snapshot is taken first).",
            u.stacks.join(", ")
        )
    };
    let ev = field(code(ev, "docker.update_available"), "image", &u.image);
    field(ev, "stacks", u.stacks.join(", "))
}

// ---------------------------------------------------------------------------------------------
// the event watcher and the stats sampler
// ---------------------------------------------------------------------------------------------

/// Follows the engine's events: notifications on transitions, `docker_changed` hints, a
/// sweep every minute (crash loops over, data-root fill level). Reconnects with backoff
/// ([`Backoff`]: grows on quick failures, resets after a stable stream); sleeps while
/// Docker is absent.
///
/// Hints are coalesced: the first event that changes the lists
/// ([`rynas_docker::events::changes_lists`]) arms a [`HINT_DELAY`] timer, and one
/// `docker_changed` covers every event until it fires. Nothing wakes up while the engine is
/// quiet (no per-second tick).
pub fn spawn_watch(s: Arc<DockerSvc>) {
    tokio::spawn(async move {
        let set = s.settings();
        let mut tracker = Tracker::new(set.crash_restarts, set.crash_window_min);
        let mut disk = DiskWatch::default();
        let mut backoff = Backoff::new(MAX_BACKOFF);
        loop {
            let e = match s.engine_opt().await {
                Ok(e) => e,
                Err(_) => {
                    s.drop_engine().await;
                    tokio::time::sleep(backoff.after(Duration::ZERO)).await;
                    continue;
                }
            };
            let up = Instant::now();
            // first reading after a (re)connection
            if let Ok(cs) = e.containers().await {
                let un: Vec<(String, Option<String>)> = cs
                    .iter()
                    .filter(|x| x.c.health == Some(ContainerHealth::Unhealthy))
                    .map(|x| (x.c.name.clone(), x.c.stack.clone()))
                    .collect();
                for a in tracker.seed(&un) {
                    s.notifier.emit(alert_event(&a));
                }
            }
            s.changed();
            let mut events = Box::pin(e.events());
            let mut sweep = tokio::time::interval(SWEEP);
            sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut hint: Option<tokio::time::Instant> = None;
            loop {
                tokio::select! {
                    ev = events.next() => match ev {
                        Some(Ok(m)) => {
                            if hint.is_none() && rynas_docker::events::changes_lists(&m) {
                                hint = Some(tokio::time::Instant::now() + HINT_DELAY);
                            }
                            if let Some(ev) = parse_event(&m) {
                                for a in tracker.on_event(&ev) {
                                    s.notifier.emit(alert_event(&a));
                                }
                            }
                        }
                        Some(Err(err)) => {
                            tracing::debug!("docker events: {err}");
                            break;
                        }
                        None => break,
                    },
                    _ = tokio::time::sleep_until(hint.unwrap_or_else(tokio::time::Instant::now)), if hint.is_some() => {
                        hint = None;
                        s.changed();
                    }
                    _ = sweep.tick() => {
                        let set = s.settings();
                        tracker.set_limits(set.crash_restarts, set.crash_window_min);
                        if let Ok(cs) = e.containers().await {
                            let present: HashSet<String> = cs.iter().map(|x| x.c.name.clone()).collect();
                            for a in tracker.sweep(rynas_docker::now_s(), &present) {
                                s.notifier.emit(alert_event(&a));
                            }
                        }
                        if let Ok(i) = e.info().await
                            && let Some(dr) = i.docker_root_dir
                        {
                            let p = dr.clone();
                            let u = tokio::task::spawn_blocking(move || fs_usage(&p)).await.ok().flatten();
                            if let Some(u) = u.filter(|u| u.total_bytes > 0) {
                                let pct = u.used_bytes as f64 * 100.0 / u.total_bytes as f64;
                                if let Some(full) = disk.check(pct, set.data_root_full_pct) {
                                    s.notifier.emit(disk_event(full, &dr, pct, set.data_root_full_pct));
                                }
                            }
                        }
                    }
                }
            }
            if hint.is_some() {
                s.changed();
            }
            s.drop_engine().await;
            let wait = backoff.after(up.elapsed());
            tracing::debug!("docker events: reconnecting in {wait:?}");
            tokio::time::sleep(wait).await;
        }
    });
}

fn sample_once(sampler: &mut stats::Sampler, cs: &[Seen]) -> DockerStats {
    let targets: Vec<stats::Target> = cs
        .iter()
        .filter(|x| x.c.state == ContainerState::Running)
        .map(|x| (x.c.id.clone(), x.c.name.clone(), x.c.stack.clone()))
        .collect();
    let running = targets.len() as u32;
    let total = cs.len() as u32;
    let per = sampler.sample(&targets, mono_us());
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    let mem = stats::mem_total(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default());
    stats::aggregate(per, running, total, cpus, mem, now_ms())
}

/// The stats sampler: every [`STATS_TICK`] while a task stream listens with `?docker=1`;
/// the container list is read again on `docker_changed` or every 30 s. Idle otherwise.
pub fn spawn_stats(s: Arc<DockerSvc>) {
    tokio::spawn(async move {
        let mut sampler = stats::Sampler::new("/sys/fs/cgroup");
        let mut tick = tokio::time::interval(STATS_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut cached: Option<(Instant, u64, Vec<Seen>)> = None;
        loop {
            tick.tick().await;
            if s.live.receiver_count() == 0 {
                cached = None;
                continue;
            }
            let g = s.generation.load(Ordering::Relaxed);
            let stale = cached
                .as_ref()
                .is_none_or(|(t, cg, _)| *cg != g || t.elapsed() > Duration::from_secs(30));
            if stale {
                match s.engine_opt().await {
                    Ok(e) => match e.containers().await {
                        Ok(cs) => cached = Some((Instant::now(), g, cs)),
                        Err(_) => {
                            s.drop_engine().await;
                            continue;
                        }
                    },
                    Err(_) => continue,
                }
            }
            let Some((_, _, cs)) = cached.as_ref() else {
                continue;
            };
            let sample = sample_once(&mut sampler, cs);
            let j: Arc<str> = serde_json::to_string(&sample).unwrap_or_default().into();
            *s.last_stats.lock().unwrap() = Some(j.clone());
            let _ = s.live.send((EV_STATS, j));
        }
    });
}

// ---------------------------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------------------------

pub fn router(st: DkState) -> Router {
    // every route, reads included, needs an administrator (§5.1 "rynasd checks it again")
    let gate = st.as_ref().map(|s| crate::roles::AdminGate {
        policy: s.ops.admins.clone(),
        what: "Docker",
    });
    let r = docker_routes(st);
    match gate {
        Some(g) => r.route_layer(axum::middleware::from_fn_with_state(
            g,
            crate::roles::admin_only,
        )),
        None => r,
    }
}

fn docker_routes(st: DkState) -> Router {
    Router::new()
        .route("/v1/docker/status", get(status))
        .route("/v1/docker/settings", get(settings_get).put(settings_put))
        .route("/v1/docker/plan/engine", post(plan_engine))
        .route("/v1/docker/install", post(install_route))
        .route("/v1/docker/engine/migrate", post(migrate_route))
        .route("/v1/docker/containers", get(containers))
        .route("/v1/docker/containers/{id}/logs", get(logs))
        .route("/v1/docker/containers/{id}/logs/stream", get(logs_stream))
        .route(
            "/v1/docker/containers/{id}/{action}",
            post(container_action),
        )
        .route("/v1/docker/stacks", get(stacks).post(stack_create))
        .route("/v1/docker/stacks/validate", post(stack_validate))
        .route("/v1/docker/stacks/{name}", get(stack_get))
        .route(
            "/v1/docker/stacks/{name}/files",
            get(stack_files_get).put(stack_files_put),
        )
        .route(
            "/v1/docker/stacks/{name}/snapshots",
            get(snapshots_get).post(snapshot_take),
        )
        .route(
            "/v1/docker/stacks/{name}/snapshots/{id}",
            delete(snapshot_delete),
        )
        .route("/v1/docker/stacks/{name}/{action}", post(stack_action))
        .route("/v1/docker/images", get(images))
        .route("/v1/docker/volumes", get(volumes))
        .route("/v1/docker/networks", get(networks))
        .route("/v1/docker/df", get(disk_usage))
        .route("/v1/docker/stats", get(stats_get))
        .route("/v1/docker/updates", get(updates_get))
        .route("/v1/docker/updates/check", post(updates_check))
        .with_state(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_ranges() {
        assert!(normalize(DockerSettings::default()).is_ok());
        let bad = |f: &dyn Fn(&mut DockerSettings), field: &str| {
            let mut s = DockerSettings::default();
            f(&mut s);
            assert_eq!(normalize(s).unwrap_err().field.as_deref(), Some(field));
        };
        bad(&|s| s.volume = Some("Bad Label".into()), "volume");
        bad(&|s| s.keep_snapshots = 0, "keep_snapshots");
        bad(&|s| s.data_root_full_pct = 100, "data_root_full_pct");
        bad(&|s| s.crash_restarts = 1, "crash_restarts");
        bad(
            &|s| s.address_pool.base = "10.0.0.1/16".into(),
            "address_pool.base",
        );
    }

    #[test]
    fn data_root_labels() {
        assert_eq!(
            label_of_data_root("/vol1/docker/engine").as_deref(),
            Some("vol1")
        );
        assert_eq!(label_of_data_root("/var/lib/docker"), None);
        assert_eq!(label_of_data_root("/a/b/docker/engine"), None);
    }

    #[test]
    fn notification_codes() {
        let a = alert_event(&Alert::CrashLoop {
            name: "web".into(),
            stack: Some("site".into()),
            restarts: 5,
            minutes: 10,
        });
        assert_eq!(a.code, "docker.crash_loop");
        assert_eq!(a.fields.get("restarts").map(String::as_str), Some("5"));
        assert_eq!(a.fields.get("stack").map(String::as_str), Some("site"));
        let r = alert_event(&Alert::CrashLoopOver {
            name: "web".into(),
            stack: None,
        });
        assert_eq!(
            (r.key.as_deref(), r.code.as_str()),
            (Some("docker:loop:web"), "docker.crash_loop_over")
        );
        let d = disk_event(true, "/vol1/docker/engine", 93.4, 90);
        assert_eq!(
            (d.code.as_str(), d.fields["used_pct"].as_str()),
            ("docker.disk_full", "93")
        );
        let u = update_event(&ImageUpdate {
            image: "nginx:1".into(),
            stacks: vec!["web".into()],
            ..Default::default()
        });
        assert_eq!(u.category, NotifyCategory::Update);
        assert_eq!(u.fields["stacks"], "web");
    }

    #[test]
    fn copy_line() {
        assert_eq!(
            copy_cmd("/var/lib/docker", "/vol1/docker/engine").display(),
            "cp -a --reflink=auto /var/lib/docker/. /vol1/docker/engine/"
        );
    }
}
