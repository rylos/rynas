//! UPS endpoints of the internal API (`/v1/ups/...`, public under `/api/v1/ups/...`), the
//! `ups.safemode` configuration domain and synodeb's pre-Safe Mode hook.
//!
//! On synodeb (`/etc/synodeb/ups.conf` exists) rynas is the interface, synodeb the executor
//! (`docs/synodeb-integration.md`, S2–S4):
//! - `ups.safemode` owns `SAFEMODE_AFTER` and `PRE_SAFEMODE_TIMEOUT` of
//!   `/etc/synodeb/ups.conf` (a `# BEGIN rynas ups` block; other assignments of the two keys
//!   are dropped, the rest of the file stays synodeb's). `synodeb-ups` reads the file at
//!   every event, so an apply needs no reload. Safe Mode also always starts on NUT low
//!   battery and on a critical UPS (OB+LB), whichever comes first: rynas cannot turn that off.
//! - `POST /v1/ups/test` runs `synodeb-ups test quick|deep|stop` as the task `ups.test` and
//!   follows `synodeb-ups test result` until the UPS reports an outcome.
//! - `POST /v1/ups/safe-mode` (root only, `rynasctl ups safe-mode <reason>` from
//!   `/etc/synodeb/ups-pre-safemode.d/50-rynas`): cancels running rynas tasks, stops running
//!   scheduler task units and records "Safe Mode: <reason>" as a notification, within
//!   [`SAFE_MODE_BUDGET`].
//!
//! The UPS status itself is read from upsd by rynas-web (no root needed).

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
    SAFEMODE_AFTER_RANGE_S, SafeModeReport, SafeModeRequest, UpsSettings, UpsSettingsState,
};
use rynas_api_types::{TaskStarted, TaskStatus};
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile};
use rynas_notify::Notifier;
use rynas_notify::event::oneshot;
use rynas_tasks::Cmd;
use serde_json::Value;

use crate::ops::{Error, Ops, Res, user};
use crate::services::{docs, stage};

pub const UPS_KEY: &str = "ups.safemode";
pub const UPS_CONF: &str = "/etc/synodeb/ups.conf";
/// Server-side bound of the pre-Safe Mode work (the hook's `rynasctl` waits 30 s, synodeb's
/// `PRE_SAFEMODE_TIMEOUT` is 120 s by default for every hook together).
pub const SAFE_MODE_BUDGET: Duration = Duration::from_secs(25);
/// Longest wait for the outcome of a battery test.
const QUICK_MAX: Duration = Duration::from_secs(5 * 60);
const DEEP_MAX: Duration = Duration::from_secs(3 * 3600);
const RESULT_EVERY: Duration = Duration::from_secs(10);
const CMD_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------------------------
// the configuration domain
// ---------------------------------------------------------------------------------------------

/// `ups.safemode`: the rynas block of `/etc/synodeb/ups.conf`.
pub struct UpsDomain;

fn settings_of(v: &Value) -> Result<UpsSettings, Invalid> {
    serde_json::from_value(v.clone())
        .map_err(|e| Invalid::from(format!("invalid ups.safemode document: {e}")))
}

/// Checks the ranges; `SAFEMODE_AFTER` is kept in whole minutes (the UI's unit).
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

/// The block's lines.
pub fn render_block(s: &UpsSettings) -> String {
    format!(
        "# Safe Mode settings of the rynas UPS page (ups.safemode). NUT low battery and a\n\
         # critical UPS (OB+LB) always start Safe Mode as well, whichever comes first.\n\
         SAFEMODE_AFTER={}\n\
         PRE_SAFEMODE_TIMEOUT={}\n",
        s.safemode_after_s.unwrap_or(0),
        s.pre_safemode_timeout_s
    )
}

