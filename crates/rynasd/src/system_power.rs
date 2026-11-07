//! Manual restart and shut down of the NAS (`/v1/system/power`, public under
//! `/api/v1/system/power`), administrators only.
//!
//! - `GET /v1/system/power`: the restart or shut down scheduled for later, if any, and the
//!   warnings (what an action now would interrupt): RAID resync/rebuild/check
//!   (`/proc/mdstat`), scrubs and other rynas tasks, Task Scheduler units, files open over
//!   SMB (`smbstatus -L --json`), apt/dpkg processes, the UPS on battery (upsd), and a new
//!   kernel armed for a trial boot by a module's boot layer (its cached status only). The
//!   administrator may go on after reading them: nothing here refuses because of a warning.
//! - `POST /v1/system/power` `{action, delay_min}` (0, 1 or 5 minutes): the task
//!   `system.restart` / `system.shutdown` logs the warnings, writes the audit line, sends the
//!   notification "NAS restarting/shutting down" with the user, then runs systemd's
//!   `shutdown -r|-P now|+N "<message>"`. `now` waits a few seconds first, so the answer and
//!   the notification leave before the network goes down.
//! - `POST /v1/system/power/cancel`: `shutdown -c` (204; 409 `nothing_pending`).
//!
//! Why `shutdown` and not `systemctl reboot`: a delayed action is kept by systemd-logind
//! (`/run/systemd/shutdown/scheduled`), outlives a restart of rynasd, warns the users logged in
//! over SSH, blocks new non-root logins in its last five minutes (`/run/nologin`) and is
//! cancelled with `shutdown -c`; a schedule set over SSH shows here too. Both talk to systemd
//! over D-Bus, which rynasd's sandbox allows (`AF_UNIX`, root, no new capability); the
//! reboot itself is done by PID 1, outside the sandbox.
//!
//! `--dry-system`: `shutdown` is never run; logind's schedule is simulated by a file in the
//! state directory (`power/dry-scheduled`), so the interface can be tried end to end.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
use rynas_api_types::power::{
    PendingSystemPower, PowerWarning, PowerWarningCode, SystemPowerAction, SystemPowerRequest,
    SystemPowerState,
};
use rynas_api_types::updates::KernelTrial;
use rynas_api_types::{TaskStarted, TaskStatus, error_codes};
use rynas_notify::event::{code, field, oneshot};
use rynas_power::manual::{
    SCHEDULED, apt_processes, parse_scheduled, shutdown_args, valid_delay,
};
use rynas_power::policy::{mdstat_ops, smb_open_files};
use rynas_tasks::Cmd;
use serde::{Deserialize, Serialize};

use crate::hardware::Hardware;
use crate::ops::{Error, Res, user};
use crate::roles::AdminPolicy;

const SMBSTATUS_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

pub struct SystemPower {
    pub hw: Arc<Hardware>,
    /// The kernel status of the updates page (a boot layer's trial boot).
    pub updates: Option<Arc<crate::updates::Updates>>,
    /// upsd's address when rynasd may talk to it.
    pub upsd: Option<String>,
    /// argv of systemd's `shutdown` (tests: a fake); empty: dry system, nothing is run.
    pub shutdown: Vec<String>,
    /// Wait before an immediate action.
    pub grace: Duration,
    pub admins: AdminPolicy,
}

pub type SpState = Option<Arc<SystemPower>>;

/// Who asked for the scheduled action (logind only knows the uid, root).
#[derive(Serialize, Deserialize)]
struct Asked {
    action: SystemPowerAction,
    at_ms: u64,
    by: String,
}

impl SystemPower {
    fn dry(&self) -> bool {
        self.shutdown.is_empty()
    }

    fn scheduled_path(&self) -> PathBuf {
        if self.dry() {
            self.hw.state_dir.join("power/dry-scheduled")
        } else {
            rynas_hw::under(&self.hw.hw_root, SCHEDULED)
        }
    }

    fn asked_path(&self) -> PathBuf {
        self.hw.state_dir.join("power/manual.json")
    }

    pub fn pending(&self) -> Option<PendingSystemPower> {
        let text = std::fs::read_to_string(self.scheduled_path()).ok()?;
        let (action, at_ms) = parse_scheduled(&text)?;
        let by = std::fs::read(self.asked_path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Asked>(&b).ok())
            .filter(|a| a.action == action && a.at_ms.abs_diff(at_ms) < 120_000)
            .map(|a| a.by);
        Some(PendingSystemPower { action, at_ms, by })
    }

