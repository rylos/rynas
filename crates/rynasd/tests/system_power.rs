//! Manual restart and shut down (`/v1/system/power`): administrators only, the warnings
//! (fake `/proc`), the action as a task with a fake `shutdown` (a shell script that records
//! its arguments and writes logind's schedule file like logind does), the pending action and
//! its cancel, and the dry system's simulation. Nothing here ever reboots anything.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::power::{PowerWarningCode, SystemPowerAction, SystemPowerState};
use rynas_api_types::{ApiError, TaskInfo, TaskStarted, TaskStatus, error_codes};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_notify::MemoryNotifier;
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::hardware::Hardware;
use rynasd::ops::Ops;
use rynasd::roles::AdminPolicy;
use rynasd::server::{DaemonState, router};
use rynasd::system_power::SystemPower;
use serde_json::{Value, json};
use tower::ServiceExt;

struct NoAuth;

impl Authenticator for NoAuth {
    fn realm(&self) -> &str {
        "none"
    }
    fn authenticate<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Err(AuthError::InvalidCredentials) })
    }
}

struct Env {
    dir: tempfile::TempDir,
    hw: PathBuf,
    app: axum::Router,
    ops: Arc<Ops>,
    notifier: Arc<MemoryNotifier>,
}

fn groups(u: &str) -> Option<Vec<String>> {
    match u {
        "admin" => Some(vec!["rynas-admin".into()]),
        "bob" => Some(vec!["users".into()]),
        _ => None,
    }
}

/// The fake `shutdown`: records its arguments, writes or removes logind's schedule file.
fn fake_shutdown(dir: &Path, hw: &Path) -> Vec<String> {
    let sched = hw.join("run/systemd/shutdown/scheduled");
    let script = format!(
        r#"echo "$@" >> {log}
case "$1" in
  -c) rm -f {sched} ;;
  -r|-P) if [ "$2" != now ]; then
      m=reboot; [ "$1" = -P ] && m=poweroff
      mkdir -p {dir} && printf 'USEC=%s000000\nWARN_WALL=1\nMODE=%s\n' $(( $(date +%s) + 300 )) $m > {sched}
    fi ;;
esac"#,
        log = dir.join("shutdown.log").display(),
        sched = sched.display(),
        dir = sched.parent().unwrap().display(),
    );
    vec!["/bin/sh".into(), "-c".into(), script, "shutdown".into()]
}

fn env(dry: bool) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let hw = dir.path().join("hw");
    let state_dir = dir.path().join("state");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(hw.join("proc")).unwrap();
    std::fs::write(hw.join("proc/mdstat"), "unused devices: <none>\n").unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let config = Arc::new(
        Config::open(ConfigOptions::new(&state_dir, &root), sys, Vec::new()).unwrap(),
    );
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let ops = Arc::new(Ops::new(config, tasks));
    let notifier = Arc::new(MemoryNotifier::default());
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(
        MemoryStorage::default(),
        clock.clone(),
        "rynas",
    ));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    let hardware = Arc::new(Hardware {
        ops: ops.clone(),
        control: None,
        wake_rtc: None,
        hw_root: hw.clone(),
        state_dir,
        notifier: notifier.clone(),
        protection: None,
        poweroff: vec!["/bin/false".into()],
    });
    st.system_power = Some(Arc::new(SystemPower {
        hw: hardware,
        updates: None,
        upsd: None,
        shutdown: if dry {
            Vec::new()
        } else {
            fake_shutdown(dir.path(), &hw)
        },
        grace: Duration::ZERO,
        admins: AdminPolicy::new(vec!["rynas-admin".into()], groups),
    }));
    Env {
        dir,
        hw,
        app: router(st),
        ops,
        notifier,
    }
}

async fn call_as(
    e: &Env,
    who: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(w) = who {
        req = req.header("x-rynas-user", w);
    }
    let req = req
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    let r = e.app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec())
}

async fn call(e: &Env, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    call_as(e, Some("admin"), method, uri, body).await
}

fn code(b: &[u8]) -> String {
    serde_json::from_slice::<ApiError>(b)
        .map(|e| e.code)
        .unwrap_or_else(|_| String::from_utf8_lossy(b).into_owned())
}

