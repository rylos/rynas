//! UPS endpoints of the internal API (`/v1/ups/...`, public under `/api/v1/ups/...`), the
//! `ups.safemode` configuration domain, the on-battery watcher and the pre-shutdown stop.
//!
//! Two backends ([`UpsBackend`]); the generic one comes first:
//!
//! - **NUT** (any Debian with the `nut` package, `/etc/nut` present). rynas never rewrites the
//!   user's NUT files: upsmon keeps shutting the system down on low battery as configured.
//!   - `ups.safemode` owns the rynas file `/etc/rynas/ups.conf` (`SHUTDOWN_AFTER`,
//!     `STOP_TIMEOUT`). [`watch`] reads upsd every few seconds; after `SHUTDOWN_AFTER` seconds
//!     on battery it stops rynas's work ([`stop_work`]) and asks upsmon for the shutdown
//!     (`upsmon -c fsd`, the call NUT documents for "shut down before the battery is
//!     critical"; `systemctl poweroff` if upsmon refuses). When the UPS turns critical (OB+LB
//!     or FSD) it only stops rynas's work: upsmon shuts down by itself. It also records
//!     "on battery" and "power back" as notifications.
//!   - `POST /v1/ups/test` sends the NUT instant command (`test.battery.start.quick|deep`,
//!     `test.battery.stop`) as the upsd user `rynas`: a managed block of
//!     `/etc/nut/upsd.users` (`instcmds` limited to the three tests) whose password is
//!     generated once into `/etc/rynas/nut-upsd.secret` (root, 0600) and never logged. The
//!     block is written (and upsd reloaded) only when a test runs and it is missing or
//!     changed; a `[rynas]` user outside the block is an error, not overwritten.
//! - **a module's UPS layer** (`rynas_module::UpsLayer`, D-MODULE): a hardware module whose
//!   system layer runs NUT itself. rynas is the interface, the layer the executor:
//!   - `ups.safemode` writes the layer's policy file (`UpsLayer::render_policy`);
//!   - `POST /v1/ups/test` runs the layer's test command;
//!   - `POST /v1/ups/safe-mode` (root only, `rynasctl ups safe-mode <reason>` from the
//!     layer's pre-shutdown hook): the same [`stop_work`];
//!   - rynasd does not follow upsd: the layer shuts the system down.
//!
//! Either way the test runs as the task `ups.test`, which follows `ups.test.result` until the
//! UPS reports an outcome. The UPS status itself is read from upsd by rynas-web (no root).

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
use rynas_api_types::ups::{
    BatteryTest, BatteryTestRequest, PRE_SAFEMODE_TIMEOUT_RANGE_S, SAFE_MODE_REASONS,
    SAFEMODE_AFTER_RANGE_S, SafeModeReport, SafeModeRequest, UpsBackend, UpsLink, UpsSettings,
    UpsSettingsState, UpsStatus,
};
use rynas_api_types::{TaskStarted, TaskStatus};
use rynas_config::files::{self, FileKind};
use rynas_module::UpsLayer;
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile};
use rynas_notify::Notifier;
use rynas_notify::event::oneshot;
use rynas_tasks::{Cmd, TaskCtx};
use serde_json::Value;

use crate::ops::{Error, Ops, Res, user};
use crate::services::{docs, stage};

pub const UPS_KEY: &str = "ups.safemode";
/// The policy file of the NUT backend (rynas-owned, read by [`watch`]).
pub const NUT_POLICY: &str = "/etc/rynas/ups.conf";
pub const NUT_CONF: &str = "/etc/nut/nut.conf";
pub const NUT_USERS: &str = "/etc/nut/upsd.users";
/// Password of the rynas upsd user (root 0600).
pub const NUT_SECRET: &str = "/etc/rynas/nut-upsd.secret";
pub const NUT_USER: &str = "rynas";
/// Keys of the two settings in the rynas policy file.
const NUT_KEYS: (&str, &str) = ("SHUTDOWN_AFTER", "STOP_TIMEOUT");
/// Server-side bound of the pre-shutdown work asked by a module layer's hook (the hook's
/// `rynasctl` waits 30 s; the layer's own budget for all its hooks is larger).
pub const SAFE_MODE_BUDGET: Duration = Duration::from_secs(25);
/// Longest wait for the outcome of a battery test.
const QUICK_MAX: Duration = Duration::from_secs(5 * 60);
const DEEP_MAX: Duration = Duration::from_secs(3 * 3600);
const RESULT_EVERY: Duration = Duration::from_secs(10);
const CMD_TIMEOUT: Duration = Duration::from_secs(30);
/// Watcher period while a UPS answers, and while none does.
const WATCH_EVERY: Duration = Duration::from_secs(5);
const WATCH_IDLE: Duration = Duration::from_secs(30);
const PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

// ---------------------------------------------------------------------------------------------
// the configuration domain
// ---------------------------------------------------------------------------------------------

/// `ups.safemode`: the module layer's policy file, or the rynas file `/etc/rynas/ups.conf`
/// (NUT). Registered on every system: the file is harmless without NUT.
pub struct UpsDomain {
    pub layer: Option<Arc<dyn UpsLayer>>,
}

fn settings_of(v: &Value) -> Result<UpsSettings, Invalid> {
    serde_json::from_value(v.clone())
        .map_err(|e| Invalid::from(format!("invalid ups.safemode document: {e}")))
}