    /// What an action now would interrupt (`me`: the task asking, left out).
    pub async fn warnings(&self, me: Option<&str>) -> Vec<PowerWarning> {
        let h = &self.hw;
        let mut out = Vec::new();
        let mdstat = rynas_hw::under(&h.hw_root, "/proc/mdstat");
        let text = tokio::fs::read_to_string(mdstat).await.unwrap_or_default();
        for m in mdstat_ops(&text) {
            out.push(PowerWarning {
                code: PowerWarningCode::RaidBusy,
                text: format!("{}: RAID {} {}", m.array, m.op, m.progress)
                    .trim_end()
                    .to_string(),
                pct: m.pct(),
                subject: Some(m.array),
                op: Some(m.op.to_string()),
                n: None,
            });
        }
        for t in h.ops.tasks.list(500) {
            if t.status != TaskStatus::Running
                || Some(t.id.as_str()) == me
                || t.kind.starts_with("system.")
            {
                continue;
            }
            let scrub = t.kind.contains("scrub");
            out.push(warning(
                if scrub {
                    PowerWarningCode::ScrubRunning
                } else {
                    PowerWarningCode::TaskRunning
                },
                Some(t.kind.clone()),
                None,
                format!("rynas task {} running", t.kind),
            ));
        }
        if let Some(prot) = &h.protection {
            for u in prot.runtime().await.running.iter().cloned() {
                let text = format!("scheduler task {u} running");
                out.push(warning(
                    PowerWarningCode::SchedulerTaskRunning,
                    Some(u),
                    None,
                    text,
                ));
            }
        }
        if !self.dry() {
            let cmd = Cmd::new("smbstatus")
                .args(["-L", "--json"])
                .env_clear()
                .timeout(SMBSTATUS_TIMEOUT);
            if let Ok(o) = cmd.run(None, None).await
                && o.success()
                && let Ok(n) = smb_open_files(&o.stdout)
                && n > 0
            {
                out.push(warning(
                    PowerWarningCode::SmbOpenFiles,
                    None,
                    Some(n as u64),
                    format!("{n} files open over SMB"),
                ));
            }
        }
        let proc = rynas_hw::under(&h.hw_root, "/proc");
        let apt = tokio::task::spawn_blocking(move || apt_processes(&proc))
            .await
            .unwrap_or_default();
        for p in apt {
            let text = format!("{p} running");
            out.push(warning(PowerWarningCode::AptRunning, Some(p), None, text));
        }
        if let Some(addr) = &self.upsd
            && rynas_nut::read(addr).await.on_battery()
        {
            out.push(warning(
                PowerWarningCode::UpsOnBattery,
                None,
                None,
                "the UPS runs on battery".into(),
            ));
        }
        if let Some(u) = &self.updates
            && u
                .kernel_cached()
                .and_then(|k| k.boot)
                .is_some_and(|b| b.trial == KernelTrial::Armed)
        {
            out.push(warning(
                PowerWarningCode::BootTrial,
                None,
                None,
                "the next boot tries the new kernel; if it fails, the previous one comes back"
                    .into(),
            ));
        }
        out
    }

    async fn state(&self) -> SystemPowerState {
        SystemPowerState {
            pending: self.pending(),
            warnings: self.warnings(None).await,
        }
    }

    /// Runs `shutdown <args>`, or simulates logind's schedule on a dry system.
    async fn run(&self, args: &[String]) -> Result<Option<String>, String> {
        let Some((prog, pre)) = self.shutdown.split_first() else {
            let note = format!("dry system: shutdown {} not run", args.join(" "));
            return self.simulate(args).map(|()| Some(note));
        };
        let o = Cmd::new(prog)
            .args(pre)
            .args(args)
            .env_clear()
            .timeout(SHUTDOWN_TIMEOUT)
            .run(None, None)
            .await
            .map_err(|e| format!("shutdown: {e}"))?;
        if o.success() {
            Ok(None)
        } else {
            Err(format!(
                "shutdown: exit code {:?}: {}",
                o.code,
                o.stderr.trim()
            ))
        }
    }

    fn simulate(&self, args: &[String]) -> Result<(), String> {
        let p = self.scheduled_path();
        let when = args.get(1).and_then(|w| w.strip_prefix('+'));
        match (args.first().map(String::as_str), when) {
            (Some("-c"), _) => {
                let _ = std::fs::remove_file(&p);
            }
            (Some(flag), Some(min)) => {
                let min: u64 = min.parse().map_err(|_| "bad delay".to_string())?;
                let mode = if flag == "-r" { "reboot" } else { "poweroff" };
                let usec = (rynas_tasks::now_ms() + min * 60_000) * 1000;
                if let Some(d) = p.parent() {
                    std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
                }
                std::fs::write(&p, format!("USEC={usec}\nWARN_WALL=1\nMODE={mode}\n"))
                    .map_err(|e| e.to_string())?;
            }
            // now: nothing would come back
            _ => {
                let _ = std::fs::remove_file(&p);
            }
        }
        Ok(())
    }
}

fn warning(
    code: PowerWarningCode,
    subject: Option<String>,
    n: Option<u64>,
    text: String,
) -> PowerWarning {
    PowerWarning {
        code,
        subject,
        op: None,
        pct: None,
        n,
        text,
    }
}

fn sp(st: &SpState) -> Res<Arc<SystemPower>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "restart and shut down are not enabled in this rynasd".into(),
        )
    })
}

/// A line of the audit log.
fn audit(who: &str, action: &str, target: &str) {
    tracing::info!(target: "rynas::audit", "system power {action} {target} by {who}");
}