async fn state(e: &Env) -> SystemPowerState {
    let (s, b) = call(e, "GET", "/v1/system/power", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    serde_json::from_slice(&b).unwrap()
}

async fn start(e: &Env, action: &str, delay: u32) -> TaskInfo {
    let body = json!({"action": action, "delay_min": delay});
    let (s, b) = call(e, "POST", "/v1/system/power", Some(body)).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    for _ in 0..250 {
        if let Some(t) = e.ops.tasks.get(&id)
            && t.status.is_finished()
        {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {id} still running");
}

fn shutdown_log(e: &Env) -> String {
    std::fs::read_to_string(e.dir.path().join("shutdown.log")).unwrap_or_default()
}

#[tokio::test]
async fn administrators_only() {
    let e = env(false);
    for (m, uri, body) in [
        ("GET", "/v1/system/power", None),
        (
            "POST",
            "/v1/system/power",
            Some(json!({"action": "restart", "delay_min": 0})),
        ),
        ("POST", "/v1/system/power/cancel", None),
    ] {
        let (s, b) = call_as(&e, Some("bob"), m, uri, body.clone()).await;
        assert_eq!(
            (s, code(&b).as_str()),
            (StatusCode::FORBIDDEN, error_codes::FORBIDDEN),
            "{m} {uri}"
        );
        let (s, _) = call_as(&e, None, m, uri, body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{m} {uri} without a user");
    }
    assert!(shutdown_log(&e).is_empty());
    assert!(e.ops.tasks.list(10).is_empty());
}

#[tokio::test]
async fn warnings_list_what_would_be_interrupted() {
    let e = env(false);
    let st = state(&e).await;
    assert!(st.pending.is_none() && st.warnings.is_empty(), "{st:?}");
    std::fs::write(
        e.hw.join("proc/mdstat"),
        "md1 : active raid1 sdb3[1] sda3[0]\n      976630464 blocks [2/2] [UU]\n      [=>....]  recovery =  8.5% (1/2) finish=81.0min speed=1K/sec\n",
    )
    .unwrap();
    std::fs::create_dir_all(e.hw.join("proc/4242")).unwrap();
    std::fs::write(e.hw.join("proc/4242/comm"), "dpkg\n").unwrap();
    let st = state(&e).await;
    let codes: Vec<_> = st.warnings.iter().map(|w| w.code).collect();
    assert_eq!(
        codes,
        [PowerWarningCode::RaidBusy, PowerWarningCode::AptRunning]
    );
    let raid = &st.warnings[0];
    assert_eq!(raid.subject.as_deref(), Some("md1"));
    assert_eq!(raid.op.as_deref(), Some("recovery"));
    assert_eq!(raid.pct, Some(8.5));
    assert_eq!(st.warnings[1].subject.as_deref(), Some("dpkg"));
    // warnings never refuse: the administrator goes on, the task log keeps them
    let t = start(&e, "shutdown", 5).await;
    assert_eq!(t.status, TaskStatus::Ok);
    let log = e.ops.tasks.read_log(&t.id, 0, 1 << 20).unwrap().text;
    assert!(log.contains("warning: md1: RAID recovery 8.5%"), "{log}");
    assert!(log.contains("warning: dpkg running"), "{log}");
}

#[tokio::test]
async fn delayed_restart_then_cancel() {
    let e = env(false);
    let (s, b) = call(
        &e,
        "POST",
        "/v1/system/power",
        Some(json!({"action": "restart", "delay_min": 2})),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE)
    );
    let (s, _) = call(
        &e,
        "POST",
        "/v1/system/power",
        Some(json!({"action": "reboot"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let t = start(&e, "restart", 5).await;
    assert_eq!(t.kind, "system.restart");
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", e.ops.tasks.read_log(&t.id, 0, 1 << 20));
    assert_eq!(
        shutdown_log(&e),
        "-r +5 rynas: the NAS restarts (asked by admin)\n"
    );
    let ev = e.notifier.take();
    assert!(
        ev.iter().any(|x| x.title == "NAS restarting by admin"
            && x.code == "power.manual_restart"
            && x.fields.get("user").map(String::as_str) == Some("admin")
            && x.fields.get("delay_min").map(String::as_str) == Some("5")),
        "{ev:?}"
    );
    let p = state(&e).await.pending.expect("pending restart");
    assert_eq!(p.action, SystemPowerAction::Restart);
    assert_eq!(p.by.as_deref(), Some("admin"));
    // cancel: shutdown -c, the schedule is gone
    let (s, b) = call(&e, "POST", "/v1/system/power/cancel", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{}", String::from_utf8_lossy(&b));
    assert!(shutdown_log(&e).ends_with("-c\n"));
    assert!(state(&e).await.pending.is_none());
    let ev = e.notifier.take();
    assert!(ev.iter().any(|x| x.code == "power.manual_cancelled"), "{ev:?}");
    let (s, b) = call(&e, "POST", "/v1/system/power/cancel", None).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, error_codes::NOTHING_PENDING)
    );
}

#[tokio::test]
async fn schedule_set_outside_rynas_shows_without_a_user() {
    let e = env(false);
    let d = e.hw.join("run/systemd/shutdown");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("scheduled"), "USEC=1900000000000000\nMODE=poweroff\n").unwrap();
    let p = state(&e).await.pending.unwrap();
    assert_eq!(
        (p.action, p.at_ms, p.by),
        (SystemPowerAction::Shutdown, 1_900_000_000_000, None)
    );
}

#[tokio::test]
async fn immediate_shutdown_runs_now() {
    let e = env(false);
    let t = start(&e, "shutdown", 0).await;
    assert_eq!(t.kind, "system.shutdown");
    assert_eq!(t.status, TaskStatus::Ok);
    assert_eq!(
        shutdown_log(&e),
        "-P now rynas: the NAS shuts down (asked by admin)\n"
    );
    assert!(state(&e).await.pending.is_none());
}

#[tokio::test]
async fn dry_system_runs_nothing_and_simulates_the_schedule() {
    let e = env(true);
    let t = start(&e, "shutdown", 1).await;
    assert_eq!(t.status, TaskStatus::Ok);
    let log = e.ops.tasks.read_log(&t.id, 0, 1 << 20).unwrap().text;
    assert!(log.contains("dry system: shutdown -P +1"), "{log}");
    let p = state(&e).await.pending.unwrap();
    assert_eq!(p.action, SystemPowerAction::Shutdown);
    assert_eq!(p.by.as_deref(), Some("admin"));
    assert!(!e.hw.join("run/systemd/shutdown/scheduled").exists());
    let (s, _) = call(&e, "POST", "/v1/system/power/cancel", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(state(&e).await.pending.is_none());
    let t = start(&e, "restart", 0).await;
    assert_eq!(t.status, TaskStatus::Ok);
    assert!(shutdown_log(&e).is_empty());
}
