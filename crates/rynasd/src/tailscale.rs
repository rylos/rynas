//! Tailscale (`/v1/tailscale...`, public under `/api/v1/tailscale...`): remote access of
//! phase 2 (`docs/phase2/remote-access.md`). The package side lives in [`rynas_tailscale`];
//! this module holds the routes and the tasks.
//!
//! - Every route needs the administrator role (reads too: tailnet names and addresses).
//! - Install, uninstall, login, logout and option changes are tasks `tailscale.*` (user,
//!   time and log in the task history) and audit lines. Tailscale keeps its own state, so
//!   none of it is a configuration key of the store.
//! - An option change waits for a confirmation ([`CONFIRM_WINDOW`]): the page confirms as
//!   soon as it can reach rynas again after the task; if the change cut that path (routes
//!   accepted from the tailnet hiding the LAN), rynasd puts the previous options back. The
//!   window starts right after `tailscale set`; while it is open, options, login and logout
//!   are refused (409), so the previous options stay the ones before the first change. The
//!   pending revert is kept in `<state>/tailscale/confirm.json`: after a restart of rynasd
//!   the window goes on, or the revert runs at once when it has passed. A revert that finds
//!   another Tailscale task running waits for it.
//! - The status says whether the request itself came over the tailnet (`via_tailscale`:
//!   the client's address, as rynas-web resolved it behind rynas-caddy, or the local address
//!   of the connection), so the page warns before an action that may end the session.
//! - `--dry-system`: status reads go to the socket given with `--tailscale-socket` (a test
//!   double), changes are only logged.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::tailscale::*;
use rynas_api_types::{TaskStarted, error_codes};
use rynas_tailscale::{cli, install, localapi, under};
use rynas_tasks::{Cmd, CmdOutput, TaskCtx};

use crate::ops::{Error, Ops, Res, user};

/// How long an option change waits for its confirmation.
pub const CONFIRM_WINDOW: Duration = Duration::from_secs(90);
/// How long a status read is reused (the page reads it every few seconds during a login).
const STATUS_TTL: Duration = Duration::from_secs(2);
const LOCALAPI_TIMEOUT: Duration = Duration::from_secs(3);

struct Pending {
    deadline_ms: u64,
    previous: TailscaleOptions,
    generation: u64,
}

/// [`Pending`] on disk (`<state>/tailscale/confirm.json`).
#[derive(serde::Serialize, serde::Deserialize)]
struct PendingFile {
    deadline_ms: u64,
    previous: TailscaleOptions,
}

const PENDING_FILE: &str = "confirm.json";
/// How often a revert that found another task running tries again.
const REVERT_RETRY: Duration = Duration::from_millis(500);

/// What the Tailscale endpoints work on.
pub struct TailscaleSvc {
    pub ops: Arc<Ops>,
    /// The configuration root (`/`, or the dry root): files rynas writes, the installed CLI.
    pub root: PathBuf,
    pub dry: bool,
    /// tailscaled's socket; `None`: never contacted.
    pub socket: Option<PathBuf>,
    /// `<state>/tailscale`: the auth key file during a login.
    pub state_dir: PathBuf,
    /// Where `/etc/os-release` is read (`/`; a fake tree in tests).
    pub host_root: PathBuf,
    task: Mutex<Option<String>>,
    confirm: Mutex<Option<Pending>>,
    generation: std::sync::atomic::AtomicU64,
    /// [`CONFIRM_WINDOW`] in ms (shorter in tests: [`TailscaleSvc::set_confirm_window`]).
    confirm_window_ms: std::sync::atomic::AtomicU64,
    cache: tokio::sync::Mutex<Option<(Instant, TailscaleStatus)>>,
}

pub type TsState = Option<Arc<TailscaleSvc>>;

fn now_ms() -> u64 {
    rynas_tasks::now_ms()
}

fn audit(who: &str, action: &str) {
    tracing::info!(target: "rynas::audit", "tailscale {action} by {who}");
}

