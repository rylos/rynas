//! System updates (`/v1/updates/...`, public under `/api/v1/updates/...`).
//!
//! - The list comes from `apt-get -s full-upgrade` and `apt-get -s autoremove` (simulations,
//!   no lock), cached until `/var/lib/dpkg/status`, `/var/cache/apt/pkgcache.bin` or the
//!   lists change, or an `updates.*` task ends; one refresh at a time (S8). Nothing heavy
//!   runs on page load once the cache is warm.
//! - Check, install, autoremove and changelog are tasks (`updates.check`, `updates.install`,
//!   `updates.autoremove`, `updates.changelog`), one at a time. Write runs of apt go
//!   through `systemd-run` ([`apt`]: rynasd's sandbox makes `/usr` and `/boot` read-only).
//!   apt runs non-interactively
//!   (`DEBIAN_FRONTEND=noninteractive`, `--force-confdef --force-confold`: local conffiles
//!   are kept), needrestart only lists (`NEEDRESTART_MODE=l`). Removals are confirmed by name,
//!   checked at once against the cached simulation and again by the task against a fresh one.
//!   autoremove never runs by itself.
//! - The kernel card reads `uname -r`, `/boot/vmlinuz-*` and, where a hardware module has a
//!   boot layer (`rynas_module::BootLayer`), its boot device (may mount it): cached 6 hours,
//!   a manual refresh at most once a minute.
//! - A daily check emits `updates:available` (warning with security updates, notice
//!   otherwise; resolved when nothing is left). Package lists are refreshed by apt's own
//!   `apt-daily.timer` (on the root SSD); the check runs `apt-get update` only when they are
//!   older than 36 hours.
//!
//! `--dry-system`: apt is never run (nor the boot layer's tool); the simulations come from
//! fixtures and the tasks only log what they would run.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::notify::{EventAction, NotifyCategory, NotifySeverity};
use rynas_api_types::updates::{
    AutoUpdates, AutoUpdatesState, AutoremoveRequest, InstallRequest, KernelStatus,
    UpdatesStatus,
};
use rynas_api_types::{TaskStarted, TaskStatus, error_codes};
use rynas_notify::Notifier;
use rynas_notify::event::{field, firing, resolved};
use rynas_tasks::{Cmd, TaskCtx};
use rynas_updates::apt::{parse_reboot_pkgs, parse_simulation, valid_package_name};
use rynas_updates::domain::unattended_installed;
use rynas_updates::{AUTO_KEY, fixtures, kernels};

use crate::ops::{Error, Ops, Res, user};
use crate::services::{docs, stage};

const APT_GET: &str = "/usr/bin/apt-get";
const SIM_TIMEOUT: Duration = Duration::from_secs(90);
const KERNEL_TTL: Duration = Duration::from_secs(6 * 3600);
const KERNEL_MIN_REFRESH: Duration = Duration::from_secs(60);
const DAILY: Duration = Duration::from_secs(24 * 3600);
const FIRST_CHECK: Duration = Duration::from_secs(10 * 60);
const LISTS_MAX_AGE: Duration = Duration::from_secs(36 * 3600);
const NOTIFY_KEY: &str = "updates:available";

/// What decides whether the cached simulation is still good.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Stamp {
    files: Vec<Option<SystemTime>>,
    generation: u64,
}

pub struct Updates {
    pub ops: Arc<Ops>,
    pub notifier: Arc<dyn Notifier>,
    pub dry: bool,
    /// Where rynas writes configuration (`/`, the dry root): unattended-upgrades' presence.
    pub config_root: PathBuf,
    /// Bumped when an `updates.*` task ends (the cache is then stale).
    generation: AtomicU64,
    cache: std::sync::Mutex<Option<(Stamp, UpdatesStatus)>>,
    refresh: tokio::sync::Mutex<()>,
    kernel: std::sync::Mutex<Option<(Instant, KernelStatus)>>,
    kernel_lock: tokio::sync::Mutex<()>,
    /// A hardware module's boot layer.
    boot: Option<Arc<dyn rynas_module::BootLayer>>,
}

pub type UpdState = Option<Arc<Updates>>;

