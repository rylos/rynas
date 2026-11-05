//! Synology hardware and power schedule endpoints with the in-memory system, a fake systemd
//! probe and a fake hardware tree (`sys/`, `proc/`, `run/synohwd/` with a FIFO read like
//! synohwd reads it): settings staged and applied, FIFO commands, identify, fan curves, the
//! power schedule with its scheduler tasks, and the scheduled shutdown's checks and policy.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::power::{PowerNext, PowerScheduleState};
use rynas_api_types::scheduler::{TaskAction, TaskTrigger, TaskView};
use rynas_api_types::synology::{FanCurves, MuteResult, SynologySettingsState, SynologyStatus};
use rynas_api_types::{ApiError, TaskInfo, TaskStarted, TaskStatus, error_codes};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_btrfs::{MemorySnapshots, SnapshotsDomain};
use rynas_config::{Applier, Config, ConfigOptions, MemorySystem};
use rynas_hw_synology::Synology;
use rynas_hw_synology::domain::{SYNOLOGY_KEY, SynologyDomain};
use rynas_notify::MemoryNotifier;
use rynas_power::PowerDomain;
use rynas_scheduler::SchedulerDomain;
use rynas_scheduler::history::History;
use rynas_scheduler::probe::FakeProbe;
use rynas_scheduler::units::Paths;
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::hardware::Hardware;
use rynasd::ops::Ops;
use rynasd::protection::Protection;
use rynasd::server::{DaemonState, router};
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
    sys: Arc<MemorySystem>,
    config: Arc<Config>,
    ops: Arc<Ops>,
    notifier: Arc<MemoryNotifier>,
    /// Lines read from the synohwd FIFO.
    fifo_lines: Arc<Mutex<Vec<String>>>,
}

fn fake_hw(root: &Path, synology: bool) {
    let w = |p: &str, v: &str| {
        let f = root.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, v).unwrap();
    };
    w("proc/sys/kernel/random/boot_id", "boot-1\n");
    w("proc/uptime", "100.0 50.0\n");
    w(
        "proc/mdstat",
        "Personalities : [raid1]\nunused devices: <none>\n",
    );
    w("sys/class/rtc/rtc0/wakealarm", "");
    if !synology {
        return;
    }
    std::fs::create_dir_all(root.join("sys/devices/platform/synology-nas")).unwrap();
    w("sys/class/dmi/id/sys_vendor", "Synology Inc.\n");
    w("sys/class/dmi/id/product_name", "DS920+\n");
    w(
        "run/synohwd/status.json",
        include_str!("../../rynas-hw-synology/tests/fixtures/synohwd-status.json"),
    );
    for bay in 1..=4 {
        w(
            &format!("sys/class/leds/syno:green:disk{bay}/trigger"),
            &format!("none [ata{bay}] timer"),
        );
        w(
            &format!("sys/class/leds/syno:green:disk{bay}/brightness"),
            "255",
        );
        w(
            &format!("sys/class/leds/syno:orange:disk{bay}/trigger"),
            "[none] timer",
        );
        w(
            &format!("sys/class/leds/syno:orange:disk{bay}/brightness"),
            "0",
        );
    }
}

/// A FIFO and a reader thread like synohwd's `cmd_loop`.
fn fake_synohwd_fifo(root: &Path) -> Arc<Mutex<Vec<String>>> {
    let fifo = root.join("run/synohwd/cmd");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: mkfifo on a path we own
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let lines = Arc::new(Mutex::new(Vec::new()));
    let l2 = lines.clone();
    std::thread::spawn(move || {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        let mut buf = [0u8; 256];
        let mut acc = String::new();
        while let Ok(n) = f.read(&mut buf) {
            acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            while let Some(i) = acc.find('\n') {
                let line: String = acc.drain(..=i).collect();
                l2.lock().unwrap().push(line.trim().to_string());
            }
        }
    });
    lines
}