/// Checks the ranges; the timer is kept in whole minutes (the UI's unit).
pub fn normalize(s: UpsSettings) -> Result<UpsSettings, Invalid> {
    if let Some(a) = s.safemode_after_s {
        let (lo, hi) = SAFEMODE_AFTER_RANGE_S;
        if a < lo || a > hi || a % 60 != 0 {
            return Err(Invalid::at(
                "safemode_after_s",
                format!("whole minutes between {} and {}", lo / 60, hi / 60),
            ));
        }
    }
    let (lo, hi) = PRE_SAFEMODE_TIMEOUT_RANGE_S;
    if s.pre_safemode_timeout_s < lo || s.pre_safemode_timeout_s > hi {
        return Err(Invalid::at(
            "pre_safemode_timeout_s",
            format!("between {lo} and {hi} seconds"),
        ));
    }
    Ok(s)
}

/// The rynas policy file of the NUT backend.
pub fn render_policy(s: &UpsSettings) -> String {
    format!(
        "# Shutdown policy of the rynas UPS page (ups.safemode), carried out by rynasd: after\n\
         # SHUTDOWN_AFTER seconds on battery (0: never) rynas stops its tasks within\n\
         # STOP_TIMEOUT seconds and asks NUT's upsmon to shut the system down (upsmon -c fsd).\n\
         # Low battery is always handled by upsmon itself, as configured in /etc/nut.\n\
         SHUTDOWN_AFTER={}\n\
         STOP_TIMEOUT={}\n",
        s.safemode_after_s.unwrap_or(0),
        s.pre_safemode_timeout_s
    )
}

/// The two keys as the shell would see them (the last assignment wins; comments skipped).
fn parse_keys(text: &str, (after, timeout): (&str, &str)) -> UpsSettings {
    let mut s = UpsSettings::default();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('#') {
            continue;
        }
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        // `X=1 # comment` and quotes
        let v = v
            .split('#')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches(['"', '\'']);
        let (k, n) = (k.trim(), v.parse::<u32>());
        match n {
            Ok(n) if k == after => s.safemode_after_s = (n > 0).then_some(n),
            Ok(n) if k == timeout => s.pre_safemode_timeout_s = n,
            _ => {}
        }
    }
    s
}

/// The settings of the rynas policy file.
pub fn parse_policy(text: &str) -> UpsSettings {
    parse_keys(text, NUT_KEYS)
}

impl UpsDomain {
    fn path(&self) -> &'static str {
        self.layer.as_ref().map_or(NUT_POLICY, |l| l.policy_path())
    }
}

impl Applier for UpsDomain {
    fn key(&self) -> &str {
        UPS_KEY
    }

    fn order(&self) -> i32 {
        50
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let s = normalize(settings_of(&value)?)?;
        serde_json::to_value(s).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        let s = settings_of(value).map_err(|e| e.message)?;
        Ok(vec![match &self.layer {
            Some(l) => l.render_policy(&s),
            None => ManagedFile::whole(NUT_POLICY, render_policy(&s), true),
        }])
    }

    /// Both readers (the layer, rynasd's watcher) read the file at the next power event.
    fn reload(&self, _value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        (ctx.log)(match &self.layer {
            Some(l) => l.policy_note(),
            None => "rynasd reads /etc/rynas/ups.conf at the next power event: no reload needed",
        });
        Ok(())
    }

    fn import(&self, ctx: &ApplyCtx) -> Result<Option<Value>, String> {
        let Some(text) = ctx.read(Path::new(self.path())) else {
            return Ok(None);
        };
        let s = match &self.layer {
            Some(l) => l.parse_policy(&text),
            None => parse_policy(&text),
        };
        serde_json::to_value(s).map(Some).map_err(|e| e.to_string())
    }
}

/// The backend of this machine: a module's layer (fixed at start), else NUT when its
/// configuration directory has `nut.conf` or `upsd.users` (checked at every call: NUT may
/// come later).
pub fn backend_of(root: &Path, layer: Option<&dyn UpsLayer>) -> UpsBackend {
    let has = |p: &str| files::under_root(root, Path::new(p)).is_file();
    if let Some(l) = layer {
        l.backend()
    } else if has(NUT_CONF) || has(NUT_USERS) {
        UpsBackend::Nut
    } else {
        UpsBackend::NotInstalled
    }
}

// ---------------------------------------------------------------------------------------------
// the rynas upsd user (NUT backend)
// ---------------------------------------------------------------------------------------------

/// The managed block of `upsd.users`: one `instcmds` line per command, as upsd.users(5)
/// documents, and nothing else (no `actions`, no `upsmon` role).
pub fn users_block(password: &str) -> String {
    let mut b = format!(
        "# rynas sends the battery tests of its UPS page to upsd as this user.\n\
         [{NUT_USER}]\n\tpassword = \"{password}\"\n"
    );
    for t in BatteryTest::ALL {
        b.push_str(&format!("\tinstcmds = {}\n", t.nut_cmd()));
    }
    b
}

fn users_kind() -> FileKind {
    FileKind::Block {
        id: "ups".into(),
        drop_outside: Vec::new(),
    }
}

/// 32 random letters and digits (a word upsd takes without quoting).
pub(crate) fn new_password() -> std::io::Result<String> {
    use std::io::Read;
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut raw = [0u8; 64];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut raw)?;
    // rejection sampling: no modulo bias
    let pw: String = raw
        .iter()
        .filter(|b| (**b as usize) < 248)
        .take(32)
        .map(|b| ABC[*b as usize % ABC.len()] as char)
        .collect();
    if pw.len() < 32 {
        return Err(std::io::Error::other("not enough random bytes"));
    }
    Ok(pw)
}

/// The password of the rynas upsd user, generated on first use (root 0600 below `root`).
pub fn nut_password(root: &Path) -> Result<String, String> {
    secret(root, NUT_SECRET)
}