fn accepted(id: String) -> Response {
    (StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response()
}

fn busy() -> Error {
    Error::new(
        StatusCode::CONFLICT,
        error_codes::BUSY,
        "another Tailscale task is running".into(),
    )
}

fn not_installed() -> Error {
    Error::new(
        StatusCode::CONFLICT,
        error_codes::TAILSCALE_NOT_INSTALLED,
        "Tailscale is not installed".into(),
    )
}

fn svc(st: &TsState) -> Res<Arc<TailscaleSvc>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "Tailscale is not enabled in this rynasd".into(),
        )
    })
}

/// The session came over the tailnet: the client's address (from rynas-web: the peer, or the
/// address rynas-caddy forwarded) or the local address of its connection is a tailnet one.
fn via_tailscale(h: &HeaderMap) -> bool {
    let addr = |name: &str| {
        h.get(name).and_then(|v| v.to_str().ok()).and_then(|v| {
            v.parse::<SocketAddr>()
                .map(|s| s.ip())
                .or_else(|_| v.parse::<IpAddr>())
                .ok()
        })
    };
    [rynas_auth::ipc::CLIENT_ADDR_HEADER, rynas_auth::ipc::LOCAL_ADDR_HEADER]
        .iter()
        .filter_map(|n| addr(n))
        .any(rynas_tailscale::is_tailnet)
}

fn confirm_pending() -> Error {
    Error::new(
        StatusCode::CONFLICT,
        error_codes::CONFLICT,
        "an option change waits for its confirmation: confirm it or wait for the previous options to come back".into(),
    )
}

/// IPv4 subnets of the LAN interfaces (`ip -j addr show`), what "advertise this LAN" offers.
fn lan_subnets() -> Vec<String> {
    let out = Cmd::new("ip")
        .args(["-j", "addr", "show"])
        .env_clear()
        .timeout(Duration::from_secs(5))
        .output_blocking();
    let Ok(out) = out else { return Vec::new() };
    let facts = crate::web_access::model::parse_facts("", &out.stdout, "[]");
    facts
        .lan
        .iter()
        .filter(|c| c.addr.is_ipv4())
        .map(|c| c.to_string())
        .collect()
}

impl TailscaleSvc {
    pub fn new(
        ops: Arc<Ops>,
        root: PathBuf,
        dry: bool,
        socket: Option<PathBuf>,
        state_dir: PathBuf,
        host_root: PathBuf,
    ) -> Arc<Self> {
        // an auth key or a key download left by a stop in the middle of a task
        for f in ["authkey", install::KEY_STAGING_NAME] {
            let _ = std::fs::remove_file(state_dir.join(f));
        }
        Arc::new(Self {
            ops,
            root,
            dry,
            socket,
            state_dir,
            host_root,
            task: Mutex::new(None),
            confirm: Mutex::new(None),
            generation: Default::default(),
            confirm_window_ms: (CONFIRM_WINDOW.as_millis() as u64).into(),
            cache: tokio::sync::Mutex::new(None),
        })
    }