/// The two keys as the shell would see them (the last assignment wins; comments skipped).
pub fn parse_conf(text: &str) -> UpsSettings {
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
        let v = v.split('#').next().unwrap_or("").trim().trim_matches(['"', '\'']);
        match (k.trim(), v.parse::<u32>()) {
            ("SAFEMODE_AFTER", Ok(n)) => s.safemode_after_s = (n > 0).then_some(n),
            ("PRE_SAFEMODE_TIMEOUT", Ok(n)) => s.pre_safemode_timeout_s = n,
            _ => {}
        }
    }
    s
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
        let mut f = ManagedFile::block(UPS_CONF, "ups", render_block(&s));
        if let rynas_config::FileKind::Block { drop_outside, .. } = &mut f.kind {
            *drop_outside = vec!["SAFEMODE_AFTER=".into(), "PRE_SAFEMODE_TIMEOUT=".into()];
        }
        Ok(vec![f])
    }

    /// synodeb-ups reads the file at every UPS event: nothing to reload.
    fn reload(&self, _value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        (ctx.log)("synodeb-ups reads ups.conf at its next event: no reload needed");
        Ok(())
    }

    fn import(&self, ctx: &ApplyCtx) -> Result<Option<Value>, String> {
        let Some(text) = ctx.read(Path::new(UPS_CONF)) else {
            return Ok(None);
        };
        serde_json::to_value(parse_conf(&text))
            .map(Some)
            .map_err(|e| e.to_string())
    }
}

/// Whether this machine runs synodeb's UPS layer (below the configuration root). The file,
/// not the directory: the rynas package itself installs `/etc/synodeb/ups-pre-safemode.d/`.
pub fn synodeb_present(root: &Path) -> bool {
    rynas_config::files::under_root(root, Path::new(UPS_CONF)).is_file()
}

// ---------------------------------------------------------------------------------------------
// endpoints
// ---------------------------------------------------------------------------------------------