fn verb(a: SystemPowerAction) -> &'static str {
    match a {
        SystemPowerAction::Restart => "restart",
        SystemPowerAction::Shutdown => "shutdown",
    }
}

pub fn router(st: SpState) -> Router {
    let admins = st.as_ref().map(|s| s.admins.clone()).unwrap_or_default();
    Router::new()
        .route("/v1/system/power", get(state_get).post(power_post))
        .route("/v1/system/power/cancel", post(cancel_post))
        .with_state(st)
        .route_layer(axum::middleware::from_fn_with_state(
            crate::roles::AdminGate {
                policy: admins,
                what: "restart and shut down",
            },
            crate::roles::admin_only,
        ))
}

async fn state_get(State(st): State<SpState>) -> Res<Json<SystemPowerState>> {
    let s = sp(&st)?;
    Ok(Json(s.state().await))
}

async fn power_post(
    State(st): State<SpState>,
    hd: HeaderMap,
    body: Result<Json<SystemPowerRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (sp(&st)?, user(&hd)?);
    let Json(req) = body?;
    if !valid_delay(req.delay_min) {
        return Err(Error::invalid("delay_min", "the delay is 0, 1 or 5 minutes"));
    }
    let (action, delay) = (req.action, req.delay_min);
    let kind = format!("system.{}", verb(action));
    let s2 = s.clone();
    let who2 = who.clone();
    let id = s.hw.ops.tasks.spawn(&kind, &who, move |ctx| async move {
        let (s, who) = (s2, who2);
        let warnings = s.warnings(Some(ctx.id())).await;
        for w in &warnings {
            ctx.log(&format!("warning: {}", w.text));
        }
        if !warnings.is_empty() {
            ctx.log("going on as asked by the administrator");
        }
        let when = if delay == 0 {
            "now".to_string()
        } else {
            format!("in {delay} min")
        };
        audit(&who, verb(action), &when);
        let (title, body) = match action {
            SystemPowerAction::Restart => (
                format!("NAS restarting by {who}"),
                format!("{who} restarts the NAS {when}."),
            ),
            SystemPowerAction::Shutdown => (
                format!("NAS shutting down by {who}"),
                format!("{who} shuts the NAS down {when}."),
            ),
        };
        let at_ms = rynas_tasks::now_ms() + u64::from(delay) * 60_000;
        let mut ev = oneshot(
            NotifyCategory::System,
            NotifySeverity::Warning,
            "power",
            format!("power:manual:{}:{at_ms}", verb(action)),
            title,
        );
        ev.body = if warnings.is_empty() {
            body
        } else {
            let list: Vec<&str> = warnings.iter().map(|w| w.text.as_str()).collect();
            format!("{body} Interrupted: {}.", list.join("; "))
        };
        let ev = field(code(ev, &format!("power.manual_{}", verb(action))), "user", &who);
        s.hw.notifier.emit(field(ev, "delay_min", delay));
        let asked = s.asked_path();
        if delay > 0 {
            let a = Asked {
                action,
                at_ms,
                by: who.clone(),
            };
            if let Some(d) = asked.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = std::fs::write(&asked, serde_json::to_vec(&a).unwrap_or_default());
        } else {
            let _ = std::fs::remove_file(&asked);
            ctx.log(&format!("{} in {} s", verb(action), s.grace.as_secs()));
            tokio::time::sleep(s.grace).await;
        }
        let msg = match action {
            SystemPowerAction::Restart => format!("rynas: the NAS restarts (asked by {who})"),
            SystemPowerAction::Shutdown => format!("rynas: the NAS shuts down (asked by {who})"),
        };
        let args = shutdown_args(action, delay, &msg);
        ctx.log(&format!("shutdown {}", args[..2].join(" ")));
        if let Some(note) = s.run(&args).await? {
            ctx.log(&note);
        }
        Ok(())
    })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

async fn cancel_post(State(st): State<SpState>, hd: HeaderMap) -> Res<StatusCode> {
    let (s, who) = (sp(&st)?, user(&hd)?);
    let Some(p) = s.pending() else {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::NOTHING_PENDING,
            "no restart or shut down is scheduled".into(),
        ));
    };
    if let Some(note) = s.run(&["-c".to_string()])
        .await
        .map_err(Error::internal)?
    {
        tracing::info!("{note}");
    }
    let _ = std::fs::remove_file(s.asked_path());
    audit(&who, &format!("{}.cancel", verb(p.action)), "-");
    let title = match p.action {
        SystemPowerAction::Restart => format!("NAS restart cancelled by {who}"),
        SystemPowerAction::Shutdown => format!("NAS shut down cancelled by {who}"),
    };
    let mut ev = oneshot(
        NotifyCategory::System,
        NotifySeverity::Info,
        "power",
        format!("power:manual:cancel:{}", p.at_ms),
        title,
    );
    ev.body = "The NAS stays on.".into();
    s.hw
        .notifier
        .emit(field(code(ev, "power.manual_cancelled"), "user", &who));
    Ok(StatusCode::NO_CONTENT)
}