/// A password kept in the file `name` (below `root`), generated on first use (root 0600).
pub fn secret(root: &Path, name: &str) -> Result<String, String> {
    let path = files::under_root(root, Path::new(name));
    if let Ok(t) = std::fs::read_to_string(&path) {
        let pw = t.lines().next().unwrap_or("").trim().to_string();
        if pw.len() >= 16 && rynas_nut::valid_name(&pw) {
            return Ok(pw);
        }
    }
    let pw = new_password().map_err(|e| format!("generating the upsd password: {e}"))?;
    let cand = files::candidate_path(&path);
    files::write_synced(&cand, format!("{pw}\n").as_bytes(), 0o600)
        .and_then(|_| files::rename_synced(&cand, &path))
        .map_err(|e| format!("{name}: {e}"))?;
    Ok(pw)
}

/// Makes sure `upsd.users` has the rynas block with `password`. Returns whether the file
/// changed (upsd must then reload it). Keeps the file's mode and owner (root:nut 0640).
pub fn ensure_nut_user(root: &Path, password: &str) -> Result<bool, String> {
    let path = files::under_root(root, Path::new(NUT_USERS));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("NUT is not installed or not readable ({NUT_USERS}: {e})"))?;
    let body = users_block(password);
    let kind = users_kind();
    if files::current_hash(&kind, &text) == Some(files::sha256_hex(body.as_bytes())) {
        return Ok(false);
    }
    let (new, _) = files::render_final(&kind, &body, Some(&text));
    let header = format!("[{NUT_USER}]");
    if new.lines().filter(|l| l.trim() == header).count() > 1 {
        return Err(format!(
            "{NUT_USERS} already has a [{NUT_USER}] user that rynas does not manage: \
             remove or rename it to run battery tests from rynas"
        ));
    }
    let meta = std::fs::metadata(&path).map_err(|e| format!("{NUT_USERS}: {e}"))?;
    let cand = files::candidate_path(&path);
    files::write_synced(&cand, new.as_bytes(), meta.mode() & 0o640)
        .and_then(|_| std::os::unix::fs::chown(&cand, Some(meta.uid()), Some(meta.gid())))
        .and_then(|_| files::rename_synced(&cand, &path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&cand);
            format!("{NUT_USERS}: {e}")
        })?;
    Ok(true)
}

// ---------------------------------------------------------------------------------------------
// endpoints
// ---------------------------------------------------------------------------------------------

pub struct Ups {
    pub ops: Arc<Ops>,
    /// Task Scheduler (its running units are stopped before the shutdown).
    pub protection: Option<Arc<crate::protection::Protection>>,
    pub notifier: Arc<dyn Notifier>,
    /// A module's UPS layer (fixed at start: the domain writes its file).
    pub layer: Option<Arc<dyn UpsLayer>>,
    /// Configuration root: NUT's files, the policy and the upsd secret are below it.
    pub root: PathBuf,
    /// `--dry-system`: layer tests, upsd reloads, unit stops and shutdowns are only logged.
    pub dry: bool,
    /// upsd's address.
    pub upsd: String,
    /// Battery test commands are sent to upsd and the watcher runs (off under
    /// `--dry-system` unless `--upsd` names a test upsd).
    pub talk_to_upsd: bool,
    /// The setup wizard ([`crate::ups_setup`]).
    pub setup: crate::ups_setup::Setup,
    /// Folder of the key that seals the network UPS server's passwords (`<state>/notify`,
    /// [`crate::ups_netserver`]).
    pub net_secret_dir: PathBuf,
}

impl Ups {
    pub fn backend(&self) -> UpsBackend {
        backend_of(&self.root, self.layer.as_deref())
    }

    /// The applied settings as the executor reads them (NUT: the rynas policy file).
    fn policy(&self) -> UpsSettings {
        let p = files::under_root(&self.root, Path::new(NUT_POLICY));
        std::fs::read_to_string(p)
            .map(|t| parse_policy(&t))
            .unwrap_or_default()
    }
}

pub type UpsState = Option<Arc<Ups>>;

fn ups(st: &UpsState) -> Res<Arc<Ups>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            rynas_api_types::error_codes::BACKEND_UNAVAILABLE,
            "configuration and tasks are not enabled in this rynasd".into(),
        )
    })
}

fn need_backend(u: &Ups) -> Res<()> {
    if u.backend() == UpsBackend::NotInstalled {
        Err(Error::not_found(
            "UPS settings and battery tests need NUT (Debian package nut)",
        ))
    } else {
        Ok(())
    }
}

pub fn router(st: UpsState) -> Router {
    Router::new()
        .route("/v1/ups/settings", get(settings_get).put(settings_put))
        .route("/v1/ups/test", post(test))
        .route("/v1/ups/install", post(install))
        .merge(
            Router::new()
                .route("/v1/ups/safe-mode", post(safe_mode))
                .route_layer(axum::middleware::from_fn(crate::server::require_root_peer)),
        )
        .with_state(st.clone())
        .merge(crate::ups_setup::router(st.clone()))
        .merge(crate::ups_netserver::router(st))
}

fn state_of(u: &Ups, owner: &str) -> Res<UpsSettingsState> {
    let backend = u.backend();
    if backend == UpsBackend::NotInstalled {
        return Ok(UpsSettingsState::default());
    }
    let (eff, cur, _): (UpsSettings, UpsSettings, Option<UpsSettings>) =
        docs(&u.ops.config, owner, UPS_KEY)?;
    Ok(UpsSettingsState {
        available: true,
        backend,
        pending: (eff != cur).then_some(eff),
        applied: cur,
    })
}

async fn blocking<T: Send + 'static>(
    u: &Arc<Ups>,
    f: impl FnOnce(&Ups) -> Res<T> + Send + 'static,
) -> Res<T> {
    let u = u.clone();
    tokio::task::spawn_blocking(move || f(&u))
        .await
        .map_err(Error::internal)?
}

async fn settings_get(State(st): State<UpsState>, hd: HeaderMap) -> Res<Json<UpsSettingsState>> {
    let (u, owner) = (ups(&st)?, user(&hd)?);
    Ok(Json(blocking(&u, move |u| state_of(u, &owner)).await?))
}