pub struct Ups {
    pub ops: Arc<Ops>,
    /// Task Scheduler (its running units are stopped before Safe Mode).
    pub protection: Option<Arc<crate::protection::Protection>>,
    pub notifier: Arc<dyn Notifier>,
    /// `/etc/synodeb` exists and `ups.safemode` is registered.
    pub available: bool,
    /// `--dry-system`: battery tests and unit stops are only logged.
    pub dry: bool,
    /// The synodeb UPS tool.
    pub synodeb_ups: PathBuf,
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

fn synodeb_only(u: &Ups) -> Res<()> {
    if u.available {
        Ok(())
    } else {
        Err(Error::not_found(
            "UPS Safe Mode and battery tests need synodeb (/etc/synodeb)",
        ))
    }
}

pub fn router(st: UpsState) -> Router {
    Router::new()
        .route("/v1/ups/settings", get(settings_get).put(settings_put))
        .route("/v1/ups/test", post(test))
        .merge(
            Router::new()
                .route("/v1/ups/safe-mode", post(safe_mode))
                .route_layer(axum::middleware::from_fn(crate::server::require_root_peer)),
        )
        .with_state(st)
}

fn state_of(u: &Ups, owner: &str) -> Res<UpsSettingsState> {
    if !u.available {
        return Ok(UpsSettingsState::default());
    }
    let (eff, cur, _): (UpsSettings, UpsSettings, Option<UpsSettings>) =
        docs(&u.ops.config, owner, UPS_KEY)?;
    Ok(UpsSettingsState {
        available: true,
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
    synodeb_only(&u)?;
    let Json(req) = body?;
    Ok(Json(
        blocking(&u, move |u| {
            stage(&u.ops.config, &owner, UPS_KEY, &req)?;
            state_of(u, &owner)
        })
        .await?,
    ))
}

/// The text after `ups: ` of `synodeb-ups test result`.
pub fn result_text(out: &str) -> String {
    let l = out.lines().next().unwrap_or("").trim();
    l.split_once(": ").map_or(l, |(_, r)| r).trim().to_string()
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

async fn test(
    State(st): State<UpsState>,
    hd: HeaderMap,
    body: Result<Json<BatteryTestRequest>, JsonRejection>,
) -> Res<Response> {
    let (u, who) = (ups(&st)?, user(&hd)?);
    synodeb_only(&u)?;
    let Json(req) = body?;
    let u2 = u.clone();
    let id = u.ops.tasks.spawn("ups.test", &who, move |ctx| async move {
        let u = u2;
        let kind = req.test;
        let tool = u.synodeb_ups.display().to_string();
        let cmd = |arg: &str| {
            Cmd::new(&u.synodeb_ups)
                .args(["test", arg])
                .env_clear()
                .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
                .timeout(CMD_TIMEOUT)
        };
        if u.dry {
            ctx.log(&format!("dry system: would run {tool} test {}", kind.arg()));
            return Ok(());
        }
        let before = match ctx.run(&cmd("result")).await {
            Ok(o) => result_text(&o.stdout),
            Err(e) => return Err(format!("{tool} test result: {e}")),
        };
        ctx.log(&format!("last result: {before}"));
        let out = ctx
            .run(&cmd(kind.arg()))
            .await
            .map_err(|e| format!("{tool} test {}: {e}", kind.arg()))?;
        if !out.success() {
            return Err(format!(
                "{tool} test {}: {}",
                kind.arg(),
                out.stderr.trim().lines().last().unwrap_or("failed")
            ));
        }
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
                    let _ = ctx.run(&cmd("stop")).await;
                    return Err("cancelled".into());
                }
            }
            let r = match cmd("result").run(None, None).await {
                Ok(o) => result_text(&o.stdout),
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
        ctx.log(&format!("no new result after {} min: {last}", max.as_secs() / 60));
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
        return Err(Error::invalid("reason", "unknown Safe Mode reason"));
    }
    tracing::warn!("UPS Safe Mode ({}): stopping rynas tasks", req.reason);
    let mut ev = oneshot(
        NotifyCategory::Ups,
        NotifySeverity::Critical,
        "ups",
        format!("ups:safemode:{}", req.reason),
        format!("Safe Mode: {}", req.reason),
    );
    ev.body = "The UPS started Safe Mode: rynas stops its running and scheduled tasks; services \
               stop, volumes are unmounted and the NAS restarts when power is back."
        .into();
    u.notifier.emit(ev);
    let report = tokio::time::timeout(SAFE_MODE_BUDGET, stop_everything(&u))
        .await
        .unwrap_or_else(|_| {
            tracing::warn!("Safe Mode: stopping took longer than {SAFE_MODE_BUDGET:?}");
            SafeModeReport::default()
        });
    Ok(Json(report))
}

async fn stop_everything(u: &Arc<Ups>) -> SafeModeReport {
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
                .timeout(SAFE_MODE_BUDGET - Duration::from_secs(5));
            match c.run(None, None).await {
                Ok(o) if o.success() => Some(unit.clone()),
                Ok(o) => {
                    tracing::warn!("Safe Mode: stopping {unit}: {}", o.stderr.trim());
                    None
                }
                Err(e) => {
                    tracing::warn!("Safe Mode: stopping {unit}: {e}");
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
    let waits = report
        .cancelled_tasks
        .iter()
        .map(|id| u.ops.tasks.wait(id));
    let _ = futures_util::future::join_all(waits).await;
    tracing::warn!(
        "Safe Mode: cancelled tasks {:?}, stopped units {:?}",
        report.cancelled_tasks,
        report.stopped_units
    );
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conf_round_trip() {
        let synodeb = "# synodeb\n#SAFEMODE_AFTER=0\nDRYRUN=0\nSAFEMODE_AFTER=300 # five\nPRE_SAFEMODE_TIMEOUT=\"90\"\n";
        let s = parse_conf(synodeb);
        assert_eq!(s.safemode_after_s, Some(300));
        assert_eq!(s.pre_safemode_timeout_s, 90);
        assert_eq!(parse_conf("SAFEMODE_AFTER=0\n"), UpsSettings::default());
        let want = UpsSettings {
            safemode_after_s: Some(600),
            pre_safemode_timeout_s: 120,
        };
        let kind = rynas_config::FileKind::Block {
            id: "ups".into(),
            drop_outside: vec!["SAFEMODE_AFTER=".into(), "PRE_SAFEMODE_TIMEOUT=".into()],
        };
        let (text, _) = rynas_config::files::render_final(&kind, &render_block(&want), Some(synodeb));
        assert!(text.contains("#SAFEMODE_AFTER=0\n"), "{text}");
        assert!(text.contains("DRYRUN=0\n"));
        assert!(!text.contains("=300"));
        assert!(!text.contains("\"90\""));
        assert_eq!(parse_conf(&text), want);
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
        assert_eq!(result_text("ups: Done and passed\n"), "Done and passed");
        assert_eq!(verdict("In progress"), None);
        assert_eq!(verdict("Done and passed"), Some(Ok(())));
        assert!(verdict("Done and error").unwrap().is_err());
        assert!(verdict("Aborted").unwrap().is_err());
        assert!(valid_reason("lowbatt") && !valid_reason("rm -rf") && !valid_reason(""));
    }
}