fn upd(st: &UpdState) -> Res<Arc<Updates>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "updates are not enabled in this rynasd".into(),
        )
    })
}

fn ms(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    ms(SystemTime::now())
}

async fn mtime(p: &str) -> Option<SystemTime> {
    tokio::fs::metadata(p).await.ok()?.modified().ok()
}

/// apt-get with the options of every real run, in a transient unit outside rynasd's sandbox.
///
/// rynasd runs with `ProtectSystem=yes` (`/usr`, `/boot` read-only), `MemoryMax=256M` and
/// `NoNewPrivileges`: dpkg could not write packages, `update-initramfs` could not write
/// `/boot`, and a big upgrade could hit the memory limit. So the write runs of apt go through
/// `systemd-run --wait --pipe --collect` (a unit `rynas-apt-<n>` with the default service
/// environment); its output streams into the task log as usual. Simulations stay in rynasd
/// (read-only). Cancelling the task stops following the unit, not dpkg: an interrupted dpkg
/// is worse than a finished upgrade, and apt's own lock keeps a second run out meanwhile.
fn apt(args: &[&str], timeout: Duration) -> Cmd {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let unit = format!(
        "--unit=rynas-apt-{}-{}",
        now_ms() / 1000,
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    Cmd::new("systemd-run")
        .args(["--wait", "--pipe", "--collect", "--quiet", "--service-type=exec"])
        .arg(unit)
        .args([
            "--description=rynas: apt-get (System > Updates)",
            "--setenv=DEBIAN_FRONTEND=noninteractive",
            "--setenv=NEEDRESTART_MODE=l",
            "--setenv=APT_LISTCHANGES_FRONTEND=none",
            "--setenv=LC_ALL=C",
            "--property=Nice=5",
            "--",
            APT_GET,
            "-q",
            "-y",
            "-o",
            "Dpkg::Options::=--force-confdef",
            "-o",
            "Dpkg::Options::=--force-confold",
        ])
        .args(args)
        .env_clear()
        .timeout(timeout)
}

/// A simulation (no lock, so it also works while dpkg runs).
fn apt_sim(args: &[&str]) -> Cmd {
    Cmd::new(APT_GET)
        .args(["-s", "-q", "-o", "Debug::NoLocking=true"])
        .args(args)
        .env_clear()
        .timeout(SIM_TIMEOUT)
}

impl Updates {
    pub fn new(ops: Arc<Ops>, notifier: Arc<dyn Notifier>, dry: bool, config_root: PathBuf) -> Self {
        Self {
            ops,
            notifier,
            dry,
            config_root,
            generation: AtomicU64::new(0),
            cache: std::sync::Mutex::new(None),
            refresh: tokio::sync::Mutex::new(()),
            kernel: std::sync::Mutex::new(None),
            kernel_lock: tokio::sync::Mutex::new(()),
            boot: None,
        }
    }

    /// The boot layer of a hardware module (its boot device on the kernel card).
    pub fn with_boot(mut self, boot: Option<Arc<dyn rynas_module::BootLayer>>) -> Self {
        self.boot = boot;
        self
    }

    fn apt_present(&self) -> bool {
        self.dry || Path::new(APT_GET).exists()
    }

    async fn stamp(&self) -> Stamp {
        let mut files = Vec::new();
        if !self.dry {
            for p in [
                "/var/lib/dpkg/status",
                "/var/cache/apt/pkgcache.bin",
                "/var/lib/apt/lists",
            ] {
                files.push(mtime(p).await);
            }
        }
        Stamp {
            files,
            generation: self.generation.load(Ordering::Relaxed),
        }
    }

    /// Runs a simulation (or answers the fixture on a dry system).
    async fn simulate(&self, args: &[&str], fixture: &'static str) -> Result<String, String> {
        if self.dry {
            return Ok(fixture.to_string());
        }
        let o = apt_sim(args)
            .run(None, None)
            .await
            .map_err(|e| format!("apt-get -s {}: {e}", args.join(" ")))?;
        if !o.success() {
            let last = o.stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
            return Err(format!("apt-get -s {}: exit code {:?}: {last}", args.join(" "), o.code));
        }
        Ok(o.stdout)
    }

    async fn compute(&self) -> UpdatesStatus {
        let mut s = UpdatesStatus {
            apt: self.apt_present(),
            read_at_ms: now_ms(),
            ..Default::default()
        };
        if !s.apt {
            return s;
        }
        s.lists_updated_ms = if self.dry {
            Some(now_ms().saturating_sub(3 * 3600 * 1000))
        } else {
            let a = mtime("/var/lib/apt/lists").await;
            let b = mtime("/var/cache/apt/pkgcache.bin").await;
            a.max(b).map(ms)
        };
        match self.simulate(&["full-upgrade"], fixtures::FULL_UPGRADE).await {
            Ok(text) => {
                let sim = parse_simulation(&text);
                s.packages = sim.packages;
                s.removals = sim.removals;
                s.held = sim.held;
            }
            Err(e) => s.error = Some(e),
        }
        match self.simulate(&["autoremove"], fixtures::AUTOREMOVE).await {
            Ok(text) => s.autoremovable = parse_simulation(&text).removals,
            Err(e) => {
                s.error.get_or_insert(e);
            }
        }
        s
    }

    fn running_task(&self) -> Option<String> {
        self.ops
            .tasks
            .list(100)
            .into_iter()
            .find(|t| {
                t.kind.starts_with("updates.")
                    && matches!(t.status, TaskStatus::Running | TaskStatus::Queued)
            })
            .map(|t| t.id)
    }

    /// The cached status (recomputed when apt's state changed), with the live fields.
    pub async fn status(&self, force: bool) -> UpdatesStatus {
        let stamp = self.stamp().await;
        let cached = |stamp: &Stamp| {
            self.cache
                .lock()
                .expect("cache lock")
                .as_ref()
                .filter(|(s, _)| s == stamp)
                .map(|(_, v)| v.clone())
        };
        let mut s = match (force, cached(&stamp)) {
            (false, Some(v)) => v,
            _ => {
                let _g = self.refresh.lock().await;
                // another request may have refreshed it meanwhile
                match (force, cached(&stamp)) {
                    (false, Some(v)) => v,
                    _ => {
                        let v = self.compute().await;
                        *self.cache.lock().expect("cache lock") = Some((stamp, v.clone()));
                        v
                    }
                }
            }
        };
        if self.dry {
            s.reboot_required = true;
            s.reboot_packages = vec!["linux-image-amd64".into()];
        } else {
            s.reboot_required = tokio::fs::metadata("/var/run/reboot-required").await.is_ok();
            s.reboot_packages = tokio::fs::read_to_string("/var/run/reboot-required.pkgs")
                .await
                .map(|t| parse_reboot_pkgs(&t))
                .unwrap_or_default();
        }
        s.running_task = self.running_task();
        s.unattended_installed = unattended_installed(&self.config_root);
        s
    }

    fn task_done(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// `updates:available` firing or resolved.
    pub fn notify(&self, s: &UpdatesStatus) {
        if !s.apt || s.error.is_some() {
            return;
        }
        let n = s.packages.len();
        if n == 0 {
            self.notifier.emit(resolved(
                NotifyCategory::Update,
                "updates",
                NOTIFY_KEY,
                "The system is up to date",
            ));
            return;
        }
        let sec = s.security_count();
        let (sev, title) = if sec > 0 {
            (
                NotifySeverity::Warning,
                format!("{n} updates available, {sec} for security"),
            )
        } else {
            (NotifySeverity::Notice, format!("{n} updates available"))
        };
        let mut ev = firing(NotifyCategory::Update, sev, "updates", NOTIFY_KEY, title);
        let mut names: Vec<&str> = s.packages.iter().map(|p| p.name.as_str()).take(20).collect();
        if n > 20 {
            names.push("…");
        }
        ev.body = format!("Packages: {}.", names.join(", "));
        ev.actions = vec![EventAction {
            label: "Open updates".into(),
            url: "/system/updates".into(),
        }];
        let ev = field(ev, "updates", n);
        self.notifier.emit(field(ev, "security", sec));
    }

    /// Starts an `updates.*` task, refusing a second one (409 `busy`). When it ends the cache
    /// is refreshed and the notification re-evaluated.
    fn spawn<F, Fut>(self: &Arc<Self>, kind: &str, who: &str, job: F) -> Res<Response>
    where
        F: FnOnce(Arc<Updates>, TaskCtx) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        if let Some(id) = self.running_task() {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::BUSY,
                format!("another update task is running ({id})"),
            ));
        }
        let me = self.clone();
        let notify = kind != "updates.changelog";
        let id = self.ops.tasks.spawn(kind, who, move |ctx| async move {
            let r = job(me.clone(), ctx).await;
            me.task_done();
            if notify {
                let s = me.status(false).await;
                me.notify(&s);
            }
            r
        })?;
        Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
    }

    /// Runs `cmd` in the task, or only logs it on a dry system.
    async fn run(&self, ctx: &TaskCtx, cmd: Cmd) -> Result<String, String> {
        if self.dry {
            ctx.log(&format!("dry system, not run: {}", cmd.display()));
            tokio::time::sleep(Duration::from_millis(600)).await;
            return Ok(String::new());
        }
        ctx.run(&cmd).await.map(|o| o.stdout).map_err(|e| e.to_string())
    }

    // ------------------------------------------------------------------ kernel

    pub async fn kernel(&self, refresh: bool) -> KernelStatus {
        let fresh = |max: Duration| {
            self.kernel
                .lock()
                .expect("kernel lock")
                .as_ref()
                .filter(|(t, _)| t.elapsed() < max)
                .map(|(_, k)| k.clone())
        };
        if let Some(k) = fresh(if refresh { KERNEL_MIN_REFRESH } else { KERNEL_TTL }) {
            return k;
        }
        let _g = self.kernel_lock.lock().await;
        if let Some(k) = fresh(KERNEL_MIN_REFRESH) {
            return k;
        }
        let k = self.read_kernel().await;
        *self.kernel.lock().expect("kernel lock") = Some((Instant::now(), k.clone()));
        k
    }

    async fn read_kernel(&self) -> KernelStatus {
        let mut k = KernelStatus {
            read_at_ms: Some(now_ms()),
            ..Default::default()
        };
        if self.dry {
            k.running = "6.12.48-amd64".into();
            k.installed = vec!["6.12.48-amd64".into(), "6.12.41-amd64".into()];
        } else {
            k.running = tokio::fs::read_to_string("/proc/sys/kernel/osrelease")
                .await
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let mut names = Vec::new();
            if let Ok(mut rd) = tokio::fs::read_dir("/boot").await {
                while let Ok(Some(e)) = rd.next_entry().await {
                    names.push(e.file_name().to_string_lossy().into_owned());
                }
            }
            k.installed = kernels::installed_kernels(names);
        }
        let Some(boot) = self.boot.clone() else {
            return k;
        };
        k.boot_layer = true;
        let read = tokio::task::spawn_blocking(move || boot.status())
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
        match read {
            Ok(d) => {
                // the dry system's kernels are the boot device's
                if self.dry {
                    let rel = |k: &Option<rynas_api_types::updates::BootKernel>| {
                        k.as_ref().and_then(|k| k.release.clone())
                    };
                    if let Some(r) = rel(&d.kernel) {
                        k.running = r;
                        k.installed = [rel(&d.kernel), rel(&d.kernel_old)]
                            .into_iter()
                            .flatten()
                            .collect();
                    }
                }
                k.boot = Some(d)
            }
            Err(e) => {
                tracing::warn!("boot layer: {e}");
                k.error = Some(e);
            }
        }
        k
    }
}