async fn settings_put(
    State(st): State<UpsState>,
    hd: HeaderMap,
    body: Result<Json<UpsSettings>, JsonRejection>,
) -> Res<Json<UpsSettingsState>> {
    let (u, owner) = (ups(&st)?, user(&hd)?);
    need_backend(&u)?;
    let Json(req) = body?;
    Ok(Json(
        blocking(&u, move |u| {
            stage(&u.ops.config, &owner, UPS_KEY, &req)?;
            state_of(u, &owner)
        })
        .await?,
    ))
}

/// Outcome of a battery test result: `None` while it runs.
pub fn verdict(r: &str) -> Option<Result<(), String>> {
    let l = r.to_ascii_lowercase();
    if l.contains("in progress") || l.contains("scheduled") {
        None
    } else if l.contains("error") || l.contains("fail") || l.contains("abort") {
        Some(Err(format!("battery test: {r}")))
    } else {
        Some(Ok(()))
    }
}

/// The current `ups.test.result` (empty when the UPS does not report one).
async fn current_result(u: &Ups) -> Result<String, String> {
    if let Some(l) = &u.layer {
        let c = l.result_cmd();
        let o = c
            .run(None, None)
            .await
            .map_err(|e| format!("{}: {e}", c.display()))?;
        Ok(l.parse_result(&o.stdout))
    } else {
        let s = rynas_nut::read(&u.upsd).await;
        if s.link != UpsLink::Connected {
            return Err("upsd does not answer".into());
        }
        Ok(s.test_result.unwrap_or_default())
    }
}

/// Asks upsd to reload `upsd.users` (Debian's `nut-server.service` runs `upsd -c reload`).
async fn reload_upsd(u: &Ups, ctx: &TaskCtx) -> Result<(), String> {
    if u.dry {
        ctx.log("dry system: would reload upsd (systemctl reload nut-server.service)");
        return Ok(());
    }
    let unit = Cmd::new("systemctl")
        .args(["reload", "nut-server.service"])
        .env_clear()
        .env("PATH", PATH)
        .timeout(CMD_TIMEOUT);
    if ctx.run(&unit).await.is_ok_and(|o| o.success()) {
        ctx.log("upsd reloaded");
    } else {
        let direct = Cmd::new("upsd")
            .args(["-c", "reload"])
            .env_clear()
            .env("PATH", PATH)
            .timeout(CMD_TIMEOUT);
        match ctx.run(&direct).await {
            Ok(o) if o.success() => ctx.log("upsd reloaded (upsd -c reload)"),
            Ok(o) => return Err(format!("reloading upsd: {}", o.stderr.trim())),
            Err(e) => return Err(format!("reloading upsd: {e}")),
        }
    }
    // upsd re-reads its files right after the signal
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

/// One instant command as the rynas upsd user.
async fn nut_instcmd(u: &Ups, ups: &str, cmd: &str) -> Result<(), String> {
    let root = u.root.clone();
    let pw = tokio::task::spawn_blocking(move || nut_password(&root))
        .await
        .map_err(|e| e.to_string())??;
    let talk = async {
        let mut c = rynas_nut::Conn::connect(&u.upsd).await?;
        c.login(NUT_USER, &pw).await?;
        c.instcmd(ups, cmd).await?;
        c.logout().await;
        std::io::Result::Ok(())
    };
    match tokio::time::timeout(Duration::from_secs(10), talk).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("upsd refused {cmd}: {e}")),
        Err(_) => Err(format!("upsd did not answer {cmd} in time")),
    }
}

/// Starts (or stops) a test on the NUT backend: checks the UPS offers it, makes sure the rynas
/// upsd user exists, sends the instant command.
async fn nut_send(u: &Ups, ctx: &TaskCtx, kind: BatteryTest) -> Result<(), String> {
    let s = rynas_nut::read(&u.upsd).await;
    let Some(name) = s.name.clone().filter(|_| s.link == UpsLink::Connected) else {
        return Err("no UPS answers on upsd".into());
    };
    if !s.supports(kind) {
        return Err(format!("the UPS does not offer {}", kind.nut_cmd()));
    }
    let (root, ops) = (u.root.clone(), u.ops.clone());
    // under the apply lock: an apply rewrites upsd.users too (ups.device, ups.netserver)
    let changed = tokio::task::spawn_blocking(move || {
        ops.config
            .exclusive(|| nut_password(&root).and_then(|pw| ensure_nut_user(&root, &pw)))
    })
    .await
    .map_err(|e| e.to_string())??;
    if changed {
        ctx.log("added the rynas user to /etc/nut/upsd.users (battery test commands only)");
        reload_upsd(u, ctx).await?;
    }
    nut_instcmd(u, &name, kind.nut_cmd()).await?;
    ctx.log(&format!("sent {} to {name}", kind.nut_cmd()));
    Ok(())
}

async fn send_test(u: &Ups, ctx: &TaskCtx, kind: BatteryTest) -> Result<(), String> {
    let Some(l) = &u.layer else {
        return nut_send(u, ctx, kind).await;
    };
    let c = l.test_cmd(kind);
    let out = ctx
        .run(&c)
        .await
        .map_err(|e| format!("{}: {e}", c.display()))?;
    if !out.success() {
        return Err(format!(
            "{}: {}",
            c.display(),
            out.stderr.trim().lines().last().unwrap_or("failed")
        ));
    }
    Ok(())
}