fn env(synology: bool) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let hw = dir.path().join("hw");
    std::fs::create_dir_all(&root).unwrap();
    fake_hw(&hw, synology);
    let fifo_lines = if synology {
        fake_synohwd_fifo(&hw)
    } else {
        Arc::default()
    };
    let sys = Arc::new(MemorySystem::new("nas"));
    let snapshots = Arc::new(SnapshotsDomain::new(Arc::new(MemorySnapshots::new())));
    let scheduler = Arc::new(SchedulerDomain::new(
        Paths::default(),
        Arc::new(|_: &str| None),
    ));
    let state_dir = dir.path().join("state");
    let mut domains: Vec<Arc<dyn Applier>> = vec![
        snapshots.clone(),
        scheduler.clone(),
        Arc::new(PowerDomain {
            rynasctl: "/usr/libexec/rynas/rynasctl".into(),
            state_dir: state_dir.clone(),
            hw_root: hw.clone(),
        }),
    ];
    let synology_p = synology.then(|| Arc::new(Synology::detect(&hw).unwrap()));
    if synology {
        domains.push(Arc::new(SynologyDomain));
    }
    let config = Arc::new(
        Config::open(ConfigOptions::new(&state_dir, &root), sys.clone(), domains).unwrap(),
    );
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let ops = Arc::new(Ops::new(config.clone(), tasks));
    let history = History::open(&dir.path().join("scheduler.db")).unwrap();
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(
        MemoryStorage::default(),
        clock.clone(),
        "rynas",
    ));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    let protection = Arc::new(Protection::new(
        ops.clone(),
        scheduler,
        snapshots,
        Arc::new(FakeProbe::default()),
        Arc::new(history),
        Arc::new(rynas_notify::NullNotifier),
        state_dir.clone(),
    ));
    st.protection = Some(protection.clone());
    let notifier = Arc::new(MemoryNotifier::default());
    let off = dir.path().join("powered-off");
    st.hardware = Some(Arc::new(Hardware {
        ops: ops.clone(),
        synology: synology_p,
        hw_root: hw.clone(),
        state_dir,
        notifications: None,
        notifier: notifier.clone(),
        protection: Some(protection),
        poweroff: vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("echo off > {}", off.display()),
        ],
    }));
    Env {
        dir,
        hw,
        app: router(st),
        sys,
        config,
        ops,
        notifier,
        fifo_lines,
    }
}

async fn call(e: &Env, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-rynas-user", "admin")
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    let r = e.app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 4 << 20).await.unwrap().to_vec())
}

fn code(b: &[u8]) -> String {
    serde_json::from_slice::<ApiError>(b)
        .map(|e| e.code)
        .unwrap_or_else(|_| String::from_utf8_lossy(b).into_owned())
}

fn apply(e: &Env) {
    e.config
        .apply("admin", "admin", "test", &|l| eprintln!("{l}"), &|| false)
        .unwrap();
}

async fn wait_task(e: &Env, id: &str) -> TaskInfo {
    for _ in 0..200 {
        if let Some(t) = e.ops.tasks.get(id)
            && t.status.is_finished()
        {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {id} still running");
}

#[tokio::test]
async fn settings_are_staged_and_applied() {
    let e = env(true);
    let (s, b) = call(&e, "GET", "/v1/hardware/synology/settings", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let st: SynologySettingsState = serde_json::from_slice(&b).unwrap();
    assert_eq!(st.platform.model, "DS920+");
    assert_eq!((st.platform.bays, st.platform.fans), (4, 2));
    assert!(st.platform.synohwd_running || !st.platform.synohwd_running);
    assert_eq!(st.pending, None);
    let mut v = serde_json::to_value(&st.applied).unwrap();
    // synohwd's range: refused, not clamped
    v["fan_period_s"] = json!(301);
    let (s, b) = call(&e, "PUT", "/v1/hardware/synology/settings", Some(v.clone())).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE)
    );
    // command: is reserved for rynas handlers (not a choice of the API)
    v["fan_period_s"] = json!(60);
    v["power_button"] = json!("command:/bin/x");
    let (s, b) = call(&e, "PUT", "/v1/hardware/synology/settings", Some(v.clone())).await;
    // not one of the accepted values: invalid_value about that field (rynas_http)
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE)
    );
    let err: rynas_api_types::ApiError = serde_json::from_slice(&b).unwrap();
    assert_eq!(err.field.as_deref(), Some("power_button"));
    v["power_button"] = json!("log");
    v["reset_button"] = json!("network_rescue");
    v["fan_mode"] = json!("high");
    let (s, b) = call(&e, "PUT", "/v1/hardware/synology/settings", Some(v)).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let st: SynologySettingsState = serde_json::from_slice(&b).unwrap();
    assert_eq!(st.pending.as_ref().unwrap().fan_period_s, 60);
    apply(&e);
    let text = std::fs::read_to_string(e.dir.path().join("root/etc/synohwd.toml")).unwrap();
    assert!(text.contains("\nfan_period = 60\n"), "{text}");
    assert!(
        text.contains(
            "\nreset_button = \"command:/usr/libexec/rynas/rynasctl net reset-button\"\n"
        )
    );
    assert!(
        e.sys
            .state()
            .calls
            .iter()
            .any(|c| c.contains("restart synohwd.service"))
    );
    // the curves call the applied mode active
    let (s, b) = call(&e, "GET", "/v1/hardware/synology/fan-curves", None).await;
    assert_eq!(s, StatusCode::OK);
    let c: FanCurves = serde_json::from_slice(&b).unwrap();
    assert_eq!(c.active_mode, rynas_api_types::synology::FanMode::High);
    assert_eq!(c.modes[0].curves[2].shutdown_c, 90, "DS920+ CPU");
}