    /// Test hook: another confirmation window than [`CONFIRM_WINDOW`].
    #[doc(hidden)]
    pub fn set_confirm_window(&self, d: Duration) {
        self.confirm_window_ms
            .store(d.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// Test hook: marks the task slot taken (`Some`) or free, as a running task would.
    #[doc(hidden)]
    pub fn hold_task_slot(&self, id: Option<&str>) {
        *self.task.lock().unwrap() = id.map(str::to_string);
    }

    /// Picks up an option change left waiting by a previous rynasd
    /// (`<state>/tailscale/confirm.json`): the window goes on, or the previous options come
    /// back at once when it has passed. Call once, inside the runtime.
    pub fn resume_pending(self: &Arc<Self>) {
        let p = self.state_dir.join(PENDING_FILE);
        let Ok(text) = std::fs::read(&p) else { return };
        match serde_json::from_slice::<PendingFile>(&text) {
            Ok(f) => {
                tracing::info!(
                    "tailscale: an option change waits for its confirmation since before the restart"
                );
                self.arm_until(f.previous, f.deadline_ms);
            }
            Err(e) => {
                tracing::warn!("tailscale: {}: {e}", p.display());
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    fn save_pending(&self, p: Option<&PendingFile>) {
        let path = self.state_dir.join(PENDING_FILE);
        let r = match p {
            Some(f) => std::fs::create_dir_all(&self.state_dir).and_then(|_| {
                let tmp = path.with_extension("json.new");
                std::fs::write(&tmp, serde_json::to_vec(f).unwrap_or_default())?;
                std::fs::rename(&tmp, &path)
            }),
            None => match std::fs::remove_file(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                r => r,
            },
        };
        if let Err(e) = r {
            tracing::warn!("tailscale: {}: {e}", path.display());
        }
    }

    /// Drops the pending confirmation (confirmed, uninstalled); `true` when there was one.
    fn clear_pending(&self) -> bool {
        let had = self.confirm.lock().unwrap().take().is_some();
        self.save_pending(None);
        had
    }

    fn distro(&self) -> Option<install::Distro> {
        std::fs::read_to_string(under(&self.host_root, "/etc/os-release"))
            .ok()
            .and_then(|t| install::distro(&t))
    }

    fn prefs_blocking(&self) -> Option<localapi::Prefs> {
        let sock = self.socket.as_deref()?;
        localapi::get(sock, "/localapi/v0/prefs", LOCALAPI_TIMEOUT)
            .ok()
            .and_then(|j| localapi::parse_prefs(&j).ok())
    }

    /// The status without the per-request fields (`via_tailscale`, task, confirmation).
    fn read_blocking(&self) -> TailscaleStatus {
        let mut s = TailscaleStatus {
            installed: under(&self.root, rynas_tailscale::CLI).exists(),
            ..Default::default()
        };
        if let Some(sock) = self.socket.as_deref() {
            match localapi::get(sock, "/localapi/v0/status", LOCALAPI_TIMEOUT) {
                Ok(j) => {
                    let prefs = self.prefs_blocking();
                    if let Err(e) = localapi::fill_status(&mut s, &j, prefs.as_ref()) {
                        tracing::warn!("tailscale: {e}");
                    }
                }
                Err(e) if s.installed => tracing::debug!("tailscale: {e}"),
                Err(_) => {}
            }
        }
        if !s.installed {
            s.install_block = match self.distro() {
                None => Some(TailscaleBlock::UnsupportedOs),
                Some(d) if d.codename.is_none() => Some(TailscaleBlock::UnknownRelease),
                Some(_) => None,
            };
        } else {
            s.lan_subnets = lan_subnets();
        }
        s
    }

    /// The status, reused for [`STATUS_TTL`] (one read at a time).
    pub async fn status(self: &Arc<Self>, fresh: bool) -> TailscaleStatus {
        let mut c = self.cache.lock().await;
        if !fresh
            && let Some((at, s)) = c.as_ref()
            && at.elapsed() < STATUS_TTL
        {
            return s.clone();
        }
        let me = self.clone();
        let s = tokio::task::spawn_blocking(move || me.read_blocking())
            .await
            .unwrap_or_default();
        *c = Some((Instant::now(), s.clone()));
        s
    }

    async fn invalidate(&self) {
        *self.cache.lock().await = None;
    }

    /// Runs a command of a task: logged and run, or only logged under `--dry-system`.
    async fn run(&self, ctx: &TaskCtx, cmd: &Cmd) -> Result<CmdOutput, String> {
        if self.dry {
            ctx.log(&format!("$ {} (dry system: not run)", cmd.display()));
            return Ok(CmdOutput {
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        match ctx.run(cmd).await {
            Ok(o) => Ok(o),
            Err(e) => {
                if matches!(
                    e,
                    rynas_tasks::CmdError::Cancelled | rynas_tasks::CmdError::Timeout(_)
                ) && let Some(stop) = install::on_abort(cmd)
                {
                    ctx.log(&format!("$ {}", stop.display()));
                    let _ = stop.run_checked(None, None).await;
                }
                Err(format!("{}: {e}", cmd.display()))
            }
        }
    }

    /// Writes a whole file through a temporary one. Only paths rynasd can write inside its
    /// sandbox (`ProtectSystem=yes`: not /usr); the rest goes through
    /// [`install::outside`]. In a `.d` directory (`sources.list.d`, `sysctl.d`) the temporary
    /// file is staged in the parent, where nothing reads it.
    fn write_file(&self, ctx: &TaskCtx, path: &str, content: &[u8], mode: u32) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        if !install::writable_in_sandbox(path) {
            return Err(format!("{path}: read-only inside rynasd's sandbox"));
        }
        ctx.log(&format!("writing {path}"));
        let p = under(&self.root, path);
        let dir = p.parent().ok_or_else(|| format!("{path}: no folder"))?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stage_dir = match dir.parent() {
            Some(up) if dir.extension().is_some_and(|e| e == "d") => up,
            _ => dir,
        };
        let tmp = stage_dir.join(format!(".{name}.rynas-new"));
        std::fs::write(&tmp, content).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
            .map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &p).map_err(|e| format!("{}: {e}", p.display()))
    }

    fn remove_file(&self, ctx: &TaskCtx, path: &str) {
        debug_assert!(install::writable_in_sandbox(path), "{path}");
        let p = under(&self.root, path);
        if p.exists() {
            ctx.log(&format!("removing {path}"));
            if let Err(e) = std::fs::remove_file(&p) {
                ctx.log(&format!("{path}: {e}"));
            }
        }
    }

    /// Starts a task `tailscale.<kind>` unless one runs.
    fn spawn<F, Fut>(self: &Arc<Self>, kind: &str, who: &str, job: F) -> Res<String>
    where
        F: FnOnce(Arc<Self>, TaskCtx) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        let mut slot = self.task.lock().unwrap();
        if slot.is_some() {
            return Err(busy());
        }
        let me = self.clone();
        let id = self.ops.tasks.spawn(&format!("tailscale.{kind}"), who, move |ctx| async move {
            let r = job(me.clone(), ctx).await;
            *me.task.lock().unwrap() = None;
            me.invalidate().await;
            r
        })?;
        *slot = Some(id.clone());
        Ok(id)
    }

    /// The forwarding sysctl follows the options: written and loaded while a subnet router
    /// or exit node is offered, removed otherwise (the running value stays until a reboot:
    /// Docker needs forwarding too).
    async fn forwarding(&self, ctx: &TaskCtx, o: &TailscaleOptions) -> Result<(), String> {
        if o.exit_node || !o.advertise_routes.is_empty() {
            self.write_file(ctx, install::SYSCTL, install::sysctl_content().as_bytes(), 0o644)?;
            self.run(ctx, &install::sysctl_load()).await?;
        } else {
            self.remove_file(ctx, install::SYSCTL);
        }
        Ok(())
    }

    /// Opens the confirmation window of an option change: `previous` comes back unless it is
    /// confirmed in time.
    fn arm_confirm(self: &Arc<Self>, previous: TailscaleOptions) -> u64 {
        let window = self.confirm_window_ms.load(std::sync::atomic::Ordering::Relaxed);
        self.arm_until(previous, now_ms() + window)
    }

    fn arm_until(self: &Arc<Self>, previous: TailscaleOptions, deadline_ms: u64) -> u64 {
        let generation = self.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        self.save_pending(Some(&PendingFile {
            deadline_ms,
            previous: previous.clone(),
        }));
        *self.confirm.lock().unwrap() = Some(Pending {
            deadline_ms,
            previous,
            generation,
        });
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(deadline_ms.saturating_sub(now_ms()))).await;
            me.expire(generation).await;
        });
        generation
    }

    /// The window of `generation` has passed: the previous options come back, as soon as no
    /// other Tailscale task runs. The pending change stays until the revert starts (options,
    /// login and logout stay refused meanwhile); a confirmation that still arrives first
    /// keeps the change.
    async fn expire(self: Arc<Self>, generation: u64) {
        let mut warned = false;
        loop {
            {
                let mut c = self.confirm.lock().unwrap();
                let Some(p) = c.as_ref().filter(|p| p.generation == generation) else {
                    return;
                };
                let prev = p.previous.clone();
                let r = self.spawn("revert", "rynas", move |s, ctx| async move {
                    ctx.log("the option change was not confirmed in time: previous options back");
                    s.run(&ctx, &cli::set(&prev)).await?;
                    s.forwarding(&ctx, &prev).await
                });
                match r {
                    Ok(_) => {
                        c.take();
                        drop(c);
                        tracing::warn!(
                            "tailscale: option change not confirmed, putting the previous options back"
                        );
                        self.save_pending(None);
                        return;
                    }
                    Err(e) if !warned => {
                        warned = true;
                        tracing::warn!("tailscale: revert waits: {}", e.2);
                    }
                    Err(_) => {}
                }
            }
            tokio::time::sleep(REVERT_RETRY).await;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------------------------

async fn status(State(st): State<TsState>, hd: HeaderMap) -> Res<Json<TailscaleStatus>> {
    let s = svc(&st)?;
    let mut out = s.status(false).await;
    out.via_tailscale = via_tailscale(&hd);
    out.task_id = s.task.lock().unwrap().clone();
    out.confirm = s.confirm.lock().unwrap().as_ref().map(|p| TailscaleConfirm {
        deadline_ms: p.deadline_ms,
    });
    Ok(Json(out))
}

fn body<T: serde::de::DeserializeOwned + Default>(b: &axum::body::Bytes) -> Res<T> {
    if b.iter().all(u8::is_ascii_whitespace) {
        Ok(T::default())
    } else {
        rynas_http::from_slice(b).map_err(Error::from)
    }
}

async fn install_route(State(st): State<TsState>, hd: HeaderMap) -> Res<Response> {
    let (s, who) = (svc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Tailscale install")?;
    let f = s.status(true).await;
    if f.installed {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            "Tailscale is already installed".into(),
        ));
    }
    let Some((d, code)) = s
        .distro()
        .and_then(|d| d.codename.clone().map(|c| (d, c)))
    else {
        return Err(Error::new(
            StatusCode::NOT_IMPLEMENTED,
            error_codes::NOT_AVAILABLE,
            "Tailscale has no apt repository for this system".into(),
        ));
    };
    audit(&who, "install");
    let id = s.spawn("install", &who, move |s, ctx| async move {
        ctx.log(&format!("installing Tailscale from {} ({code})", install::repo_url(&d)));
        let key_ok = std::fs::read(under(&s.root, install::KEYRING))
            .is_ok_and(|k| install::check_key(&k).is_ok());
        let staged = s.state_dir.join(install::KEY_STAGING_NAME);
        if !s.dry {
            std::fs::create_dir_all(&s.state_dir)
                .map_err(|e| format!("{}: {e}", s.state_dir.display()))?;
        }
        let steps = install::install_steps(&d, &code, key_ok, &staged.to_string_lossy());
        let n = steps.len();
        for (i, step) in steps.into_iter().enumerate() {
            if ctx.is_cancelled() {
                return Err("cancelled".into());
            }
            match step {
                install::Step::Run(c) => {
                    s.run(&ctx, &c).await?;
                }
                install::Step::CheckKey { staged } => {
                    let p = PathBuf::from(&staged);
                    if s.dry && !p.exists() {
                        ctx.log(&format!(
                            "checking the key fingerprint {} (dry system: not downloaded)",
                            install::KEY_FINGERPRINT
                        ));
                    } else {
                        let k = std::fs::read(&p).unwrap_or_default();
                        if let Err(e) = install::check_key(&k) {
                            let _ = std::fs::remove_file(&p);
                            return Err(e);
                        }
                        ctx.log(&format!("key fingerprint {} checked", install::KEY_FINGERPRINT));
                    }
                    // /usr is read-only inside rynasd: placed by `install` outside the sandbox
                    let r = s.run(&ctx, &install::place_key(&staged)).await;
                    let _ = std::fs::remove_file(&p);
                    r?;
                }
                install::Step::Write { path, content, mode } => {
                    s.write_file(&ctx, path, content.as_bytes(), mode)?;
                }
            }
            ctx.progress(((i + 1) * 100 / n) as u8);
        }
        ctx.log("Tailscale installed: log this NAS in to your tailnet");
        Ok(())
    })?;
    Ok(accepted(id))
}

async fn uninstall_route(
    State(st): State<TsState>,
    hd: HeaderMap,
    b: axum::body::Bytes,
) -> Res<Response> {
    let (s, who) = (svc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Tailscale uninstall")?;
    let req: TailscaleUninstall = body(&b)?;
    if !s.status(true).await.installed {
        return Err(not_installed());
    }
    audit(&who, if req.purge_state { "uninstall (purge)" } else { "uninstall" });
    let id = s.spawn("uninstall", &who, move |s, ctx| async move {
        s.clear_pending();
        for c in install::uninstall_cmds(req.purge_state) {
            if ctx.is_cancelled() {
                return Err("cancelled".into());
            }
            let r = s.run(&ctx, &c).await;
            // logging out an already logged-out node fails: not a reason to stop
            if let Err(e) = r {
                if c.display().contains(" logout") {
                    ctx.log(&e);
                } else {
                    return Err(e);
                }
            }
        }
        for f in install::UNINSTALL_FILES {
            s.remove_file(&ctx, f);
        }
        ctx.log("Tailscale removed");
        Ok(())
    })?;
    Ok(accepted(id))
}

async fn login_route(
    State(st): State<TsState>,
    hd: HeaderMap,
    b: axum::body::Bytes,
) -> Res<Response> {
    let (s, who) = (svc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Tailscale login")?;
    let req: TailscaleLogin = body(&b)?;
    cli::check_login(&req).map_err(|(f, m)| Error::invalid(f, m))?;
    if s.confirm.lock().unwrap().is_some() {
        return Err(confirm_pending());
    }
    let st_now = s.status(true).await;
    if !st_now.installed {
        return Err(not_installed());
    }
    if !st_now.running {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "tailscaled is not running".into(),
        ));
    }
    audit(&who, if req.auth_key.is_some() { "login (auth key)" } else { "login" });
    let id = s.spawn("login", &who, move |s, ctx| async move {
        let me = s.clone();
        let prefs = tokio::task::spawn_blocking(move || me.prefs_blocking())
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        // a node that never logged in starts with the server defaults
        let keep = if prefs.control_url.is_empty() {
            TailscaleOptions::default()
        } else {
            prefs.options.clone()
        };
        let key = req.auth_key.as_deref().filter(|k| !k.is_empty());
        let key_path = s.state_dir.join("authkey");
        if let Some(k) = key {
            ctx.log("using the auth key given (not logged)");
            if !s.dry {
                write_secret(&key_path, k)?;
            }
        } else {
            ctx.log("interactive login: open the address shown on the page (it is also below)");
        }
        let kp = key_path.to_string_lossy().into_owned();
        let cmd = cli::login(&req, &keep, Some(&prefs.control_url), key.map(|_| kp.as_str()));
        let r = s.run(&ctx, &cmd).await;
        if key.is_some() {
            let _ = std::fs::remove_file(&key_path);
        }
        r.map(|_| ())
    })?;
    Ok(accepted(id))
}

/// The auth key in a root-only file for the CLI's `--auth-key=file:` (removed after).
fn write_secret(path: &Path, key: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(key.as_bytes()).map_err(|e| e.to_string())
}

async fn logout_route(State(st): State<TsState>, hd: HeaderMap) -> Res<Response> {
    let (s, who) = (svc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Tailscale logout")?;
    if s.confirm.lock().unwrap().is_some() {
        return Err(confirm_pending());
    }
    if !s.status(true).await.installed {
        return Err(not_installed());
    }
    audit(&who, "logout");
    let id = s.spawn("logout", &who, move |s, ctx| async move {
        s.run(&ctx, &cli::logout()).await.map(|_| ())
    })?;
    Ok(accepted(id))
}

async fn options_route(
    State(st): State<TsState>,
    hd: HeaderMap,
    b: axum::body::Bytes,
) -> Res<Response> {
    let (s, who) = (svc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Tailscale options")?;
    let mut o: TailscaleOptions = rynas_http::from_slice(&b).map_err(Error::from)?;
    o.advertise_routes =
        cli::normalize_routes(&o.advertise_routes).map_err(|m| Error::invalid("advertise_routes", m))?;
    if s.confirm.lock().unwrap().is_some() {
        return Err(confirm_pending());
    }
    let now = s.status(true).await;
    if !now.installed {
        return Err(not_installed());
    }
    if !now.running {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "tailscaled is not running".into(),
        ));
    }
    // the previous options must be known, or a revert would put defaults back
    let me = s.clone();
    let Some(prefs) = tokio::task::spawn_blocking(move || me.prefs_blocking())
        .await
        .ok()
        .flatten()
    else {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "the current options cannot be read from tailscaled: nothing changed".into(),
        ));
    };
    audit(&who, "options");
    let previous = prefs.options;
    let id = s.spawn("options", &who, move |s, ctx| async move {
        s.run(&ctx, &cli::set(&o)).await?;
        // the window opens as soon as the change is made (the forwarding below may fail)
        s.arm_confirm(previous);
        let window = s.confirm_window_ms.load(std::sync::atomic::Ordering::Relaxed) / 1000;
        ctx.log(&format!(
            "waiting {window} s for the confirmation, then the previous options come back"
        ));
        s.forwarding(&ctx, &o).await?;
        if !o.advertise_routes.is_empty() || o.exit_node {
            ctx.log("advertised routes and exit nodes must be approved in the tailnet's admin console (unless auto-approved)");
        }
        Ok(())
    })?;
    Ok(accepted(id))
}

async fn confirm_route(State(st): State<TsState>, hd: HeaderMap) -> Res<StatusCode> {
    let (s, who) = (svc(&st)?, user(&hd)?);
    s.ops.admins.require_admin(&who, "Tailscale options")?;
    match s.clear_pending() {
        true => {
            audit(&who, "options confirmed");
            Ok(StatusCode::NO_CONTENT)
        }
        false => Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::NOTHING_PENDING,
            "no option change waits for a confirmation".into(),
        )),
    }
}

pub fn router(st: TsState) -> Router {
    let gate = st.as_ref().map(|s| crate::roles::AdminGate {
        policy: s.ops.admins.clone(),
        what: "Tailscale",
    });
    let r = Router::new()
        .route("/v1/tailscale", get(status))
        .route("/v1/tailscale/install", post(install_route))
        .route("/v1/tailscale/uninstall", post(uninstall_route))
        .route("/v1/tailscale/login", post(login_route))
        .route("/v1/tailscale/logout", post(logout_route))
        .route("/v1/tailscale/options", axum::routing::put(options_route))
        .route("/v1/tailscale/options/confirm", post(confirm_route))
        .with_state(st);
    match gate {
        Some(g) => r.route_layer(axum::middleware::from_fn_with_state(
            g,
            crate::roles::admin_only,
        )),
        None => r,
    }
}