pub fn router(st: UpdState) -> Router {
    Router::new()
        .route("/v1/updates", get(status_get))
        .route("/v1/updates/check", post(check))
        .route("/v1/updates/install", post(install))
        .route("/v1/updates/autoremove", post(autoremove))
        .route("/v1/updates/packages/{name}/changelog", post(changelog))
        .route("/v1/updates/auto", get(auto_get).put(auto_put))
        .route("/v1/updates/kernel", get(kernel_get))
        .with_state(st)
}

async fn status_get(State(st): State<UpdState>) -> Res<Json<UpdatesStatus>> {
    let u = upd(&st)?;
    Ok(Json(u.status(false).await))
}

fn no_apt() -> Error {
    Error::not_found("apt is not installed on this machine")
}

async fn check(State(st): State<UpdState>, hd: HeaderMap) -> Res<Response> {
    let (u, who) = (upd(&st)?, user(&hd)?);
    if !u.apt_present() {
        return Err(no_apt());
    }
    u.spawn("updates.check", &who, |u, ctx| async move {
        u.run(&ctx, apt(&["update"], Duration::from_secs(600))).await?;
        Ok(())
    })
}

fn not_confirmed(missing: &[String]) -> Error {
    Error::new(
        StatusCode::BAD_REQUEST,
        error_codes::CONFIRMATION_REQUIRED,
        format!("the upgrade would remove {}: confirm them", missing.join(", ")),
    )
    .field("confirm_removals")
}