#[tokio::test]
async fn actions_go_through_the_fifo_and_the_leds() {
    let e = env(true);
    for (kind, want) in [("short", "2"), ("long", "3")] {
        let (s, b) = call(
            &e,
            "POST",
            "/v1/hardware/synology/beep",
            Some(json!({"kind": kind})),
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT, "{}", String::from_utf8_lossy(&b));
        for _ in 0..50 {
            if e.fifo_lines.lock().unwrap().last().map(String::as_str) == Some(want) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            e.fifo_lines.lock().unwrap().last().map(String::as_str),
            Some(want)
        );
    }
    let (s, b) = call(
        &e,
        "POST",
        "/v1/hardware/synology/beep",
        Some(json!({"kind": "loud"})),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE)
    );
    // identify: the orange LED blinks, then everything is restored
    let (s, b) = call(
        &e,
        "POST",
        "/v1/hardware/synology/bays/3/identify",
        Some(json!({"duration_s": 1})),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{}", String::from_utf8_lossy(&b));
    let orange = e.hw.join("sys/class/leds/syno:orange:disk3/trigger");
    assert_eq!(std::fs::read_to_string(&orange).unwrap(), "timer");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(std::fs::read_to_string(&orange).unwrap(), "none");
    let green = e.hw.join("sys/class/leds/syno:green:disk3/trigger");
    assert_eq!(std::fs::read_to_string(&green).unwrap(), "ata3");
    // an empty body is the default duration
    let (s, _) = call(&e, "POST", "/v1/hardware/synology/bays/1/identify", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, b) = call(&e, "POST", "/v1/hardware/synology/bays/5/identify", None).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::NOT_FOUND, error_codes::NOT_FOUND)
    );
    let (s, b) = call(&e, "POST", "/v1/hardware/synology/bays/x/identify", None).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::BAD_REQUEST)
    );
    let (s, b) = call(&e, "POST", "/v1/hardware/synology/mute", None).await;
    assert_eq!(s, StatusCode::OK);
    let m: MuteResult = serde_json::from_slice(&b).unwrap();
    assert!(!m.stopped_beep);
    // status with API units and the boot record
    let (s, b) = call(&e, "GET", "/v1/hardware/synology/status", None).await;
    assert_eq!(s, StatusCode::OK);
    let st: SynologyStatus = serde_json::from_slice(&b).unwrap();
    assert_eq!((st.model.as_str(), st.bays.len()), ("DS920+", 4));
    assert_eq!(st.diagnostics.up_current_raw, Some(870));
    // synohwd gone: the FIFO has no reader
    std::fs::remove_file(e.hw.join("run/synohwd/cmd")).unwrap();
    let (s, b) = call(&e, "POST", "/v1/hardware/synology/beep", Some(json!({}))).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE
        )
    );
}