async fn run_test(u: Arc<Ups>, ctx: TaskCtx, kind: BatteryTest) -> Result<(), String> {
    if u.dry && (u.layer.is_some() || !u.talk_to_upsd) {
        let how = match &u.layer {
            Some(l) => l.test_cmd(kind).display(),
            None => format!("INSTCMD {}", kind.nut_cmd()),
        };
        ctx.log(&format!("dry system: would run {how}"));
        return Ok(());
    }
    let before = current_result(&u).await?;
    ctx.log(&format!("last result: {before}"));
    send_test(&u, &ctx, kind).await?;
    if kind == BatteryTest::Stop {
        ctx.log("battery test stopped");
        return Ok(());
    }
    let max = if kind == BatteryTest::Deep {
        DEEP_MAX
    } else {
        QUICK_MAX
    };
    let start = tokio::time::Instant::now();
    let mut seen_running = false;
    let mut last = before.clone();
    while start.elapsed() < max {
        tokio::select! {
            _ = tokio::time::sleep(RESULT_EVERY) => {}
            _ = ctx.token().cancelled() => {
                ctx.log("cancelled: stopping the battery test");
                let _ = send_test(&u, &ctx, BatteryTest::Stop).await;
                return Err("cancelled".into());
            }
        }
        let r = match current_result(&u).await {
            Ok(r) => r,
            Err(e) => {
                ctx.log(&format!("reading the result: {e}"));
                continue;
            }
        };
        if r != last {
            ctx.log(&format!("result: {r}"));
            last = r.clone();
        }
        match verdict(&r) {
            None => seen_running = true,
            Some(v) if seen_running || r != before => return v,
            Some(_) => {}
        }
        ctx.progress((start.elapsed().as_secs() * 100 / max.as_secs()).min(99) as u8);
    }
    ctx.log(&format!(
        "no new result after {} min: {last}",
        max.as_secs() / 60
    ));
    Ok(())
}