async fn install(
    State(st): State<UpdState>,
    hd: HeaderMap,
    body: Result<Json<InstallRequest>, JsonRejection>,
) -> Res<Response> {
    let (u, who) = (upd(&st)?, user(&hd)?);
    let Json(req) = body?;
    if !u.apt_present() {
        return Err(no_apt());
    }
    if req.packages.len() > 2000 {
        return Err(Error::invalid("packages", "too many packages"));
    }
    if let Some(bad) = req.packages.iter().find(|p| !valid_package_name(p)) {
        return Err(Error::invalid("packages", format!("{bad:?} is not a package name")));
    }
    // checked at once against the cached simulation (a full upgrade), then by the task
    if req.packages.is_empty() {
        let s = u.status(false).await;
        let missing: Vec<String> = s
            .removals
            .iter()
            .filter(|r| !req.confirm_removals.contains(r))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(not_confirmed(&missing));
        }
    }
    u.spawn("updates.install", &who, move |u, ctx| async move {
        let mut args: Vec<&str> = Vec::new();
        if req.packages.is_empty() {
            args.push("full-upgrade");
        } else {
            args.extend(["install", "--only-upgrade", "--"]);
            args.extend(req.packages.iter().map(String::as_str));
        }
        ctx.log("simulating first: packages to remove must have been confirmed");
        let sim = u
            .simulate(&args, fixtures::FULL_UPGRADE)
            .await
            .map(|t| parse_simulation(&t))?;
        let missing: Vec<&String> = sim
            .removals
            .iter()
            .filter(|r| !req.confirm_removals.contains(r))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "the upgrade would now also remove {}: nothing done, check again and confirm",
                missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        ctx.log(&format!(
            "{} packages to upgrade or install, {} to remove",
            sim.packages.len(),
            sim.removals.len()
        ));
        u.run(&ctx, apt(&args, Duration::from_secs(3 * 3600))).await?;
        Ok(())
    })
}