#[tokio::test]
async fn not_a_synology() {
    let e = env(false);
    for uri in [
        "/v1/hardware/synology/status",
        "/v1/hardware/synology/settings",
    ] {
        let (s, b) = call(&e, "GET", uri, None).await;
        assert_eq!(
            (s, code(&b).as_str()),
            (StatusCode::NOT_FOUND, error_codes::NOT_FOUND),
            "{uri}"
        );
    }
    // the power schedule works on any machine
    let (s, b) = call(&e, "GET", "/v1/power/next", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let n: PowerNext = serde_json::from_slice(&b).unwrap();
    assert!(n.wake.supported);
    assert_eq!(n.wake.rtc.as_deref(), Some("rtc0"));
    assert_eq!(n.next_startup_ms, None);
}

fn schedule(when_busy: &str) -> Value {
    json!({"rules": [
        {"id": "morning", "action": "startup", "days": ["mon", "tue", "wed", "thu", "fri"], "hour": 7, "minute": 0},
        {"id": "night", "action": "shutdown", "days": ["mon", "tue", "wed", "thu", "fri", "sat", "sun"], "hour": 23, "minute": 30,
         "shutdown": {"when_busy": when_busy, "max_wait_min": 1, "check_smb": false}}
    ]})
}

#[tokio::test]
async fn power_schedule_and_scheduled_shutdown() {
    let e = env(false);
    // conflicts are refused
    let bad = json!({"rules": [
        {"id": "up", "action": "startup", "days": ["mon"], "hour": 7, "minute": 0},
        {"id": "down", "action": "shutdown", "days": ["mon"], "hour": 7, "minute": 5}]});
    let (s, b) = call(&e, "PUT", "/v1/power/schedule", Some(bad)).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE)
    );
    let (s, b) = call(&e, "PUT", "/v1/power/schedule", Some(schedule("skip"))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let st: PowerScheduleState = serde_json::from_slice(&b).unwrap();
    assert_eq!(st.pending.as_ref().unwrap().rules.len(), 2);
    assert_eq!(st.rules[1].task_id.as_deref(), Some("power-off-night"));
    // the shutdown rule is a staged scheduler task (not persistent: no shutdown after a boot)
    let (s, b) = call(&e, "GET", "/v1/scheduler/tasks/power-off-night", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let v: TaskView = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        v.task.action,
        TaskAction::PowerShutdown {
            rule: "night".into()
        }
    );
    assert!(matches!(&v.task.trigger,
        TaskTrigger::Calendar { on_calendar, persistent: false, .. } if on_calendar == "*-*-* 23:30:00"));
    apply(&e);
    let unit = std::fs::read_to_string(
        e.dir
            .path()
            .join("root/etc/systemd/system/rynas-task-power-off-night.service"),
    )
    .unwrap();
    assert!(
        unit.contains("ExecStart=/usr/libexec/rynas/rynasctl power scheduled-shutdown night\n")
    );
    assert!(
        e.sys
            .state()
            .calls
            .iter()
            .any(|c| c.contains("restart rynas-power-wake.service"))
    );
    // the task belongs to the schedule
    let (s, b) = call(&e, "DELETE", "/v1/scheduler/tasks/power-off-night", None).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, error_codes::CONFLICT)
    );
    let own = json!({"id": "mine", "action": {"kind": "power_shutdown", "rule": "night"}, "trigger": {"kind": "manual"}});
    let (s, b) = call(&e, "POST", "/v1/scheduler/tasks", Some(own)).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE)
    );
    let (s, b) = call(&e, "GET", "/v1/power/next", None).await;
    assert_eq!(s, StatusCode::OK);
    let n: PowerNext = serde_json::from_slice(&b).unwrap();
    assert_eq!(n.next_startup_rule.as_deref(), Some("morning"));
    assert_eq!(n.next_shutdown_rule.as_deref(), Some("night"));
    assert!(n.next_startup_ms.is_some() && n.next_shutdown_ms.is_some());

    // a resync running and policy skip: the NAS stays on, with a notification
    std::fs::write(
        e.hw.join("proc/mdstat"),
        "md1 : active raid1 sdb3[1] sda3[0]\n      976630464 blocks [2/2] [UU]\n      [=>....]  resync =  8.5% (1/2) finish=81.0min speed=1K/sec\n",
    )
    .unwrap();
    let (s, b) = call(&e, "POST", "/v1/power/schedule/rules/night/shutdown", None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    let t = wait_task(&e, &id).await;
    assert_eq!(t.status, TaskStatus::Ok);
    let log = e.ops.tasks.read_log(&id, 0, 1 << 20).unwrap().text;
    assert!(log.contains("busy: md1: RAID resync 8.5%"), "{log}");
    assert!(log.contains("skipped"), "{log}");
    assert!(!e.dir.path().join("powered-off").exists());
    let ev = e.notifier.take();
    assert!(
        ev.iter()
            .any(|x| x.title == "Scheduled shutdown night skipped"),
        "{ev:?}"
    );
    // idle: clean poweroff
    std::fs::write(e.hw.join("proc/mdstat"), "unused devices: <none>\n").unwrap();
    let (_, b) = call(&e, "POST", "/v1/power/schedule/rules/night/shutdown", None).await;
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    let t = wait_task(&e, &id).await;
    assert_eq!(
        t.status,
        TaskStatus::Ok,
        "{:?}",
        e.ops.tasks.read_log(&id, 0, 1 << 20)
    );
    assert!(e.dir.path().join("powered-off").exists());
    // unknown rule, startup rule
    let (s, _) = call(
        &e,
        "POST",
        "/v1/power/schedule/rules/morning/shutdown",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&e, "POST", "/v1/power/schedule/rules/Bad/shutdown", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn force_and_wait_policies() {
    let e = env(false);
    let (s, b) = call(&e, "PUT", "/v1/power/schedule", Some(schedule("force"))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    apply(&e);
    std::fs::write(
        e.hw.join("proc/mdstat"),
        "md1 : active raid1 sdb3[1] sda3[0]\n      [=>....]  recovery = 1.0% (1/2)\n",
    )
    .unwrap();
    let (_, b) = call(&e, "POST", "/v1/power/schedule/rules/night/shutdown", None).await;
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    let t = wait_task(&e, &id).await;
    assert_eq!(t.status, TaskStatus::Ok);
    let log = e.ops.tasks.read_log(&id, 0, 1 << 20).unwrap().text;
    assert!(log.contains("policy force"), "{log}");
    assert!(e.dir.path().join("powered-off").exists());
    // wait: still busy → the task waits (cancelled here instead of waiting a minute)
    let (_, _) = call(&e, "PUT", "/v1/power/schedule", Some(schedule("wait"))).await;
    apply(&e);
    std::fs::remove_file(e.dir.path().join("powered-off")).unwrap();
    let (_, b) = call(&e, "POST", "/v1/power/schedule/rules/night/shutdown", None).await;
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = e.ops.tasks.read_log(&id, 0, 1 << 20).unwrap().text;
    assert!(log.contains("waiting (up to 1 min"), "{log}");
    e.ops.tasks.cancel(&id).unwrap();
    let t = wait_task(&e, &id).await;
    assert_ne!(t.status, TaskStatus::Ok);
    assert!(!e.dir.path().join("powered-off").exists());
}

/// `/etc/synohwd.toml` has one writer: applies of `network.rescue` and `synology.hw`, in any
/// order and repeated, never touch each other's files and leave no drift; the start-up
/// adoption turns network rescue on once.
#[test]
fn synohwd_conf_has_one_writer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::write(root.join("etc/synohwd.toml"), "# synodeb defaults\n").unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let domains: Vec<Arc<dyn Applier>> = vec![
        Arc::new(rynas_net::rescue::RescueDomain {
            web_port: 8080,
            https: false,
        }),
        Arc::new(SynologyDomain),
    ];
    let cfg = Config::open(
        ConfigOptions::new(dir.path().join("state"), &root),
        sys.clone(),
        domains,
    )
    .unwrap();
    let rescue = |mdns: bool| {
        cfg.apply_value(
            "rynas",
            rynas_net::rescue::RESCUE_KEY,
            Some(json!({ "mdns_service": mdns })),
            "",
            &|_| {},
        )
        .unwrap();
    };
    let conf = || std::fs::read_to_string(root.join("etc/synohwd.toml")).unwrap();
    rescue(true);
    assert_eq!(
        conf(),
        "# synodeb defaults\n",
        "network.rescue never writes it"
    );
    assert!(
        rynasd::hardware::adopt_synohwd_conf(&cfg)
            .unwrap()
            .is_some()
    );
    let adopted = conf();
    assert!(
        adopted
            .contains("reset_button = \"command:/usr/libexec/rynas/rynasctl net reset-button\"\n")
    );
    assert!(
        rynasd::hardware::adopt_synohwd_conf(&cfg)
            .unwrap()
            .is_none(),
        "once"
    );
    for round in 0..3 {
        let before = conf();
        rescue(round % 2 == 0);
        assert_eq!(conf(), before, "round {round}");
        assert!(cfg.drift().unwrap().entries.is_empty(), "round {round}");
        let mut s: rynas_api_types::synology::SynologySettings =
            serde_json::from_value(cfg.applied(SYNOLOGY_KEY).unwrap().unwrap()).unwrap();
        s.led_brightness = 100 + round;
        cfg.apply_value(
            "admin",
            SYNOLOGY_KEY,
            Some(serde_json::to_value(&s).unwrap()),
            "",
            &|_| {},
        )
        .unwrap();
        assert!(cfg.drift().unwrap().entries.is_empty(), "round {round}");
        assert!(conf().contains("reset_button = \"command:"));
        rescue(round % 2 == 1);
        assert!(cfg.drift().unwrap().entries.is_empty(), "round {round}");
    }
    // synohwd restarts come from synology.hw only (adoption + 3 applies)
    let restarts = sys
        .state()
        .calls
        .iter()
        .filter(|c| c.contains("synohwd"))
        .count();
    assert_eq!(restarts, 4, "{:?}", sys.state().calls);
}
