//! PC backup schedule end to end (S6 of `docs/research/scheduling.md`): the real
//! `rynas-agent` service loop (`schedule-test`: the service with a stand-in for each run, which
//! reports a run and backs nothing up) against rynasd's agent routes served on 127.0.0.1, with
//! the scheduler's history and gate bound as `main.rs` binds them. An agent whose allowed
//! window is closed does not start: it reports one skipped start (`window`) to the NAS's
//! history; with the window open it starts at once and its run is in the history with its
//! trigger; Pause all reaches the plan an agent receives.
//!
//! Nothing here needs root or touches a disk (the btrfs folder is the memory host's). It needs
//! the agent binary, so it is `#[ignore]`d:
//!
//! ```sh
//! (cd crates/rynas-agent && cargo build)
//! RYNAS_AGENT_BIN=crates/rynas-agent/target/debug/rynas-agent \
//!   cargo test -p rynasd --test pc_agent_schedule_e2e -- --ignored --nocapture
//! ```
//!
//! `packaging/test-pcbackup-schedule.sh` does both.

use std::process::Command;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::pcagent::{AgentPlan, PairResponse, PairingCreated, PcDevice, PcDeviceSettings};
use rynas_api_types::scheduler::{PauseInfo, RunTrigger, SkipReason, TaskRun};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_notify::MemoryNotifier;
use rynas_scheduler::history::History;
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::ops::Ops;
use rynasd::pc_backup::{MemoryReceive, PcBackup};
use rynasd::roles::AdminPolicy;
use rynasd::server::{DaemonState, router};
use serde_json::{Value, json};
use tower::ServiceExt;

struct NoAuth;

impl Authenticator for NoAuth {
    fn realm(&self) -> &str {
        "test"
    }
    fn authenticate<'a>(&'a self, _: &'a str, _: &'a str) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Err(AuthError::InvalidCredentials) })
    }
}

fn groups(u: &str) -> Option<Vec<String>> {
    (u == "root" || u == "admin").then(|| vec!["rynas-admin".to_string()])
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri).header("x-rynas-user", "admin");
    if body.is_some() {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    let body = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
    let r = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec())
}