async fn autoremove(
    State(st): State<UpdState>,
    hd: HeaderMap,
    body: Result<Json<AutoremoveRequest>, JsonRejection>,
) -> Res<Response> {
    let (u, who) = (upd(&st)?, user(&hd)?);
    let Json(req) = body?;
    if !u.apt_present() {
        return Err(no_apt());
    }
    let mut want = req.packages.clone();
    want.sort();
    want.dedup();
    if want.is_empty() {
        return Err(Error::invalid("packages", "nothing to remove"));
    }
    let s = u.status(false).await;
    if s.autoremovable != want {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            error_codes::CONFIRMATION_REQUIRED,
            format!("autoremove would remove {}", s.autoremovable.join(", ")),
        )
        .field("packages"));
    }
    u.spawn("updates.autoremove", &who, move |u, ctx| async move {
        let now = u
            .simulate(&["autoremove"], fixtures::AUTOREMOVE)
            .await
            .map(|t| parse_simulation(&t).removals)?;
        if now != want {
            return Err(format!(
                "autoremove would now remove {}: nothing done",
                now.join(", ")
            ));
        }
        ctx.log(&format!("removing {}", want.join(", ")));
        u.run(&ctx, apt(&["autoremove"], Duration::from_secs(1800))).await?;
        Ok(())
    })
}