async fn test(
    State(st): State<UpsState>,
    hd: HeaderMap,
    body: Result<Json<BatteryTestRequest>, JsonRejection>,
) -> Res<Response> {
    let (u, who) = (ups(&st)?, user(&hd)?);
    need_backend(&u)?;
    let Json(req) = body?;
    let u2 = u.clone();
    let id = u
        .ops
        .tasks
        .spawn("ups.test", &who, move |ctx| run_test(u2, ctx, req.test))?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

// ---------------------------------------------------------------------------------------------
// installing NUT
// ---------------------------------------------------------------------------------------------

/// The Debian package that brings upsd, upsmon, upsc and the drivers (metapackage of
/// `nut-server` and `nut-client`).
pub const NUT_PACKAGE: &str = "nut";

/// Why `who` may not install NUT now (`Ok`: they may): administrators only, and only on a
/// plain Debian without NUT (a module's UPS layer brings its own).
pub fn install_refusal(
    admins: &crate::roles::AdminPolicy,
    who: &str,
    has_layer: bool,
    backend: UpsBackend,
) -> Res<()> {
    admins.require_admin(who, "NUT install")?;
    if has_layer {
        return Err(Error::new(
            StatusCode::CONFLICT,
            rynas_api_types::error_codes::CONFLICT,
            "the UPS of this machine is run by its hardware module".into(),
        ));
    }
    if backend != UpsBackend::NotInstalled {
        return Err(Error::new(
            StatusCode::CONFLICT,
            rynas_api_types::error_codes::ALREADY_EXISTS,
            "NUT is already installed".into(),
        ));
    }
    Ok(())
}

/// `--dry-system`: the configuration files Debian's packages bring (`MODE=none`, nothing
/// defined), written below the dry root when missing, so the pages see NUT installed.
pub fn dry_install(root: &Path) -> std::io::Result<()> {
    let defaults = [
        (NUT_CONF, "MODE=none\n"),
        ("/etc/nut/ups.conf", "maxretry = 3\n"),
        ("/etc/nut/upsd.conf", ""),
        (NUT_USERS, ""),
        ("/etc/nut/upsmon.conf", "SHUTDOWNCMD \"/sbin/shutdown -h +0\"\n"),
    ];
    for (p, text) in defaults {
        let p = files::under_root(root, Path::new(p));
        if !p.exists() {
            std::fs::create_dir_all(p.parent().expect("has a parent"))?;
            std::fs::write(&p, text)?;
        }
    }
    Ok(())
}

/// `POST /v1/ups/install`: installs NUT from the Debian archive (task `ups.install`). Nothing
/// is started or enabled here: Debian leaves `MODE=none`, and "UPS setup" sets the mode and
/// starts the units when the UPS is set up.
async fn install(State(st): State<UpsState>, hd: HeaderMap) -> Res<Response> {
    let (u, who) = (ups(&st)?, user(&hd)?);
    install_refusal(&u.ops.admins, &who, u.layer.is_some(), u.backend())?;
    tracing::info!(target: "rynas::audit", "ups install {NUT_PACKAGE} by {who}");
    let u2 = u.clone();
    let id = u.ops.tasks.spawn("ups.install", &who, move |ctx| async move {
        if u2.dry {
            ctx.log(&format!("dry system, not run: apt-get install {NUT_PACKAGE}"));
            tokio::time::sleep(Duration::from_millis(600)).await;
            dry_install(&u2.root).map_err(|e| format!("dry root: {e}"))?;
        } else {
            ctx.log(&format!("installing {NUT_PACKAGE} from the Debian archive"));
            let cmd = crate::updates::apt(&["install", "--", NUT_PACKAGE], Duration::from_secs(1800));
            ctx.run(&cmd).await.map_err(|e| e.to_string())?;
            if crate::ups_setup::installed_drivers(&u2.root).is_empty() {
                return Err("no UPS driver found after the install".into());
            }
        }
        // every page reads the backend from the files; the version is read again too
        *u2.setup.version.lock().unwrap() = None;
        if u2.backend() == UpsBackend::NotInstalled {
            return Err(format!("{NUT_CONF} is still missing after the install"));
        }
        ctx.log("NUT installed: set up the UPS with UPS setup");
        Ok(())
    })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

fn valid_reason(r: &str) -> bool {
    SAFE_MODE_REASONS.contains(&r)
        || (!r.is_empty() && r.len() <= 32 && r.bytes().all(|b| b.is_ascii_lowercase()))
}

async fn safe_mode(
    State(st): State<UpsState>,
    body: Result<Json<SafeModeRequest>, JsonRejection>,
) -> Res<Json<SafeModeReport>> {
    let u = ups(&st)?;
    let Json(req) = body?;
    if !valid_reason(&req.reason) {
        return Err(Error::invalid("reason", "unknown shutdown reason"));
    }
    Ok(Json(stop_work(&u, &req.reason, SAFE_MODE_BUDGET).await))
}

/// Before the module layer's shutdown or rynasd's own (NUT): records the reason as a critical
/// notification, cancels running rynas tasks and stops running scheduler units, within
/// `budget`.
pub async fn stop_work(u: &Arc<Ups>, reason: &str, budget: Duration) -> SafeModeReport {
    tracing::warn!("UPS {reason}: stopping rynas tasks");
    let (title, body) = match &u.layer {
        Some(l) => l.stop_notice(reason),
        None => (
            format!("UPS shutdown: {reason}"),
            "The system shuts down on UPS power: rynas stops its running and scheduled tasks \
             first."
                .into(),
        ),
    };
    let mut ev = oneshot(
        NotifyCategory::Ups,
        NotifySeverity::Critical,
        "ups",
        format!("ups:safemode:{reason}"),
        title,
    );
    ev.body = body;
    u.notifier.emit(ev);
    tokio::time::timeout(budget, stop_everything(u, budget))
        .await
        .unwrap_or_else(|_| {
            tracing::warn!("UPS {reason}: stopping took longer than {budget:?}");
            SafeModeReport::default()
        })
}

async fn stop_everything(u: &Arc<Ups>, budget: Duration) -> SafeModeReport {
    let mut report = SafeModeReport::default();
    // rynas tasks (configuration applies finish by themselves: a half-done apply rolls back
    // on cancel, which is worse than letting it end)
    for t in u.ops.tasks.list(500) {
        if t.status == TaskStatus::Running
            && !t.kind.starts_with("config.")
            && u.ops.tasks.cancel(&t.id).is_ok()
        {
            report.cancelled_tasks.push(t.id.clone());
        }
    }
    // scheduler task units: a clean stop (their own stop timeout), all together
    let unit_timeout = budget
        .saturating_sub(Duration::from_secs(5))
        .max(Duration::from_secs(5));
    if let Some(p) = &u.protection {
        p.invalidate();
        let running = p.runtime().await.running.clone();
        let stops = running.iter().map(|unit| async move {
            if u.dry {
                tracing::info!("dry system: would stop {unit}");
                return Some(unit.clone());
            }
            let c = Cmd::new("systemctl")
                .args(["stop", unit.as_str()])
                .env_clear()
                .timeout(unit_timeout);
            match c.run(None, None).await {
                Ok(o) if o.success() => Some(unit.clone()),
                Ok(o) => {
                    tracing::warn!("UPS stop: stopping {unit}: {}", o.stderr.trim());
                    None
                }
                Err(e) => {
                    tracing::warn!("UPS stop: stopping {unit}: {e}");
                    None
                }
            }
        });
        report.stopped_units = futures_util::future::join_all(stops)
            .await
            .into_iter()
            .flatten()
            .collect();
    }
    // the cancelled tasks end (their commands get SIGTERM from the task runner)
    let waits = report.cancelled_tasks.iter().map(|id| u.ops.tasks.wait(id));
    let _ = futures_util::future::join_all(waits).await;
    tracing::warn!(
        "UPS stop: cancelled tasks {:?}, stopped units {:?}",
        report.cancelled_tasks,
        report.stopped_units
    );
    report
}

// ---------------------------------------------------------------------------------------------
// the on-battery watcher (NUT backend)
// ---------------------------------------------------------------------------------------------

/// What the watcher does after a reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchAction {
    OnBattery,
    PowerBack,
    /// Stop rynas's work; with `shutdown`, then ask upsmon for the shutdown.
    Stop {
        reason: &'static str,
        shutdown: bool,
    },
}

/// The watcher's memory between readings.
#[derive(Debug, Default)]
pub struct Watch {
    on_battery_since: Option<tokio::time::Instant>,
    /// The stop ran in this outage (once per outage).
    fired: bool,
}

impl Watch {
    /// The actions after reading `s`; `after`: the configured time on battery.
    pub fn step(
        &mut self,
        s: &UpsStatus,
        after: Option<Duration>,
        now: tokio::time::Instant,
    ) -> Vec<WatchAction> {
        let mut out = Vec::new();
        // an unanswered reading changes nothing (upsd restarting, USB hiccup)
        if s.link != UpsLink::Connected {
            return out;
        }
        let ob = s.on_battery();
        if ob && self.on_battery_since.is_none() {
            self.on_battery_since = Some(now);
            out.push(WatchAction::OnBattery);
        }
        if !ob && !s.critical() && self.on_battery_since.take().is_some() {
            self.fired = false;
            out.push(WatchAction::PowerBack);
        }
        if s.critical() && !self.fired {
            self.fired = true;
            let reason = if s.has("FSD") { "fsd" } else { "lowbatt" };
            out.push(WatchAction::Stop {
                reason,
                shutdown: false,
            });
        } else if let (Some(since), Some(after), false) = (self.on_battery_since, after, self.fired)
            && now.duration_since(since) >= after
        {
            self.fired = true;
            out.push(WatchAction::Stop {
                reason: "waittimeup",
                shutdown: true,
            });
        }
        out
    }
}

fn notify(u: &Ups, sev: NotifySeverity, key: &str, title: String, body: String) {
    let mut ev = oneshot(NotifyCategory::Ups, sev, "ups", key.to_string(), title);
    ev.body = body;
    u.notifier.emit(ev);
}

/// The shutdown after the time on battery: upsmon's forced shutdown (it sets FSD, lets its
/// secondaries go, runs `SHUTDOWNCMD` and powers the UPS off as configured), else poweroff.
async fn shutdown(u: &Ups) {
    if u.dry {
        tracing::warn!("dry system: would run upsmon -c fsd");
        return;
    }
    let fsd = Cmd::new("upsmon")
        .args(["-c", "fsd"])
        .env_clear()
        .env("PATH", PATH)
        .timeout(CMD_TIMEOUT);
    let why = match fsd.run(None, None).await {
        Ok(o) if o.success() => {
            tracing::warn!("UPS: upsmon starts the shutdown");
            return;
        }
        Ok(o) => o.stderr.trim().to_string(),
        Err(e) => e.to_string(),
    };
    tracing::error!("UPS: upsmon -c fsd failed ({why}): systemctl poweroff");
    notify(
        u,
        NotifySeverity::Error,
        "ups:upsmon",
        "UPS: upsmon did not take the shutdown request".into(),
        format!("upsmon -c fsd: {why}. rynas powers the system off itself."),
    );
    let off = Cmd::new("systemctl")
        .arg("poweroff")
        .env_clear()
        .env("PATH", PATH)
        .timeout(CMD_TIMEOUT);
    if let Err(e) = off.run(None, None).await {
        tracing::error!("UPS: systemctl poweroff: {e}");
    }
}

/// Follows the UPS on the NUT backend (see the module documentation). Runs for the life of
/// rynasd; one upsd conversation every 5 s (30 s while no UPS answers).
pub async fn watch(u: Arc<Ups>) {
    let mut w = Watch::default();
    loop {
        let s = rynas_nut::read(&u.upsd).await;
        let policy = if s.on_battery() || s.critical() {
            let u2 = u.clone();
            tokio::task::spawn_blocking(move || u2.policy())
                .await
                .unwrap_or_default()
        } else {
            UpsSettings::default()
        };
        let after = policy
            .safemode_after_s
            .map(|a| Duration::from_secs(a.into()));
        for a in w.step(&s, after, tokio::time::Instant::now()) {
            match a {
                WatchAction::OnBattery => notify(
                    &u,
                    NotifySeverity::Warning,
                    "ups:onbatt",
                    "UPS on battery".into(),
                    match policy.safemode_after_s {
                        Some(a) => format!(
                            "Mains power is gone. The system shuts down after {} min on \
                             battery, or earlier on low battery.",
                            a / 60
                        ),
                        None => "Mains power is gone. The system shuts down on low battery.".into(),
                    },
                ),
                WatchAction::PowerBack => notify(
                    &u,
                    NotifySeverity::Info,
                    "ups:online",
                    "UPS back on mains power".into(),
                    "Mains power is back.".into(),
                ),
                WatchAction::Stop {
                    reason,
                    shutdown: down,
                } => {
                    let budget = Duration::from_secs(policy.pre_safemode_timeout_s.into());
                    stop_work(&u, reason, budget).await;
                    if down {
                        shutdown(&u).await;
                    }
                }
            }
        }
        let period = if s.link == UpsLink::Connected {
            WATCH_EVERY
        } else {
            WATCH_IDLE
        };
        tokio::time::sleep(period).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_for_admins_on_a_debian_without_nut() {
        let admins = crate::roles::AdminPolicy::new(vec!["rynas-admin".into()], |u| match u {
            "admin" => Some(vec!["rynas-admin".into()]),
            "bob" => Some(vec!["users".into()]),
            _ => None,
        });
        assert!(install_refusal(&admins, "admin", false, UpsBackend::NotInstalled).is_ok());
        let e = install_refusal(&admins, "bob", false, UpsBackend::NotInstalled).unwrap_err();
        assert_eq!((e.0, e.1), (StatusCode::FORBIDDEN, rynas_api_types::error_codes::FORBIDDEN));
        let e = install_refusal(&admins, "admin", false, UpsBackend::Nut).unwrap_err();
        assert_eq!((e.0, e.1), (StatusCode::CONFLICT, rynas_api_types::error_codes::ALREADY_EXISTS));
        let e = install_refusal(&admins, "admin", true, UpsBackend::Module).unwrap_err();
        assert_eq!((e.0, e.1), (StatusCode::CONFLICT, rynas_api_types::error_codes::CONFLICT));
        // the role is checked first: a non-admin learns nothing about the machine
        let e = install_refusal(&admins, "bob", false, UpsBackend::Nut).unwrap_err();
        assert_eq!(e.0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn dry_install_flips_the_backend_and_keeps_existing_files() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(backend_of(root.path(), None), UpsBackend::NotInstalled);
        let users = root.path().join("etc/nut/upsd.users");
        std::fs::create_dir_all(users.parent().unwrap()).unwrap();
        std::fs::write(&users, "[admin]\n").unwrap();
        dry_install(root.path()).unwrap();
        assert_eq!(backend_of(root.path(), None), UpsBackend::Nut);
        assert_eq!(std::fs::read_to_string(&users).unwrap(), "[admin]\n");
        assert_eq!(std::fs::read_to_string(root.path().join("etc/nut/nut.conf")).unwrap(), "MODE=none\n");
    }

    #[test]
    fn policy_round_trip() {
        let want = UpsSettings {
            safemode_after_s: Some(900),
            pre_safemode_timeout_s: 60,
        };
        let text = render_policy(&want);
        assert_eq!(parse_policy(&text), want);
        assert_eq!(parse_policy("SHUTDOWN_AFTER=0\n").safemode_after_s, None);
    }

    #[test]
    fn ranges() {
        let ok = UpsSettings {
            safemode_after_s: Some(300),
            pre_safemode_timeout_s: 60,
        };
        assert!(normalize(ok).is_ok());
        let e = normalize(UpsSettings {
            safemode_after_s: Some(90),
            ..ok
        })
        .unwrap_err();
        assert_eq!(e.field.as_deref(), Some("safemode_after_s"));
        let e = normalize(UpsSettings {
            pre_safemode_timeout_s: 5,
            ..ok
        })
        .unwrap_err();
        assert_eq!(e.field.as_deref(), Some("pre_safemode_timeout_s"));
    }

    #[test]
    fn results() {
        assert_eq!(verdict("In progress"), None);
        assert_eq!(verdict("Done and passed"), Some(Ok(())));
        assert!(verdict("Done and error").unwrap().is_err());
        assert!(verdict("Aborted").unwrap().is_err());
        assert!(valid_reason("lowbatt") && !valid_reason("rm -rf") && !valid_reason(""));
    }

    #[test]
    fn backends() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(backend_of(d.path(), None), UpsBackend::NotInstalled);
        std::fs::create_dir_all(d.path().join("etc/nut")).unwrap();
        std::fs::write(d.path().join("etc/nut/nut.conf"), "MODE=standalone\n").unwrap();
        assert_eq!(backend_of(d.path(), None), UpsBackend::Nut);
    }

    #[test]
    fn upsd_user_block() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let users = d.path().join("etc/nut/upsd.users");
        // NUT not installed
        assert!(ensure_nut_user(d.path(), "x").is_err());
        std::fs::create_dir_all(users.parent().unwrap()).unwrap();
        let mine = "# my users\n[upsmon]\n\tpassword = secret\n\tupsmon primary\n";
        std::fs::write(&users, mine).unwrap();
        std::fs::set_permissions(&users, std::fs::Permissions::from_mode(0o640)).unwrap();
        let pw = nut_password(d.path()).unwrap();
        assert_eq!(pw.len(), 32);
        assert!(rynas_nut::valid_name(&pw));
        // generated once, root-only
        assert_eq!(nut_password(d.path()).unwrap(), pw);
        let secret = d.path().join("etc/rynas/nut-upsd.secret");
        assert_eq!(std::fs::metadata(&secret).unwrap().mode() & 0o777, 0o600);
        assert!(ensure_nut_user(d.path(), &pw).unwrap());
        let text = std::fs::read_to_string(&users).unwrap();
        assert!(text.starts_with(mine), "{text}");
        assert!(text.contains("[rynas]\n"));
        assert!(text.contains(&format!("password = \"{pw}\"")));
        assert_eq!(text.matches("\tinstcmds = test.battery.").count(), 3);
        assert!(!text.contains("actions") && !text.contains("upsmon master"));
        assert_eq!(std::fs::metadata(&users).unwrap().mode() & 0o777, 0o640);
        // unchanged: nothing written
        assert!(!ensure_nut_user(d.path(), &pw).unwrap());
        // drift: an edited block is put back
        std::fs::write(&users, text.replace("test.battery.stop", "ALL")).unwrap();
        assert!(ensure_nut_user(d.path(), &pw).unwrap());
        assert_eq!(std::fs::read_to_string(&users).unwrap(), text);
        // a [rynas] user of the admin's own is not overwritten
        std::fs::write(&users, format!("{mine}[rynas]\n\tpassword = theirs\n")).unwrap();
        let e = ensure_nut_user(d.path(), &pw).unwrap_err();
        assert!(e.contains("does not manage") && !e.contains(&pw), "{e}");
    }

    fn reading(flags: &[&str]) -> UpsStatus {
        UpsStatus {
            link: UpsLink::Connected,
            flags: flags.iter().map(|f| f.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn watcher() {
        let t0 = tokio::time::Instant::now();
        let min = Duration::from_secs(60);
        let after = Some(5 * min);
        let mut w = Watch::default();
        assert!(w.step(&reading(&["OL", "CHRG"]), after, t0).is_empty());
        assert_eq!(
            w.step(&reading(&["OB", "DISCHRG"]), after, t0),
            [WatchAction::OnBattery]
        );
        assert!(w.step(&reading(&["OB"]), after, t0 + 4 * min).is_empty());
        // a lost reading does not end the outage
        assert!(
            w.step(&UpsStatus::default(), after, t0 + 4 * min)
                .is_empty()
        );
        assert_eq!(
            w.step(&reading(&["OB"]), after, t0 + 5 * min),
            [WatchAction::Stop {
                reason: "waittimeup",
                shutdown: true
            }]
        );
        // once per outage
        assert!(
            w.step(&reading(&["OB", "LB"]), after, t0 + 6 * min)
                .is_empty()
        );
        assert_eq!(
            w.step(&reading(&["OL"]), after, t0 + 7 * min),
            [WatchAction::PowerBack]
        );
        // without the timer: only low battery stops the work, and upsmon shuts down
        let mut w = Watch::default();
        assert_eq!(
            w.step(&reading(&["OB"]), None, t0),
            [WatchAction::OnBattery]
        );
        assert!(w.step(&reading(&["OB"]), None, t0 + 600 * min).is_empty());
        assert_eq!(
            w.step(&reading(&["OB", "LB"]), None, t0 + 601 * min),
            [WatchAction::Stop {
                reason: "lowbatt",
                shutdown: false
            }]
        );
        // a forced shutdown asked elsewhere
        let mut w = Watch::default();
        assert_eq!(
            w.step(&reading(&["OL", "FSD"]), None, t0),
            [WatchAction::Stop {
                reason: "fsd",
                shutdown: false
            }]
        );
    }
}