async fn wait_task(ops: &Ops, b: &[u8]) {
    let id = serde_json::from_slice::<rynas_api_types::TaskStarted>(b).unwrap().task_id;
    for _ in 0..500 {
        if ops.tasks.get(&id).is_some_and(|t| t.status.is_finished()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let t = ops.tasks.get(&id).unwrap();
    assert!(t.error.is_none(), "{:?}", t.error);
}

/// The window with the cells of this hour and the next one `on` (and the rest the other way):
/// the test may cross an hour.
fn window_now(on: bool) -> String {
    let tz = rynas_power::system_tz();
    let now = jiff::Timestamp::now().to_zoned(tz).datetime();
    let cell = rynas_schedule::window::cell_of(&now);
    (0..168)
        .map(|c| {
            let here = c == cell || c == (cell + 1) % 168;
            if here == on { '1' } else { '0' }
        })
        .collect()
}

/// `rynas-agent schedule-test --for <s>` with the state in `dir`: its output.
fn run_agent(bin: &str, dir: &std::path::Path, secs: u64) -> String {
    let out = Command::new(bin)
        .args(["schedule-test", "--for", &secs.to_string()])
        .env("RYNAS_AGENT_STATE", dir)
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    println!("agent ({}):\n{text}", out.status);
    assert!(out.status.success(), "{text}");
    text
}

#[tokio::test]
#[ignore = "needs the agent binary: RYNAS_AGENT_BIN=<rynas-agent>"]
async fn an_agent_with_a_window_starts_only_inside_it() {
    let bin = std::env::var("RYNAS_AGENT_BIN").expect("RYNAS_AGENT_BIN=<rynas-agent binary>");
    let dir = tempfile::tempdir().unwrap();
    let (root, state_dir) = (dir.path().join("root"), dir.path().join("state"));
    std::fs::create_dir_all(&root).unwrap();
    let config = Arc::new(Config::open(ConfigOptions::new(&state_dir, &root), Arc::new(MemorySystem::new("nas")), Vec::new()).unwrap());
    let mut ops = Ops::new(config, TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap());
    ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], groups);
    let ops = Arc::new(ops);
    let notifier = Arc::new(MemoryNotifier::default());
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(MemoryStorage::default(), clock.clone(), "rynas"));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    let svc = PcBackup::new(ops.clone(), Arc::new(MemoryReceive::new()), notifier.clone(), state_dir.clone(), AdminPolicy::new(vec!["rynas-admin".into()], groups), "nas".into());
    let agent = rynasd::pc_agent::PcAgent::new(
        ops.clone(),
        notifier,
        state_dir.clone(),
        true,
        rynas_backup::Launch::Direct { exe: "/nonexistent/rynas-backup-worker".into() },
        Arc::new(tokio::sync::Semaphore::new(1)),
        None,
        "nas".into(),
    );
    agent.bind_btrfs(svc.clone());
    // the scheduler's history and gate, as main.rs binds them
    let history = Arc::new(History::open(&state_dir.join("scheduler.db")).unwrap());
    let gate = Arc::new(rynasd::gate::Gate::new(state_dir.clone()));
    agent.bind_scheduler(history.clone(), gate.clone());
    st.pc_backup = Some(svc);
    st.pc_agent = Some(agent.clone());
    let app = router(st);

    // the PC's folder, its pairing (the btrfs method: no repository needed)
    let (s, b) = call(&app, "POST", "/v1/pc-backups/targets", Some(json!({"device": "laptop", "path": "/volume1/backup/laptop", "agent": true}))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    wait_task(&ops, &b).await;
    let (s, b) = call(&app, "POST", "/v1/pcbackup/pairings", Some(json!({"btrfs_target": "laptop"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let code = serde_json::from_slice::<PairingCreated>(&b).unwrap().code;
    let (s, b) = call(&app, "POST", "/v1/pcbackup/agent/pair", Some(json!({"code": code, "hostname": "laptop"}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let paired: PairResponse = serde_json::from_slice(&b).unwrap();
    // the plan carries the shared trigger and the NAS's zone, and still the old fields (R5)
    assert!(paired.plan.trigger.is_some());
    assert_eq!(paired.plan.plan.interval_hours, 24);

    let set = |window: String| {
        let app = app.clone();
        async move {
            let (s, b) = call(&app, "GET", "/v1/pcbackup/devices/laptop", None).await;
            assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
            let d: Value = serde_json::from_slice(&b).unwrap();
            let mut settings: PcDeviceSettings = serde_json::from_value(d["device"]["settings"].clone()).unwrap();
            settings.plan.interval_hours = 1;
            settings.plan.start_delay_min = 0;
            settings.plan.window = Some(window);
            let (s, b) = call(&app, "PUT", "/v1/pcbackup/devices/laptop", Some(serde_json::to_value(&settings).unwrap())).await;
            assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
            serde_json::from_slice::<PcDevice>(&b).unwrap()
        }
    };
    // an invalid window is refused
    let (s, _) = call(&app, "PUT", "/v1/pcbackup/devices/laptop", Some(json!({"plan": {"interval_hours": 1, "window": "1"}, "retention": {"last": 1}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // served as rynas-web would; the agent's state as a pairing over http leaves it
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let served = axum::Router::new().nest("/api", app.clone());
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let state = json!({"url": url, "fingerprint": "", "device": paired.device, "group": paired.group, "token": paired.token, "repo_password": "", "plan": paired.plan});
    std::fs::write(agent_dir.join("state.json"), state.to_string()).unwrap();
    let rows = || history.all_runs(Some("pc:laptop"), None, None, None, 100).unwrap();

    // 1. the window is closed now: a backup is due (never backed up, no delay) but does not
    //    start; one skipped start in the history
    set(window_now(false)).await;
    let (bin1, dir1) = (bin.clone(), agent_dir.clone());
    let out = tokio::task::spawn_blocking(move || run_agent(&bin1, &dir1, 12)).await.unwrap();
    assert!(out.contains("outside the allowed window"), "{out}");
    assert!(!out.contains("scheduled backup"), "{out}");
    let r: Vec<TaskRun> = rows();
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].skipped, Some(SkipReason::Window));
    assert!(agent.device("laptop").unwrap().last_ok_ms.is_none());

    // 2. the window open now: it starts at once; the run is in the history (trigger timer)
    let before = rynas_tasks::now_ms();
    set(window_now(true)).await;
    let (bin2, dir2) = (bin.clone(), agent_dir.clone());
    let out = tokio::task::spawn_blocking(move || run_agent(&bin2, &dir2, 12)).await.unwrap();
    assert!(out.contains("scheduled backup"), "{out}");
    let r: Vec<TaskRun> = rows();
    assert_eq!(r.len(), 2, "{r:?}");
    assert!(r[0].ok && r[0].skipped.is_none(), "{:?}", r[0]);
    assert_eq!(r[0].trigger, Some(RunTrigger::Timer));
    let started = r[0].start_ms.unwrap();
    assert!(started >= before);
    assert!(rynas_schedule::window::is_open(&window_now(true), &rynas_power::system_tz(), started));
    assert!(agent.device("laptop").unwrap().last_ok_ms.is_some());
    // not due again (every hour): nothing more
    let (bin3, dir3) = (bin.clone(), agent_dir.clone());
    tokio::task::spawn_blocking(move || run_agent(&bin3, &dir3, 6)).await.unwrap();
    assert_eq!(rows().len(), 2);

    // 3. Pause all on the NAS: the plan an agent receives is paused, with its own revision
    let plan_of = || {
        let app = app.clone();
        let token = paired.token.clone();
        async move {
            let req = Request::builder().uri("/v1/pcbackup/agent/plan").header(header::AUTHORIZATION, format!("Bearer {token}"));
            let r = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            serde_json::from_slice::<AgentPlan>(&to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap()
        }
    };
    let free = plan_of().await;
    assert!(!free.plan.paused);
    gate.set_pause_all(Some(PauseInfo { by: "admin".into(), ..Default::default() })).unwrap();
    let held = plan_of().await;
    assert!(held.plan.paused);
    assert_ne!(held.rev, free.rev);
    gate.set_pause_all(None).unwrap();
    assert_eq!(plan_of().await, free);
}