async fn changelog(
    State(st): State<UpdState>,
    hd: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Res<Response> {
    let (u, who) = (upd(&st)?, user(&hd)?);
    if !valid_package_name(&name) {
        return Err(Error::bad_request("invalid package name"));
    }
    if !u.apt_present() {
        return Err(no_apt());
    }
    u.spawn("updates.changelog", &who, move |u, ctx| async move {
        if u.dry {
            ctx.log(&format!("{name} (1.0-2) trixie; urgency=medium\n\n  * Dry system: no changelog downloaded.\n"));
            return Ok(());
        }
        // network: the Debian changelog server
        u.run(
            &ctx,
            Cmd::new(APT_GET)
                .args(["changelog", "-q", "--", &name])
                .env_clear()
                .timeout(Duration::from_secs(120)),
        )
        .await?;
        Ok(())
    })
}

fn auto_state(u: &Updates, owner: &str) -> Res<AutoUpdatesState> {
    let (eff, cur, _): (AutoUpdates, AutoUpdates, Option<AutoUpdates>) =
        docs(&u.ops.config, owner, AUTO_KEY)?;
    Ok(AutoUpdatesState {
        pending: (eff != cur).then_some(eff),
        applied: cur,
        unattended_installed: unattended_installed(&u.config_root),
    })
}

async fn auto_get(State(st): State<UpdState>, hd: HeaderMap) -> Res<Json<AutoUpdatesState>> {
    let (u, owner) = (upd(&st)?, user(&hd)?);
    let r = tokio::task::spawn_blocking(move || auto_state(&u, &owner))
        .await
        .map_err(Error::internal)??;
    Ok(Json(r))
}

async fn auto_put(
    State(st): State<UpdState>,
    hd: HeaderMap,
    body: Result<Json<AutoUpdates>, JsonRejection>,
) -> Res<Json<AutoUpdatesState>> {
    let (u, owner) = (upd(&st)?, user(&hd)?);
    let Json(req) = body?;
    let r = tokio::task::spawn_blocking(move || {
        stage(&u.ops.config, &owner, AUTO_KEY, &req)?;
        auto_state(&u, &owner)
    })
    .await
    .map_err(Error::internal)??;
    Ok(Json(r))
}

async fn kernel_get(State(st): State<UpdState>, RawQuery(q): RawQuery) -> Res<Json<KernelStatus>> {
    let u = upd(&st)?;
    let refresh = q.is_some_and(|q| q.split('&').any(|kv| kv == "refresh=1"));
    Ok(Json(u.kernel(refresh).await))
}

/// The daily check: first ten minutes after start, then every 24 hours.
pub fn spawn_daily(u: Arc<Updates>) {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_CHECK).await;
        loop {
            daily_check(&u).await;
            tokio::time::sleep(DAILY).await;
        }
    });
}

async fn daily_check(u: &Arc<Updates>) {
    if !u.apt_present() {
        return;
    }
    let s = u.status(false).await;
    let old = s
        .lists_updated_ms
        .is_none_or(|t| now_ms().saturating_sub(t) > LISTS_MAX_AGE.as_millis() as u64);
    if old && !u.dry && s.running_task.is_none() {
        // the task refreshes the cache and notifies when it ends
        if let Err(e) = u.spawn("updates.check", "rynas", |u, ctx| async move {
            u.run(&ctx, apt(&["update"], Duration::from_secs(600))).await?;
            Ok(())
        }) {
            tracing::warn!("daily update check: {}", e.2);
        }
        return;
    }
    u.notify(&s);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apt_runs_outside_the_sandbox() {
        let c = apt(&["full-upgrade"], Duration::from_secs(1)).display();
        assert!(c.starts_with("systemd-run --wait --pipe --collect --quiet"), "{c}");
        assert!(c.contains("--setenv=DEBIAN_FRONTEND=noninteractive"), "{c}");
        assert!(
            c.contains("-- /usr/bin/apt-get -q -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold full-upgrade"),
            "{c}"
        );
        // simulations stay in rynasd
        assert!(apt_sim(&["autoremove"]).display().starts_with("/usr/bin/apt-get -s"));
    }
}
