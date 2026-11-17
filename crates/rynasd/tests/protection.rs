//! Scheduler and snapshot endpoints with the in-memory system, a fake systemd probe and a
//! scratch root: staging, previews, run now, the run history fed by the internal
//! `finished` route, and the notification of a failed run.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use rynas_api_types::notify::{EventState, NotifyCategory, NotifySeverity};
use rynas_api_types::scheduler::{CalendarPreview, TaskRun, TaskView};
use rynas_api_types::snapshots::SnapshotJobView;
use rynas_api_types::{ApiError, PendingChanges, error_codes};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_btrfs::{MemorySnapshots, SnapshotsDomain};
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_notify::MemoryNotifier;
use rynas_scheduler::SchedulerDomain;
use rynas_scheduler::history::History;
use rynas_scheduler::probe::FakeProbe;
use rynas_scheduler::units::Paths;
use rynas_services::{MemoryAccounts, MemoryShares, SharesDomain, UsersDomain};
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::ops::Ops;
use rynasd::protection::{Protection, RootBackend};
use rynasd::roles::AdminPolicy;
use rynasd::scheduler_live::Demo;
use rynasd::server::{DaemonState, PeerCred, router};
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
    _dir: tempfile::TempDir,
    app: axum::Router,
    sys: Arc<MemorySystem>,
    probe: Arc<FakeProbe>,
    notifier: Arc<MemoryNotifier>,
    config: Arc<Config>,
    /// For the watch loop's turns (`rynasd::gate::tick`) and the scheduler history.
    prot: Arc<Protection>,
}

fn env() -> Env {
    env_with(RootBackend::None, None)
}

fn env_with(root_ops: RootBackend, boot: Option<Arc<dyn rynas_btrfs::rootfs::BootSync>>) -> Env {
    env_inner(root_ops, boot, None)
}

/// Task runs scripted by `demo` (live output, as on `--dry-system`).
fn env_live(demo: Arc<Demo>) -> Env {
    env_inner(RootBackend::None, None, Some(demo))
}

fn env_inner(
    root_ops: RootBackend,
    boot: Option<Arc<dyn rynas_btrfs::rootfs::BootSync>>,
    demo: Option<Arc<Demo>>,
) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let acc = Arc::new(MemoryAccounts::new());
    let snap_host = Arc::new(MemorySnapshots::new());
    snap_host.add_subvolume("/srv/vol1/docs");
    snap_host.add_subvolume(dir.path().join("rootfs").to_str().unwrap());
    let snapshots = Arc::new(SnapshotsDomain::new(snap_host));
    let uid = |u: &str| (u == "alice").then_some(1001);
    let scheduler = Arc::new(SchedulerDomain::new(Paths::default(), Arc::new(uid)));
    let config = Arc::new(
        Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            sys.clone(),
            vec![
                Arc::new(UsersDomain::new(acc.clone())),
                Arc::new(SharesDomain::new(Arc::new(MemoryShares::new()), acc)),
                snapshots.clone(),
                scheduler.clone(),
            ],
        )
        .unwrap(),
    );
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let mut ops = Ops::new(config.clone(), tasks);
    // "admin" is an administrator, "bob" a plain user
    ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], |u| match u {
        "admin" => Some(vec!["rynas-admin".into()]),
        "bob" => Some(vec!["users".into()]),
        _ => None,
    });
    let ops = Arc::new(ops);
    let probe = Arc::new(FakeProbe::default());
    probe
        .state
        .lock()
        .unwrap()
        .invalid
        .push("Mon..Fry 25:00".into());
    let notifier = Arc::new(MemoryNotifier::default());
    let history = History::open(&dir.path().join("scheduler.db")).unwrap();
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(
        MemoryStorage::default(),
        clock.clone(),
        "rynas",
    ));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    let protection = Protection::new(
        ops,
        scheduler,
        snapshots,
        probe.clone(),
        Arc::new(history),
        notifier.clone(),
        dir.path().join("state"),
    )
    .with_root(root_ops)
    .with_boot(boot)
    .with_replicas({
        let r = Arc::new(rynas_btrfs::replication::MemoryReplicas::new());
        r.add_btrfs("/srv/vol1", "uuid-1");
        r.add_btrfs("/srv/vol2", "uuid-2");
        r.state.lock().unwrap().host_keys = vec!["backup.lan ssh-ed25519 AAAAC3NzaC1lZDI1NTE5".into()];
        r
    });
    let protection = match demo {
        Some(d) => protection.with_live_demo(d),
        None => protection,
    };
    let prot = Arc::new(protection);
    st.protection = Some(prot.clone());
    Env {
        _dir: dir,
        app: router(st),
        sys,
        probe,
        notifier,
        config,
        prot,
    }
}

async fn call(e: &Env, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    call_peer(e, None, method, uri, body).await
}

async fn call_as(
    e: &Env,
    user: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    request(e, user, None, method, uri, body).await
}

async fn call_peer(
    e: &Env,
    uid: Option<u32>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    request(e, "admin", uid, method, uri, body).await
}

/// A call from `user` over a connection whose peer has uid `uid` (`None`: no peer
/// credentials, like a router served without `rynasd::server::serve`).
async fn request(
    e: &Env,
    user: &str,
    uid: Option<u32>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-rynas-user", user)
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    if let Some(uid) = uid {
        req.extensions_mut()
            .insert(ConnectInfo(PeerCred { uid, pid: Some(1) }));
    }
    let r = e.app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 4 << 20).await.unwrap().to_vec())
}

fn code(b: &[u8]) -> String {
    serde_json::from_slice::<ApiError>(b).unwrap().code
}

fn apply(e: &Env) {
    e.config
        .apply("admin", "admin", "test", &|l| eprintln!("{l}"), &|| false)
        .unwrap();
}

#[tokio::test]
async fn tasks_are_staged_previewed_run_and_recorded() {
    let e = env();
    let hello = json!({"id": "hello", "action": {"kind": "script", "script": "#!/bin/sh\necho hi\nexit 3", "user": "alice"},
        "trigger": {"kind": "calendar", "cron": "0 4 * * *"}, "notify": "on_failure", "notify_output": true});
    let (s, b) = call(&e, "POST", "/v1/scheduler/tasks", Some(hello.clone())).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let v: TaskView = serde_json::from_slice(&b).unwrap();
    assert!(!v.status.applied);
    assert!(
        matches!(&v.task.trigger, rynas_api_types::scheduler::TaskTrigger::Calendar { on_calendar, .. } if on_calendar == "*-*-* 04:00:00")
    );
    let (s, b) = call(&e, "POST", "/v1/scheduler/tasks", Some(hello)).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, error_codes::ALREADY_EXISTS)
    );
    for bad in [
        json!({"id": "x", "action": {"kind": "command", "command": "true", "user": "mallory"}, "trigger": {"kind": "manual"}}),
        json!({"id": "y", "action": {"kind": "command", "command": "true"}, "trigger": {"kind": "calendar", "on_calendar": "Mon..Fry 25:00"}}),
        json!({"id": "Z", "action": {"kind": "command", "command": "true"}, "trigger": {"kind": "manual"}}),
        json!({"id": "s", "action": {"kind": "snapshot", "job": "nope"}, "trigger": {"kind": "manual"}}),
    ] {
        let (s, b) = call(&e, "POST", "/v1/scheduler/tasks", Some(bad.clone())).await;
        assert_eq!(
            (s, code(&b).as_str()),
            (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE),
            "{bad}"
        );
    }
    let (s, b) = call(
        &e,
        "POST",
        "/v1/scheduler/tasks/preview-calendar",
        Some(json!({"expression": "*/15 * * * *", "syntax": "cron"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let p: CalendarPreview = serde_json::from_slice(&b).unwrap();
    assert!(
        p.valid && p.on_calendar == "*-*-* *:00/15:00" && p.next_ms.len() == 5,
        "{p:?}"
    );
    let (_, b) = call(
        &e,
        "POST",
        "/v1/scheduler/tasks/preview-calendar",
        Some(json!({"expression": "Mon..Fry 25:00"})),
    )
    .await;
    assert!(!serde_json::from_slice::<CalendarPreview>(&b).unwrap().valid);
    // the schedule preview of the editor: rynas-schedule in a given zone
    let (s, b) = call(
        &e,
        "POST",
        "/v1/scheduler/preview",
        Some(json!({"expression": "Mon..Fri 02:30", "tz": "Europe/Rome", "count": 3})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let p: rynas_api_types::scheduler::SchedulePreview = serde_json::from_slice(&b).unwrap();
    assert!(p.valid && p.tz == "Europe/Rome" && p.runs.len() == 3, "{p:?}");
    assert_eq!(p.normalized, "Mon..Fri *-*-* 02:30:00");
    assert!(p.runs.iter().all(|r| r.local.ends_with(" 02:30:00")), "{p:?}");
    assert!(p.preset.is_some());
    let (_, b) = call(
        &e,
        "POST",
        "/v1/scheduler/preview",
        Some(json!({"trigger": {"kind": "recurrence", "recurrence": {"kind": "minutes", "every": 7, "from": 0, "to": 23, "days": 0}}})),
    )
    .await;
    let p: rynas_api_types::scheduler::SchedulePreview = serde_json::from_slice(&b).unwrap();
    assert!(!p.valid && p.error.is_some(), "{p:?}");
    // (the DST cases, which depend on the date, are tested with fixed dates in rynas-scheduler)
    // a trigger the Task Scheduler does not run yet is refused
    let (s, b) = call(
        &e,
        "POST",
        "/v1/scheduler/tasks",
        Some(json!({"id": "after-hello", "action": {"kind": "command", "command": "true"}, "trigger": {"kind": "after", "task": "hello"}})),
    )
    .await;
    assert_eq!((s, code(&b).as_str()), (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE));

    // run now needs the applied unit
    let (s, b) = call(&e, "POST", "/v1/scheduler/tasks/hello/run", None).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, error_codes::NOT_APPLIED)
    );
    apply(&e);
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/hello/run", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(
        e.sys
            .state()
            .calls
            .contains(&"run systemctl start --no-block rynas-task-hello.service".to_string())
    );

    // the unit ended: rynasctl task-finished reports it, as root only
    e.probe
        .state
        .lock()
        .unwrap()
        .journals
        .insert("abc123".into(), "hi\n".into());
    let done = json!({"invocation_id": "abc123", "result": "exit-code", "exit_code": "exited", "exit_status": "3"});
    for (peer, path) in [
        (None, "finished"),
        (Some(1000), "finished"),
        (Some(1000), "execute"),
        (None, "execute"),
    ] {
        let uri = format!("/v1/scheduler/tasks/hello/{path}");
        let (s, b) = call_peer(&e, peer, "POST", &uri, Some(done.clone())).await;
        assert_eq!(
            (s, code(&b).as_str()),
            (StatusCode::FORBIDDEN, error_codes::FORBIDDEN),
            "{path} from {peer:?}"
        );
    }
    let uri = "/v1/scheduler/tasks/hello/finished";
    let (s, _) = call_peer(&e, Some(0), "POST", uri, Some(done)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // root reaches execute (refused there: not an internal task)
    let (s, b) = call_peer(
        &e,
        Some(0),
        "POST",
        "/v1/scheduler/tasks/hello/execute",
        None,
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE)
    );
    let (s, _) = call_peer(
        &e,
        Some(0),
        "POST",
        "/v1/scheduler/tasks/hello/finished",
        Some(json!({"invocation_id": "../x", "result": "x"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let mut runs: Vec<TaskRun> = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (_, b) = call(&e, "GET", "/v1/scheduler/tasks/hello/history", None).await;
        runs = serde_json::from_slice(&b).unwrap();
        if runs.first().is_some_and(|r| r.output_bytes > 0) {
            break;
        }
    }
    assert_eq!(runs.len(), 1);
    assert_eq!((runs[0].exit_code, runs[0].ok), (Some(3), false));
    let (_, b) = call(
        &e,
        "GET",
        &format!("/v1/scheduler/tasks/hello/history/{}", runs[0].id),
        None,
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<TaskRun>(&b)
            .unwrap()
            .output
            .as_deref(),
        Some("hi\n")
    );
    for _ in 0..50 {
        if !e.notifier.0.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let ev = e.notifier.take();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].state, EventState::Firing);
    assert_eq!(ev[0].severity, NotifySeverity::Error);
    assert_eq!(ev[0].category, NotifyCategory::Task);
    assert_eq!(ev[0].key.as_deref(), Some("scheduler:task:hello"));
    assert_eq!(ev[0].body, "hi\n", "notify_output attaches the output");
    assert_eq!(ev[0].fields.get("exit_code").map(String::as_str), Some("3"));

    let (_, b) = call(&e, "GET", "/v1/scheduler/tasks", None).await;
    let list: Vec<TaskView> = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        list[0].status.last_run.as_ref().map(|r| r.id),
        Some(runs[0].id)
    );
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/hello/disable", None).await;
    assert_eq!(s, StatusCode::OK);
}

/// The scheduling gate (S2): pauses, skipped starts in the history, run now while paused,
/// a retry armed after a failed scheduled run, a planned command for the unit.
#[tokio::test]
async fn the_gate_skips_paused_starts_and_arms_retries() {
    use rynas_api_types::scheduler::{RunTrigger, SchedulerPause, SkipReason};
    use rynas_scheduler::gate::GateAnswer;
    let e = env();
    let t = json!({"id": "nightly", "name": "Nightly", "action": {"kind": "command", "command": "false"},
        "trigger": {"kind": "calendar", "on_calendar": "minutely"},
        "policy": {"retry": {"attempts": 2, "delay_s": 60, "backoff": false}}});
    let (s, b) = call(&e, "POST", "/v1/scheduler/tasks", Some(t)).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let smart = json!({"id": "smart-sda", "action": {"kind": "smart_test", "disk": "sda"}, "trigger": {"kind": "manual"}});
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks", Some(smart)).await;
    assert_eq!(s, StatusCode::CREATED);
    apply(&e);
    let gate = |inv: &str, timer: bool| {
        json!({"invocation_id": inv, "trigger_unit": if timer { "rynas-task-nightly.timer" } else { "" }})
    };
    let ask = async |inv: &str, timer: bool| -> GateAnswer {
        let (s, b) = call_peer(&e, Some(0), "POST", "/v1/scheduler/tasks/nightly/gate", Some(gate(inv, timer))).await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        serde_json::from_slice(&b).unwrap()
    };
    // root only, like the other internal routes
    let (s, _) = call_peer(&e, Some(1000), "POST", "/v1/scheduler/tasks/nightly/gate", Some(gate("aa", true))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // pause every task: a timer start is skipped and recorded, its end report adds nothing
    let (s, b) = call(&e, "POST", "/v1/scheduler/pause", Some(json!({"reason": "disk swap"}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let (_, b) = call(&e, "GET", "/v1/scheduler/pause", None).await;
    let p: SchedulerPause = serde_json::from_slice(&b).unwrap();
    assert_eq!(p.all.as_ref().map(|a| (a.reason.as_str(), a.by.as_str())), Some(("disk swap", "admin")));
    let a = ask("aa01", true).await;
    assert_eq!((a.run, a.skipped, a.trigger), (false, Some(SkipReason::PausedAll), RunTrigger::Timer));
    assert!(a.message.contains("paused"), "{}", a.message);
    let done = |inv: &str, result: &str| json!({"invocation_id": inv, "result": result, "exit_code": "exited", "exit_status": "1"});
    let (s, _) = call_peer(&e, Some(0), "POST", "/v1/scheduler/tasks/nightly/finished", Some(done("aa01", "exec-condition"))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, b) = call(&e, "GET", "/v1/scheduler/tasks/nightly/history", None).await;
    let runs: Vec<TaskRun> = serde_json::from_slice(&b).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!((runs[0].skipped, runs[0].result.as_str(), runs[0].ok), (Some(SkipReason::PausedAll), "skipped", false));
    assert_eq!(runs[0].detail.as_deref(), Some("disk swap"));

    // run now goes ahead while paused
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/nightly/run", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let a = ask("aa02", false).await;
    assert_eq!((a.run, a.trigger), (true, RunTrigger::Manual));
    let (s, _) = call(&e, "POST", "/v1/scheduler/resume", None).await;
    assert_eq!(s, StatusCode::OK);

    // the task's own pause, shown in its status
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/nightly/pause", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/nope/pause", Some(json!({}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, b) = call(&e, "POST", "/v1/scheduler/pause", Some(json!({"until_ms": 5}))).await;
    assert_eq!((s, code(&b).as_str()), (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE));
    let (_, b) = call(&e, "GET", "/v1/scheduler/tasks", None).await;
    let list: Vec<TaskView> = serde_json::from_slice(&b).unwrap();
    assert!(list.iter().find(|v| v.task.id == "nightly").unwrap().status.paused.is_some());
    assert_eq!(ask("aa03", true).await.skipped, Some(SkipReason::Paused));
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/nightly/resume", None).await;
    assert_eq!(s, StatusCode::OK);

    // a scheduled run that fails arms its retry, and its failure waits for the last attempt
    let a = ask("aa04", true).await;
    assert_eq!((a.run, a.trigger), (true, RunTrigger::Timer));
    e.notifier.take();
    let (s, _) = call_peer(&e, Some(0), "POST", "/v1/scheduler/tasks/nightly/finished", Some(done("aa04", "exit-code"))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, b) = call(&e, "GET", "/v1/scheduler/tasks", None).await;
    let list: Vec<TaskView> = serde_json::from_slice(&b).unwrap();
    let st = &list.iter().find(|v| v.task.id == "nightly").unwrap().status;
    let retry = st.retry.expect("retry armed");
    assert_eq!((retry.attempt, retry.attempts), (1, 2));
    assert_eq!(st.last_run.as_ref().and_then(|r| r.trigger), Some(RunTrigger::Timer));
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(e.notifier.take().is_empty(), "no notification while a retry is armed");

    // rynasd was away: rynasctl spooled a start without the gate and its end; the gate does
    // not import them (it must answer quickly), the watch loop's next turn does (and a
    // success drops the armed retry)
    let state = e._dir.path().join("state");
    let spool = |entry: Value| rynas_scheduler::gate::spool_append(&state, &serde_json::from_value(entry).unwrap()).unwrap();
    spool(json!({"task_id": "nightly", "invocation_id": "bb01", "at_ms": 1_000, "by_timer": true}));
    spool(json!({"task_id": "nightly", "invocation_id": "bb01", "at_ms": 2_000, "by_timer": false,
        "finished": {"result": "success", "exit_code": "exited", "exit_status": "0"}}));
    ask("aa05", true).await;
    assert!(e.prot.history.by_invocation("bb01").unwrap().is_none(), "imported by the gate");
    rynasd::gate::tick(&e.prot);
    let (_, b) = call(&e, "GET", "/v1/scheduler/tasks/nightly/history", None).await;
    let runs: Vec<TaskRun> = serde_json::from_slice(&b).unwrap();
    let away = runs.iter().find(|r| r.invocation_id == "bb01").expect("spooled run recorded");
    assert!(away.ok && away.ungated, "{away:?}");
    assert_eq!((away.start_ms, away.end_ms, away.trigger), (Some(1_000), 2_000, Some(RunTrigger::Timer)));
    let (_, b) = call(&e, "GET", "/v1/scheduler/tasks", None).await;
    let list: Vec<TaskView> = serde_json::from_slice(&b).unwrap();
    assert_eq!(list.iter().find(|v| v.task.id == "nightly").unwrap().status.retry, None);

    // a one-command internal action is planned for the unit to run itself
    let (s, b) = call_peer(&e, Some(0), "POST", "/v1/scheduler/tasks/smart-sda/execute", Some(json!({"plan": true}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let plan: rynasd::jobs::JobPlan = serde_json::from_slice(&b).unwrap();
    assert_eq!(plan.argv, ["smartctl", "-t", "short", "/dev/sda"]);
    // it shows in the task panel as before; cancelling it stops the unit, and the run's end
    // ends the entry
    let (_, b) = call(&e, "GET", "/v1/tasks", None).await;
    let tasks: Vec<rynas_api_types::TaskInfo> = serde_json::from_slice(&b).unwrap();
    let entry = tasks.iter().find(|t| t.kind == "smart.test").expect("in the task panel").id.clone();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (s, _) = call(&e, "POST", &format!("/v1/tasks/{entry}/cancel"), None).await;
    assert!(s.is_success(), "{s}");
    let mut stopped = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if e.sys.state().calls.iter().any(|c| c == "run systemctl stop --no-block rynas-task-smart-sda.service") {
            stopped = true;
            break;
        }
    }
    assert!(stopped, "{:?}", e.sys.state().calls);
    let end = json!({"invocation_id": "ff01", "result": "signal", "exit_code": "killed", "exit_status": "TERM"});
    let (s, _) = call_peer(&e, Some(0), "POST", "/v1/scheduler/tasks/smart-sda/finished", Some(end)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let mut status = None;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (_, b) = call(&e, "GET", &format!("/v1/tasks/{entry}"), None).await;
        let t: rynas_api_types::TaskInfo = serde_json::from_slice(&b).unwrap();
        if t.status.is_finished() {
            status = Some(t.status);
            break;
        }
    }
    assert_eq!(status, Some(rynas_api_types::TaskStatus::Cancelled));
}

/// A start of a unit rynas does not know (left behind) is skipped, not run: rynasd answered.
#[tokio::test]
async fn the_gate_skips_a_task_it_does_not_know() {
    let e = env();
    let a = gate_ask(&e, "ghost", "dd01", true, 0).await;
    assert!(!a.run, "{a:?}");
    assert!(e.prot.history.by_invocation("dd01").unwrap().is_none(), "no row");
}

/// The queue of S4: two scrubs of one volume go one at a time (the second waits, keeps its
/// place across questions and records how long it waited); a scrub gives way to more urgent
/// work on its volume and resumes from where it stopped once the volume is free; a long SMART
/// test waits for another one on its disk; an older rynasctl never waits; run now confirmed
/// by the user goes ahead; a pause skips a waiting start; a start past its deadline is skipped
/// as `busy`.
#[tokio::test]
async fn heavy_work_queues_per_volume_and_a_scrub_yields() {
    use rynas_api_types::scheduler::{RunConflicts, RunTrigger, SkipReason};
    let e = env();
    for t in [
        json!({"id": "scrub-a", "action": {"kind": "scrub", "filesystem": "/"}, "trigger": {"kind": "calendar", "on_calendar": "minutely"}}),
        json!({"id": "scrub-b", "action": {"kind": "scrub", "filesystem": "/"}, "trigger": {"kind": "calendar", "on_calendar": "minutely"}}),
        json!({"id": "smart-a", "action": {"kind": "smart_test", "disk": "sdzz", "test": "long"}, "trigger": {"kind": "manual"}}),
        json!({"id": "smart-b", "action": {"kind": "smart_test", "disk": "sdzz", "test": "long"}, "trigger": {"kind": "manual"}}),
        json!({"id": "smart-c", "action": {"kind": "smart_test", "disk": "sdzz", "test": "long"}, "trigger": {"kind": "manual"}}),
    ] {
        let (s, b) = call(&e, "POST", "/v1/scheduler/tasks", Some(t)).await;
        assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    }
    apply(&e);
    // the units of heavy work have no start timeout: their gate may wait
    let svc = std::fs::read_to_string(e._dir.path().join("root/etc/systemd/system/rynas-task-scrub-a.service")).unwrap();
    assert!(svc.contains("TimeoutStartSec=infinity\n"), "{svc}");
    let ask = async |id: &str, inv: &str, since: u64| {
        let unit = format!("rynas-task-{id}.timer");
        gate_ask_body(&e, id, json!({"invocation_id": inv, "trigger_unit": unit, "trigger_timer_ms": rynas_tasks::now_ms(),
            "can_wait": true, "queued_since_ms": since})).await
    };
    let view = async |id: &str| -> TaskView {
        let (_, b) = call(&e, "GET", &format!("/v1/scheduler/tasks/{id}"), None).await;
        serde_json::from_slice(&b).unwrap()
    };

    // one at a time on a volume
    let a = ask("scrub-a", "ab01", 0).await;
    assert!(a.run && !a.wait, "{a:?}");
    let b = ask("scrub-b", "ab02", 0).await;
    assert!(!b.run && b.wait && b.skipped.is_none(), "{b:?}");
    assert!(b.message.contains("task scrub-a on /"), "{}", b.message);
    let q = view("scrub-b").await.status.queued.expect("shown as queued");
    assert_eq!(q.blocker.as_ref().and_then(|x| x.task.as_deref()), Some("scrub-a"));
    let (_, body) = call(&e, "GET", "/v1/scheduler/queue", None).await;
    let queue: Vec<rynas_api_types::scheduler::QueueEntry> = serde_json::from_slice(&body).unwrap();
    assert_eq!(queue.iter().map(|x| x.task.as_str()).collect::<Vec<_>>(), ["scrub-b"]);
    // nothing is recorded while it waits; a run now would wait for the same work
    assert!(e.prot.history.by_invocation("ab02").unwrap().is_none());
    let (_, body) = call(&e, "GET", "/v1/scheduler/tasks/scrub-b/conflicts", None).await;
    let c: RunConflicts = serde_json::from_slice(&body).unwrap();
    assert_eq!(c.busy.iter().filter_map(|x| x.task.as_deref()).collect::<Vec<_>>(), ["scrub-a"]);
    // scrub-a ends: scrub-b's next question runs it, with its wait recorded
    gate_end(&e, "scrub-a", "ab01", "success").await;
    let since = rynas_tasks::now_ms() - 90_000;
    let b = ask("scrub-b", "ab02", since).await;
    assert!(b.run && !b.wait, "{b:?}");
    assert_eq!(e.prot.history.gate_note("ab02").unwrap().unwrap().queued_ms / 1000, 90);
    assert!(view("scrub-b").await.status.queued.is_none());

    // a scrub gives way to more urgent work on its volume (a backup run started by hand)...
    let guard = rynasd::fslock::FsLocks::global().heavy_paths(&["/"], "backup.run").await;
    // (never in the first seconds of a run: its rynasctl may not handle signals yet)
    rynasd::gate::tick(&e.prot);
    assert!(!e.sys.state().calls.iter().any(|c| c.contains("--signal=SIGINT")));
    tokio::time::sleep(Duration::from_millis(3200)).await;
    rynasd::gate::tick(&e.prot);
    let cancel = "run systemctl kill --kill-whom=all --signal=SIGINT rynas-task-scrub-b.service";
    assert!(e.sys.state().calls.iter().any(|c| c == cancel), "{:?}", e.sys.state().calls);
    let y = view("scrub-b").await.status.yielded.expect("giving way");
    assert!(!y.stopped);
    assert_eq!(y.to.as_ref().and_then(|t| t.kind.as_deref()), Some("backup.run"));
    let state = e._dir.path().join("state");
    assert!(rynas_scheduler::gate::gave_way(&state, "ab02"), "the unit can tell its end is no failure");
    // ...its end is recorded as such, no failure, nothing notified
    e.notifier.take();
    gate_end(&e, "scrub-b", "ab02", "success").await;
    let run = e.prot.history.by_invocation("ab02").unwrap().unwrap();
    assert!(run.yielded && run.ok && run.queued_ms >= 90_000, "{run:?}");
    assert!(view("scrub-b").await.status.yielded.unwrap().stopped);
    assert!(e.notifier.take().is_empty());
    // the volume is still busy: not resumed yet
    rynasd::gate::tick(&e.prot);
    let resume = "run systemctl start --no-block rynas-task-scrub-b.service";
    let started = |e: &Env| e.sys.state().calls.iter().any(|c| c.contains("rynas-task-scrub-b.service") && c.contains("start"));
    assert!(!started(&e), "{:?}", e.sys.state().calls);
    drop(guard);
    rynasd::gate::tick(&e.prot);
    assert!(started(&e), "{resume}: {:?}", e.sys.state().calls);
    // the start rynasd made is a resume, and the unit goes on from where it stopped
    let r = gate_ask_body(&e, "scrub-b", json!({"invocation_id": "ab03", "can_wait": true})).await;
    assert_eq!((r.run, r.trigger), (true, RunTrigger::Resume), "{r:?}");
    let (s, body) = call_peer(&e, Some(0), "POST", "/v1/scheduler/tasks/scrub-b/execute", Some(json!({"plan": true}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let plan: rynasd::jobs::JobPlan = serde_json::from_slice(&body).unwrap();
    assert_eq!(plan.argv, ["btrfs", "scrub", "resume", "-B", "-d", "/"]);
    assert_eq!(plan.ok_codes, [2]);
    // the resumed run holds a normal lease: no longer shown as given way, and it gives way
    // again when more urgent work comes
    assert!(view("scrub-b").await.status.yielded.is_none());
    let sigints = |e: &Env| e.sys.state().calls.iter().filter(|c| c.as_str() == cancel).count();
    let before = sigints(&e);
    let guard = rynasd::fslock::FsLocks::global().heavy_paths(&["/"], "backup.run").await;
    tokio::time::sleep(Duration::from_millis(3200)).await;
    rynasd::gate::tick(&e.prot);
    assert_eq!(sigints(&e), before + 1, "the resumed scrub gives way again");
    assert!(rynas_scheduler::gate::gave_way(&state, "ab03"));
    // its signal is not sent again at every turn (only after a while, until its run ends)
    rynasd::gate::tick(&e.prot);
    assert_eq!(sigints(&e), before + 1);
    gate_end(&e, "scrub-b", "ab03", "success").await;
    assert!(e.prot.history.by_invocation("ab03").unwrap().unwrap().yielded);
    drop(guard);
    rynasd::gate::tick(&e.prot);
    let r = gate_ask_body(&e, "scrub-b", json!({"invocation_id": "ab05", "can_wait": true})).await;
    assert_eq!((r.run, r.trigger), (true, RunTrigger::Resume), "{r:?}");
    // asked to give way before its command began (the signal came while rynasd planned the
    // run): the plan says so, nothing runs, the run gave way
    let guard = rynasd::fslock::FsLocks::global().heavy_paths(&["/"], "backup.run").await;
    tokio::time::sleep(Duration::from_millis(3200)).await;
    rynasd::gate::tick(&e.prot);
    let (s, body) = call_peer(&e, Some(0), "POST", "/v1/scheduler/tasks/scrub-b/execute", Some(json!({"plan": true}))).await;
    assert_eq!(s, StatusCode::OK);
    let plan: rynasd::jobs::JobPlan = serde_json::from_slice(&body).unwrap();
    assert!(plan.gave_way, "{plan:?}");
    gate_end(&e, "scrub-b", "ab05", "success").await;
    assert!(e.prot.history.by_invocation("ab05").unwrap().unwrap().yielded);
    drop(guard);
    rynasd::gate::tick(&e.prot);
    let r = gate_ask_body(&e, "scrub-b", json!({"invocation_id": "ab06", "can_wait": true})).await;
    assert_eq!(r.trigger, RunTrigger::Resume);
    let (_, body) = call_peer(&e, Some(0), "POST", "/v1/scheduler/tasks/scrub-b/execute", Some(json!({"plan": true}))).await;
    let plan: rynasd::jobs::JobPlan = serde_json::from_slice(&body).unwrap();
    assert_eq!(plan.argv[2], "resume");
    gate_end(&e, "scrub-b", "ab06", "success").await;
    assert!(view("scrub-b").await.status.yielded.is_none());
    // the entry cannot be written (the state folder is read-only): no signal, no yield
    let a = ask("scrub-a", "ab07", 0).await;
    assert!(a.run);
    let guard = rynasd::fslock::FsLocks::global().heavy_paths(&["/"], "backup.run").await;
    tokio::time::sleep(Duration::from_millis(3200)).await;
    use std::os::unix::fs::PermissionsExt;
    let sched = state.join("scheduler");
    std::fs::set_permissions(&sched, std::fs::Permissions::from_mode(0o555)).unwrap();
    let before = e.sys.state().calls.len();
    rynasd::gate::tick(&e.prot);
    let kills = e.sys.state().calls[before..].iter().filter(|c| c.contains("rynas-task-scrub-a.service") && c.contains("SIGINT")).count();
    std::fs::set_permissions(&sched, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(kills, 0, "no signal without its entry on disk");
    assert!(view("scrub-a").await.status.yielded.is_none());
    drop(guard);
    gate_end(&e, "scrub-a", "ab07", "success").await;
    let (s, body) = call_peer(&e, Some(0), "POST", "/v1/scheduler/tasks/scrub-a/execute", Some(json!({"plan": true}))).await;
    assert_eq!(s, StatusCode::OK);
    let plan: rynasd::jobs::JobPlan = serde_json::from_slice(&body).unwrap();
    assert_eq!(plan.argv[2], "start", "a scrub that never gave way starts");
    gate_end(&e, "scrub-a", "ab04", "success").await;

    // a disk: one long SMART test at a time; an older rynasctl (no can_wait) never waits
    let s1 = gate_ask_body(&e, "smart-a", json!({"invocation_id": "cd01", "can_wait": true})).await;
    assert!(s1.run);
    // (started by its timer: a start the user did not ask for, which a pause skips)
    let s2 = gate_ask_body(&e, "smart-b", json!({"invocation_id": "cd02", "can_wait": true,
        "trigger_unit": "rynas-task-smart-b.timer", "trigger_timer_ms": rynas_tasks::now_ms()})).await;
    assert!(s2.wait && s2.trigger == RunTrigger::Timer, "{s2:?}");
    let old = gate_ask_body(&e, "smart-c", json!({"invocation_id": "cd03"})).await;
    assert!(old.run && !old.wait);
    gate_end(&e, "smart-c", "cd03", "success").await;
    // a pause while it waits skips it
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/smart-b/pause", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    let s2 = gate_ask_body(&e, "smart-b", json!({"invocation_id": "cd02", "can_wait": true, "queued_since_ms": rynas_tasks::now_ms() - 5000})).await;
    assert_eq!((s2.run, s2.wait, s2.skipped), (false, false, Some(SkipReason::Paused)));
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/smart-b/resume", None).await;
    assert_eq!(s, StatusCode::OK);
    // past its deadline (a day) it is skipped as busy, and recorded
    let late = rynas_tasks::now_ms() - 25 * 3_600_000;
    let s2 = gate_ask_body(&e, "smart-b", json!({"invocation_id": "cd04", "can_wait": true, "queued_since_ms": late})).await;
    assert_eq!(s2.skipped, Some(SkipReason::Busy), "{s2:?}");
    assert_eq!(e.prot.history.by_invocation("cd04").unwrap().unwrap().skipped, Some(SkipReason::Busy));
    // run now confirmed by the user goes ahead of the queue; without, it waits like the rest
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/smart-b/run", Some(json!({"skip_queue": false}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let s2 = gate_ask_body(&e, "smart-b", json!({"invocation_id": "cd05", "can_wait": true})).await;
    assert!(s2.wait && s2.trigger == RunTrigger::Manual, "{s2:?}");
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/smart-b/run", Some(json!({"skip_queue": true}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let s2 = gate_ask_body(&e, "smart-b", json!({"invocation_id": "cd05", "can_wait": true})).await;
    assert!(s2.run && s2.trigger == RunTrigger::Manual, "{s2:?}");
    gate_end(&e, "smart-b", "cd05", "success").await;
    // a start stopped while it waits (its gate exits 0 so the unit never fails): no row
    let s3 = gate_ask_body(&e, "smart-c", json!({"invocation_id": "cd06", "can_wait": true})).await;
    assert!(s3.wait, "{s3:?}");
    gate_end(&e, "smart-c", "cd06", "success").await;
    assert!(e.prot.history.by_invocation("cd06").unwrap().is_none());
    assert!(view("smart-c").await.status.queued.is_none());
    gate_end(&e, "smart-a", "cd01", "success").await;
    // the long test goes on in the drive after its unit ended: the disk stays claimed (the
    // drive says nothing here: its time is estimated)
    let s4 = gate_ask_body(&e, "smart-c", json!({"invocation_id": "cd07", "can_wait": true})).await;
    assert!(s4.wait && s4.message.contains("smart-a"), "{s4:?}");
    // a start that waits keeps waiting on a late answer (rynasctl asks again)...
    let since = rynas_tasks::now_ms() - 10_000;
    let s4 = gate_ask_body(&e, "smart-c", json!({"invocation_id": "cd07", "can_wait": true, "queued_since_ms": since, "answer_by_ms": 1})).await;
    assert!(s4.wait, "{s4:?}");
    // ...is skipped once its task is turned off...
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/smart-c/disable", None).await;
    assert!(s.is_success());
    apply(&e);
    let s4 = gate_ask_body(&e, "smart-c", json!({"invocation_id": "cd07", "can_wait": true, "queued_since_ms": since})).await;
    assert!(!s4.run && !s4.wait, "{s4:?}");
    assert!(s4.message.contains("turned off"), "{}", s4.message);
    // ...and after a restart of rynasd (rynasd no longer knows it waits) keeps the origin
    // rynasctl sends back: a timer start, which a pause skips
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/smart-b/pause", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    let s5 = gate_ask_body(&e, "smart-b", json!({"invocation_id": "cd08", "can_wait": true, "queued_since_ms": since,
        "queued_trigger": "timer", "queued_attempt": 0})).await;
    assert_eq!((s5.run, s5.skipped, s5.trigger), (false, Some(SkipReason::Paused), RunTrigger::Timer), "{s5:?}");
}

/// Debian's own timers under the gate (S4): a start of `apt-daily-upgrade.service` by its
/// timer is skipped while every task is paused and recorded as the system task's; started
/// by hand it runs; its end is recorded in the same history.
#[tokio::test]
async fn the_system_timers_are_gated() {
    use rynas_api_types::scheduler::{RunTrigger, SkipReason};
    let e = env();
    let ask = async |inv: &str, timer: bool| {
        let unit = if timer { "apt-daily-upgrade.timer" } else { "" };
        gate_ask_body(&e, "apt-upgrade", json!({"invocation_id": inv, "trigger_unit": unit,
            "trigger_timer_ms": if timer { rynas_tasks::now_ms() } else { 0 }, "can_wait": true})).await
    };
    let a = ask("ee01", true).await;
    assert!(a.run, "{a:?}");
    gate_end(&e, "apt-upgrade", "ee01", "success").await;
    let (s, _) = call(&e, "POST", "/v1/scheduler/pause", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    let a = ask("ee02", true).await;
    // (a timer start away from the daily time: a catch-up)
    assert_eq!((a.run, a.skipped, a.trigger), (false, Some(SkipReason::PausedAll), RunTrigger::CatchUp));
    let a = ask("ee03", false).await;
    assert_eq!((a.run, a.trigger), (true, RunTrigger::Other));
    gate_end(&e, "apt-upgrade", "ee03", "exit-code").await;
    let (_, b) = call(&e, "GET", "/v1/scheduler/tasks/apt-upgrade/history", None).await;
    let runs: Vec<TaskRun> = serde_json::from_slice(&b).unwrap();
    let got: Vec<(&str, bool, Option<SkipReason>)> = runs.iter().map(|r| (r.invocation_id.as_str(), r.ok, r.skipped)).collect();
    assert_eq!(got, [("ee03", false, None), ("ee02", false, Some(SkipReason::PausedAll)), ("ee01", true, None)]);
    // never retried (the system's timer runs it again), never notified (notify never)
    assert!(e.prot.history.retry("apt-upgrade").unwrap().is_none());
    // the md RAID check is known too, a unit the gate never heard of is not
    assert!(ask_unknown(&e, "mdcheck-start").await);
    assert!(!ask_unknown(&e, "mdcheck-other").await);
}

/// The gate knows the task `id` (it does not answer "not in the configuration").
async fn ask_unknown(e: &Env, id: &str) -> bool {
    let a = gate_ask_body(e, id, json!({"invocation_id": "ff99", "can_wait": true})).await;
    !a.message.contains("not in the configuration")
}

/// The gate's answer for a start of `id` (`timer`: started by its own timer); `by`: the
/// deadline rynasctl sends (0: none).
async fn gate_ask(e: &Env, id: &str, inv: &str, timer: bool, by: u64) -> rynas_scheduler::gate::GateAnswer {
    // a timer start: the timer elapsed just now
    let (unit, elapsed) = if timer { (format!("rynas-task-{id}.timer"), rynas_tasks::now_ms()) } else { (String::new(), 0) };
    gate_ask_body(e, id, json!({"invocation_id": inv, "trigger_unit": unit, "answer_by_ms": by, "trigger_timer_ms": elapsed})).await
}

async fn gate_ask_body(e: &Env, id: &str, body: Value) -> rynas_scheduler::gate::GateAnswer {
    let (s, b) = call_peer(e, Some(0), "POST", &format!("/v1/scheduler/tasks/{id}/gate"), Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    serde_json::from_slice(&b).unwrap()
}

/// `task-finished` of run `inv` of `id` with systemd's `result`, then a wait for the output
/// and the notification (they follow 500 ms later).
async fn gate_end(e: &Env, id: &str, inv: &str, result: &str) {
    let status = if result == "success" { "0" } else { "1" };
    let body = json!({"invocation_id": inv, "result": result, "exit_code": "exited", "exit_status": status});
    let (s, _) = call_peer(e, Some(0), "POST", &format!("/v1/scheduler/tasks/{id}/finished"), Some(body)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    tokio::time::sleep(Duration::from_millis(800)).await;
}

/// The armed retry of `id` falls due now.
fn retry_due_now(e: &Env, id: &str) {
    let a = e.prot.history.retry(id).unwrap().expect("retry armed");
    e.prot
        .history
        .set_retry(id, &rynas_scheduler::history::ArmedRetry { retry: rynas_api_types::scheduler::PendingRetry { due_ms: 0, ..a.retry }, ..a })
        .unwrap();
}

fn failures(events: &[rynas_api_types::notify::NewEvent], id: &str) -> usize {
    events
        .iter()
        .filter(|ev| ev.key.as_deref() == Some(&format!("scheduler:task:{id}")) && ev.state == EventState::Firing)
        .count()
}

/// A retry chain notifies its failure exactly once: at its last attempt, or when it is
/// dropped; a newer scheduled failure keeps its count; a run by hand notifies as always.
#[tokio::test]
async fn a_retry_chain_notifies_its_failure_exactly_once() {
    use rynas_api_types::scheduler::RunTrigger;
    let e = env();
    for id in ["chain", "off", "quiet", "gone"] {
        let t = json!({"id": id, "name": id, "action": {"kind": "command", "command": "false"},
            "trigger": {"kind": "calendar", "on_calendar": "minutely"}, "notify": "on_failure",
            "policy": {"retry": {"attempts": 2, "delay_s": 60, "backoff": false}}});
        let (s, b) = call(&e, "POST", "/v1/scheduler/tasks", Some(t)).await;
        assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    }
    apply(&e);
    rynasd::gate::tick(&e.prot);
    e.notifier.take();

    // a scheduled failure arms retry 1 of 2 and says nothing
    assert!(gate_ask(&e, "chain", "c001", true, 0).await.run);
    gate_end(&e, "chain", "c001", "exit-code").await;
    assert_eq!(e.prot.history.retry("chain").unwrap().unwrap().retry.attempt, 1);
    assert_eq!(failures(&e.notifier.take(), "chain"), 0);
    // a newer scheduled failure does not start the count again
    assert!(gate_ask(&e, "chain", "c002", true, 0).await.run);
    gate_end(&e, "chain", "c002", "exit-code").await;
    assert_eq!(e.prot.history.retry("chain").unwrap().unwrap().retry.attempt, 1);
    // run now fails while the retry waits: notified as always, the chain goes on
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/chain/run", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let a = gate_ask(&e, "chain", "c003", false, 0).await;
    assert_eq!((a.run, a.trigger), (true, RunTrigger::Manual));
    gate_end(&e, "chain", "c003", "exit-code").await;
    assert_eq!(failures(&e.notifier.take(), "chain"), 1);
    assert_eq!(e.prot.history.retry("chain").unwrap().unwrap().retry.attempt, 1);
    // retry 1 fails: retry 2 armed, still silent
    retry_due_now(&e, "chain");
    rynasd::gate::tick(&e.prot);
    assert!(e.sys.state().calls.iter().any(|c| c == "run systemctl start --no-block rynas-task-chain.service"));
    // systemd keeps the activation details of the last timer start: the retry still names
    // the timer, with its old elapse time
    let stale = json!({"invocation_id": "c004", "trigger_unit": "rynas-task-chain.timer",
        "trigger_timer_ms": rynas_tasks::now_ms() - 30_000});
    let a = gate_ask_body(&e, "chain", stale).await;
    assert_eq!((a.run, a.trigger), (true, RunTrigger::Retry));
    gate_end(&e, "chain", "c004", "exit-code").await;
    assert_eq!(e.prot.history.retry("chain").unwrap().unwrap().retry.attempt, 2);
    assert_eq!(failures(&e.notifier.take(), "chain"), 0);
    // the last attempt fails: one notification, nothing armed, nothing more later
    retry_due_now(&e, "chain");
    rynasd::gate::tick(&e.prot);
    let a = gate_ask(&e, "chain", "c005", false, 0).await;
    assert_eq!((a.trigger, a.message.contains("retry 2 of 2")), (RunTrigger::Retry, true), "{}", a.message);
    gate_end(&e, "chain", "c005", "exit-code").await;
    assert!(e.prot.history.retry("chain").unwrap().is_none());
    rynasd::gate::tick(&e.prot);
    assert_eq!(failures(&e.notifier.take(), "chain"), 1);
    let runs = e.prot.history.list("chain", 10, None).unwrap();
    assert_eq!(runs[0].attempt, 2);

    // chains dropped before their end send the held failure once: task off, paused at the
    // due time, deleted (its pause goes too)
    // invocation ids are hex: the task's name in hex and a number
    let inv = |id: &str, n: u8| format!("{}{n:02}", id.bytes().map(|b| format!("{b:02x}")).collect::<String>());
    for id in ["off", "quiet", "gone"] {
        assert!(gate_ask(&e, id, &inv(id, 1), true, 0).await.run);
        gate_end(&e, id, &inv(id, 1), "exit-code").await;
    }
    assert_eq!(e.notifier.take().len(), 0);
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/off/disable", None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/quiet/pause", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/gone/pause", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&e, "DELETE", "/v1/scheduler/tasks/gone", None).await;
    assert_eq!(s, StatusCode::OK);
    apply(&e);
    retry_due_now(&e, "quiet");
    rynasd::gate::tick(&e.prot);
    rynasd::gate::tick(&e.prot);
    let ev = e.notifier.take();
    for id in ["off", "quiet", "gone"] {
        assert_eq!(failures(&ev, id), 1, "{id}: {ev:?}");
        assert!(e.prot.history.retry(id).unwrap().is_none(), "{id}");
    }
    let quiet = e.prot.history.list("quiet", 1, None).unwrap();
    assert_eq!((quiet[0].trigger, quiet[0].skipped), (Some(RunTrigger::Retry), Some(rynas_api_types::scheduler::SkipReason::Paused)));
    let (_, b) = call(&e, "GET", "/v1/scheduler/pause", None).await;
    let p: rynas_api_types::scheduler::SchedulerPause = serde_json::from_slice(&b).unwrap();
    assert!(!p.tasks.contains_key("gone") && p.tasks.contains_key("quiet"), "{p:?}");
    // a retry whose start the gate skips ends the chain too
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/quiet/resume", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(gate_ask(&e, "quiet", &inv("quiet", 2), true, 0).await.run);
    gate_end(&e, "quiet", &inv("quiet", 2), "exit-code").await;
    retry_due_now(&e, "quiet");
    rynasd::gate::tick(&e.prot);
    let (s, _) = call(&e, "POST", "/v1/scheduler/pause", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    let a = gate_ask(&e, "quiet", &inv("quiet", 3), false, 0).await;
    assert_eq!((a.run, a.trigger), (false, RunTrigger::Retry));
    assert_eq!(failures(&e.notifier.take(), "quiet"), 1);
    assert!(e.prot.history.retry("quiet").unwrap().is_none());
}

/// A stale request, a late answer, the default overlap of new tasks, a spooled end imported
/// twice.
#[tokio::test]
async fn the_gate_resists_stale_requests_and_late_answers() {
    use rynas_api_types::scheduler::{Overlap, RunTrigger, SkipReason};
    let e = env();
    let t = json!({"id": "night", "action": {"kind": "command", "command": "false"},
        "trigger": {"kind": "calendar", "on_calendar": "minutely"}, "notify": "on_failure"});
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks", Some(t)).await;
    assert_eq!(s, StatusCode::CREATED);
    apply(&e);
    // a task created now skips overlapping starts; it is stored, not a default
    let (_, b) = call(&e, "GET", "/v1/scheduler/tasks/night", None).await;
    let v: TaskView = serde_json::from_slice(&b).unwrap();
    assert_eq!(v.task.policy.overlap, Some(Overlap::Skip));

    // run now on a running task leaves no request; nor does any request take over a start
    // of the timer
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/night/pause", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    {
        let mut st = e.probe.state.lock().unwrap();
        st.running.push("rynas-task-night.service".into());
        st.invocations.insert("rynas-task-night.service".into(), "ab00".into());
    }
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/night/run", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    e.probe.state.lock().unwrap().running.clear();
    assert_eq!(gate_ask(&e, "night", "ab01", false, 0).await.trigger, RunTrigger::Other);
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/night/run", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let a = gate_ask(&e, "night", "ab02", true, 0).await;
    assert_eq!((a.run, a.skipped, a.trigger), (false, Some(SkipReason::Paused), RunTrigger::Timer));
    assert_eq!(gate_ask(&e, "night", "ab03", false, 0).await.trigger, RunTrigger::Other, "the request was used");

    // an answer decided after rynasctl's deadline runs the start and records no skip
    let a = gate_ask(&e, "night", "ab04", true, 1).await;
    assert_eq!((a.run, a.skipped), (true, None));
    assert!(e.prot.history.by_invocation("ab04").unwrap().is_none());
    // a skip recorded in time, then the run happened anyway (rynasctl gave up waiting): the
    // run's end replaces the skip
    assert!(!gate_ask(&e, "night", "ab05", true, 0).await.run);
    gate_end(&e, "night", "ab05", "exit-code").await;
    let r = e.prot.history.by_invocation("ab05").unwrap().unwrap();
    assert_eq!((r.skipped, r.result.as_str()), (None, "exit-code"));
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/night/resume", None).await;
    assert_eq!(s, StatusCode::OK);

    // a spooled end imported twice is recorded and notified once; a skipped start before it
    // is not a failure before it
    e.notifier.take();
    let state = e._dir.path().join("state");
    let end = json!({"task_id": "night", "invocation_id": "ab06", "at_ms": 5_000, "by_timer": false,
        "finished": {"result": "exit-code", "exit_code": "exited", "exit_status": "1"}});
    for _ in 0..2 {
        rynas_scheduler::gate::spool_append(&state, &serde_json::from_value(end.clone()).unwrap()).unwrap();
    }
    rynasd::gate::tick(&e.prot);
    rynas_scheduler::gate::spool_append(&state, &serde_json::from_value(end).unwrap()).unwrap();
    rynasd::gate::tick(&e.prot);
    let runs = e.prot.history.list("night", 50, None).unwrap();
    assert_eq!(runs.iter().filter(|r| r.invocation_id == "ab06").count(), 1);
    assert_eq!(failures(&e.notifier.take(), "night"), 1);
}

#[tokio::test]
async fn snapshot_jobs_stage_their_task() {
    let e = env();
    let (s, b) = call(&e, "POST", "/v1/snapshots/jobs",
        Some(json!({"name": "docs", "subvolume": "/srv/vol1/docs", "schedule": {"kind": "calendar", "on_calendar": "daily"}}))).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let v: SnapshotJobView = serde_json::from_slice(&b).unwrap();
    assert_eq!(v.task_id, "snapshot-docs");
    assert_eq!(v.job.snapshot_dir, "/srv/vol1/.snapshots/docs");
    assert!(v.enabled);
    let (_, b) = call(&e, "GET", "/v1/config/pending", None).await;
    let keys: Vec<String> = serde_json::from_slice::<PendingChanges>(&b)
        .unwrap()
        .changes
        .into_iter()
        .map(|c| c.key)
        .collect();
    assert_eq!(
        keys,
        ["scheduler", "snapshots"],
        "no share: shares untouched"
    );
    let (s, b) = call(
        &e,
        "POST",
        "/v1/snapshots/jobs",
        Some(json!({"name": "x", "share": "nope"})),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE)
    );
    apply(&e);
    // the task of a job goes with the job
    let (s, b) = call(&e, "DELETE", "/v1/scheduler/tasks/snapshot-docs", None).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, error_codes::CONFLICT)
    );
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/snapshots/jobs/docs",
        Some(json!({"subvolume": "/srv/vol1/docs", "enabled": false})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert!(
        !serde_json::from_slice::<SnapshotJobView>(&b)
            .unwrap()
            .enabled
    );
    // listing reads the (missing) snapshot directory: empty, not an error
    let (s, b) = call(&e, "GET", "/v1/snapshots?job=docs", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, b"[]");
    let (s, _) = call(&e, "GET", "/v1/snapshots/docs.20261105T1200/browse", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, b) = call(&e, "DELETE", "/v1/snapshots/docs.20261105T1200", None).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::CONFIRMATION_REQUIRED)
    );
    let (s, b) = call(
        &e,
        "POST",
        "/v1/snapshots/docs.20261105T1200/rollback",
        Some(json!({"confirm": "x"})),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::CONFIRMATION_REQUIRED)
    );
    let (s, b) = call(&e, "DELETE", "/v1/snapshots/jobs/docs", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    apply(&e);
    let (_, b) = call(&e, "GET", "/v1/snapshots/jobs", None).await;
    assert_eq!(b, b"[]");
    let (_, b) = call(&e, "GET", "/v1/scheduler/tasks", None).await;
    assert_eq!(b, b"[]");
}

#[tokio::test]
async fn root_tasks_are_for_administrators() {
    let e = env();
    let root_task = json!({"id": "cleanup", "action": {"kind": "command", "command": "true"},
        "trigger": {"kind": "manual"}});
    let user_task = json!({"id": "mine", "action": {"kind": "command", "command": "true", "user": "alice"},
        "trigger": {"kind": "manual"}});
    let forbidden = |(s, b): (StatusCode, Vec<u8>)| {
        assert_eq!(
            (s, code(&b).as_str()),
            (StatusCode::FORBIDDEN, error_codes::FORBIDDEN)
        )
    };
    forbidden(
        call_as(
            &e,
            "bob",
            "POST",
            "/v1/scheduler/tasks",
            Some(root_task.clone()),
        )
        .await,
    );
    forbidden(
        call_as(
            &e,
            "ghost",
            "POST",
            "/v1/scheduler/tasks",
            Some(root_task.clone()),
        )
        .await,
    );
    let (s, b) = call_as(
        &e,
        "bob",
        "POST",
        "/v1/scheduler/tasks",
        Some(user_task.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    // bob cannot turn his task into a root one
    let mut to_root = user_task.clone();
    to_root["action"]["user"] = json!("root");
    forbidden(call_as(&e, "bob", "PUT", "/v1/scheduler/tasks/mine", Some(to_root)).await);

    let (s, b) = call(&e, "POST", "/v1/scheduler/tasks", Some(root_task.clone())).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    apply(&e);
    // bob drops his stale changeset and starts again from the applied root task
    let (s, _) = call_as(&e, "bob", "POST", "/v1/config/discard", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    forbidden(
        call_as(
            &e,
            "bob",
            "POST",
            "/v1/scheduler/tasks/cleanup/disable",
            None,
        )
        .await,
    );
    forbidden(call_as(&e, "bob", "DELETE", "/v1/scheduler/tasks/cleanup", None).await);
    forbidden(
        call_as(
            &e,
            "bob",
            "PUT",
            "/v1/scheduler/tasks/cleanup",
            Some(root_task),
        )
        .await,
    );
    forbidden(call_as(&e, "bob", "POST", "/v1/scheduler/tasks/cleanup/run", None).await);
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/cleanup/run", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/cleanup/disable", None).await;
    assert_eq!(s, StatusCode::OK);
}

/// A whole `scheduler` value that brings back, changes or drops a root task — a restored
/// revision, an imported archive, any staged changeset — needs an administrator too.
#[tokio::test]
async fn restore_and_apply_check_root_tasks() {
    let e = env();
    let forbidden = |(s, b): (StatusCode, Vec<u8>)| {
        assert_eq!(
            (s, code(&b).as_str()),
            (StatusCode::FORBIDDEN, error_codes::FORBIDDEN),
            "{}",
            String::from_utf8_lossy(&b)
        )
    };
    let rev = |e: &Env, who: &str| {
        e.config
            .apply(who, who, "test", &|l| eprintln!("{l}"), &|| false)
            .unwrap()
    };
    let tasks_applied = |e: &Env| e.config.applied("scheduler").unwrap().unwrap()["tasks"].clone();
    let root_task = json!({"id": "cleanup", "action": {"kind": "command", "command": "true"},
        "trigger": {"kind": "manual"}});
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks", Some(root_task)).await;
    assert_eq!(s, StatusCode::CREATED);
    let with_root = rev(&e, "admin");
    let (s, _) = call(&e, "DELETE", "/v1/scheduler/tasks/cleanup", None).await;
    assert_eq!(s, StatusCode::OK);
    let without_root = rev(&e, "admin");

    // bob cannot bring the root task back by restoring the revision that had it
    let restore = |n: u64| format!("/v1/config/revisions/{n}/restore");
    forbidden(call_as(&e, "bob", "POST", &restore(with_root), None).await);
    assert!(e.config.pending("bob").unwrap().changes.is_empty());

    // nor by applying a changeset that has it, however it was staged (here: directly, like
    // `rynasd import --owner bob` or a drift import would)
    let doc = e.config.revision_snapshot(with_root).unwrap()["scheduler"].clone();
    assert!(e.config.stage("bob", "scheduler", Some(doc)).unwrap());
    let apply_body = Some(json!({"comment": ""}));
    forbidden(call_as(&e, "bob", "POST", "/v1/config/apply", apply_body.clone()).await);
    assert_eq!(tasks_applied(&e), json!([]));
    let (s, _) = call_as(&e, "bob", "POST", "/v1/config/discard", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // changes without root tasks stay open to bob: his own task, applied through the API,
    // then a restore that only drops it
    let mine = json!({"id": "mine", "action": {"kind": "command", "command": "true", "user": "alice"},
        "trigger": {"kind": "manual"}});
    let (s, _) = call_as(&e, "bob", "POST", "/v1/scheduler/tasks", Some(mine)).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, b) = call_as(&e, "bob", "POST", "/v1/config/apply", apply_body.clone()).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    // the apply task runs in the background
    for _ in 0..200 {
        if tasks_applied(&e).as_array().is_some_and(|t| t.len() == 1) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(tasks_applied(&e)[0]["id"], "mine");
    let (s, b) = call_as(&e, "bob", "POST", &restore(without_root), None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(e.config.pending("bob").unwrap().changes.len(), 1);
    let (s, _) = call_as(&e, "bob", "POST", "/v1/config/discard", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // an administrator restores the root task and applies it
    let (s, b) = call(&e, "POST", &restore(with_root), None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let (s, b) = call(&e, "POST", "/v1/config/apply", apply_body).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
}

/// A system restore point with a kernel and its modules (a plain directory: the scan calls
/// it not read-only, so a real rollback is refused; the order of the steps is tested in
/// `rynas_btrfs::rootfs`).
async fn restore_point(e: &Env) -> String {
    let base = e._dir.path().join("rootfs");
    let dir = base.join(".snapshots/rootfs");
    let id = "rootfs.20261101T0300";
    let p = dir.join(id);
    std::fs::create_dir_all(p.join("boot")).unwrap();
    std::fs::write(p.join("boot/vmlinuz-7.3.0-1-synodeb"), "").unwrap();
    std::fs::create_dir_all(p.join("usr/lib/modules/7.3.0-1-synodeb")).unwrap();
    let body = json!({"enabled": true, "subvolume": base, "snapshot_dir": dir, "prefix": "rootfs", "keep": 5});
    let (s, b) = call(e, "PUT", "/v1/snapshots/system", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    apply(e);
    id.into()
}

#[tokio::test]
async fn root_rollback_needs_a_boot_layer_and_a_bootable_restore_point() {
    use rynas_api_types::snapshots::{RollbackPreview, RollbackRefusal};
    // no boot layer: previewed and refused with a clear reason
    let e = env();
    let id = restore_point(&e).await;
    let (s, b) = call(
        &e,
        "GET",
        &format!("/v1/snapshots/{id}/preview-rollback"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let pv: RollbackPreview = serde_json::from_slice(&b).unwrap();
    assert!(pv.system && !pv.allowed);
    assert_eq!(pv.refusal, Some(RollbackRefusal::NoBootLayer));
    assert_eq!(pv.kernel.as_deref(), Some("7.3.0-1-synodeb"));
    assert!(pv.modules);
    let (s, b) = call(
        &e,
        "POST",
        &format!("/v1/snapshots/{id}/rollback"),
        Some(json!({"confirm": id})),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, error_codes::CONFLICT)
    );
    assert!(String::from_utf8_lossy(&b).contains("console procedure"));
    // a boot layer (dry): the plain directory is not a read-only restore point
    struct Boot;
    impl rynas_btrfs::rootfs::BootSync for Boot {
        fn tool(&self) -> &str {
            "boot-tool"
        }
        fn sync_root(&self, _: &std::path::Path) -> Result<String, String> {
            Ok(String::new())
        }
        fn undo_hint(&self) -> String {
            String::new()
        }
    }
    let e = env_with(
        RootBackend::Custom(Arc::new(rynas_btrfs::rootfs::DryRoot::new())),
        Some(Arc::new(Boot)),
    );
    let id = restore_point(&e).await;
    let (_, b) = call(
        &e,
        "GET",
        &format!("/v1/snapshots/{id}/preview-rollback"),
        None,
    )
    .await;
    let pv: RollbackPreview = serde_json::from_slice(&b).unwrap();
    assert_eq!(pv.refusal, Some(RollbackRefusal::NotReadonly));
    // the typed confirmation comes first
    let (s, b) = call(
        &e,
        "POST",
        &format!("/v1/snapshots/{id}/rollback"),
        Some(json!({"confirm": "rootfs"})),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::CONFIRMATION_REQUIRED)
    );
    let (s, _) = call(
        &e,
        "GET",
        "/v1/snapshots/rootfs.20261102T0300/preview-rollback",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn replica_targets_stage_with_their_job() {
    use rynas_api_types::replication::{HostKey, ReplicaView, ReplicaWarning, ReplicationKey};
    let e = env();
    let (s, b) = call(&e, "POST", "/v1/snapshots/jobs", Some(json!({"name": "docs", "subvolume": "/srv/vol1/docs"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    apply(&e);
    let local = |path: &str| json!({"job": "docs", "target": {"id": "hdd", "destination": {"kind": "local", "path": path}}});
    // not a second copy, not btrfs, no such job
    for (body, why) in [
        (local("/srv/vol1/replica"), "not a second copy"),
        (local("/mnt/usb/replica"), "not on btrfs"),
        (json!({"job": "nope", "target": {"id": "x", "destination": {"kind": "local", "path": "/srv/vol2/x"}}}), "no snapshot job"),
    ] {
        let (s, b) = call(&e, "POST", "/v1/snapshots/replication", Some(body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&b));
        assert!(String::from_utf8_lossy(&b).contains(why), "{}", String::from_utf8_lossy(&b));
    }
    let (s, b) = call(&e, "POST", "/v1/snapshots/replication", Some(local("/srv/vol2/.replicas/docs"))).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let v: ReplicaView = serde_json::from_slice(&b).unwrap();
    assert!(v.target.after_snapshots && v.task_id.is_none() && v.warnings.is_empty());
    assert_eq!(v.target.retention.daily, 14, "default target retention");
    let (s, _) = call(&e, "POST", "/v1/snapshots/replication", Some(local("/srv/vol2/other"))).await;
    assert_eq!(s, StatusCode::CONFLICT, "same id twice");
    // a remote target with its own schedule
    let remote = json!({"job": "docs", "schedule": {"kind": "calendar", "on_calendar": "02:00"},
        "target": {"id": "offsite", "after_snapshots": false, "compress": "zstd", "rate_limit_kib": 2048,
            "destination": {"kind": "ssh", "host": "backup.lan", "user": "btrbk", "sudo": true, "path": "/srv/backup/docs"}}});
    let (s, b) = call(&e, "POST", "/v1/snapshots/replication", Some(remote.clone())).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let v: ReplicaView = serde_json::from_slice(&b).unwrap();
    assert_eq!(v.task_id.as_deref(), Some("replica-docs-offsite"));
    assert_eq!(v.url, "ssh://backup.lan:22/srv/backup/docs");
    assert_eq!(v.warnings, [ReplicaWarning::NoHostKey]);
    assert_eq!(v.authorized_keys_line, "", "no key yet");
    // key pair, authorized_keys line, host keys
    let (s, b) = call(&e, "POST", "/v1/snapshots/replication/ssh-key", None).await;
    assert_eq!(s, StatusCode::OK);
    let k: ReplicationKey = serde_json::from_slice(&b).unwrap();
    assert!(k.public_key.starts_with("ssh-ed25519 ") && k.fingerprint.starts_with("SHA256:"));
    let (_, b) = call(&e, "GET", "/v1/snapshots/replication/docs/offsite", None).await;
    let v: ReplicaView = serde_json::from_slice(&b).unwrap();
    assert!(v.authorized_keys_line.starts_with("command=\"/usr/share/btrbk/scripts/ssh_filter_btrbk.sh --target --delete --restrict-path /srv/backup/docs --sudo --log\",restrict ssh-ed25519 "), "{}", v.authorized_keys_line);
    let (s, b) = call(&e, "POST", "/v1/snapshots/replication/host-keys", Some(json!({"host": "backup.lan"}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let keys: Vec<HostKey> = serde_json::from_slice(&b).unwrap();
    assert_eq!(keys[0].line, "backup.lan ssh-ed25519 AAAAC3NzaC1lZDI1NTE5");
    let (s, _) = call(&e, "POST", "/v1/snapshots/replication/host-keys", Some(json!({"host": "-oProxyCommand=x"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let mut confirmed = remote.clone();
    confirmed["target"]["destination"]["host_keys"] = json!([keys[0].line]);
    let (s, b) = call(&e, "PUT", "/v1/snapshots/replication/docs/offsite", Some(confirmed)).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert!(serde_json::from_slice::<ReplicaView>(&b).unwrap().warnings.is_empty());
    apply(&e);
    let conf = std::fs::read_to_string(e._dir.path().join("root/etc/rynas/btrbk/docs.conf")).unwrap();
    assert!(conf.contains("    target send-receive /srv/vol2/.replicas/docs\n      group replica-hdd\n"), "{conf}");
    assert!(conf.contains("      rate_limit 2048k\n      ssh_user btrbk\n      ssh_identity /etc/rynas/ssh/replication_ed25519\n      stream_compress zstd\n      backend_remote btrfs-progs-sudo\n"), "{conf}");
    let kh = std::fs::read_to_string(e._dir.path().join("root/etc/rynas/ssh/known_hosts")).unwrap();
    assert!(kh.contains("backup.lan ssh-ed25519 AAAAC3NzaC1lZDI1NTE5"));
    // the task of a target goes with the target; editing the job keeps its targets
    let (s, _) = call(&e, "DELETE", "/v1/scheduler/tasks/replica-docs-offsite", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks", Some(json!({"id": "r", "action": {"kind": "replicate", "job": "docs", "target": "hdd"}, "trigger": {"kind": "manual"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, b) = call(&e, "PUT", "/v1/snapshots/jobs/docs", Some(json!({"subvolume": "/srv/vol1/docs"}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(serde_json::from_slice::<SnapshotJobView>(&b).unwrap().job.replicas.len(), 2);
    // replicas: local only; a missing directory has none; promote needs the name typed
    let (s, _) = call(&e, "GET", "/v1/snapshots/replication/docs/offsite/snapshots", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, b) = call(&e, "GET", "/v1/snapshots/replication/docs/hdd/snapshots", None).await;
    assert_eq!((s, b.as_slice()), (StatusCode::OK, b"[]".as_slice()));
    let (s, b) = call(&e, "POST", "/v1/snapshots/replication/docs/hdd/snapshots/docs.20261108T0100/promote", Some(json!({"folder": "docs-restored", "confirm": "x"}))).await;
    assert_eq!((s, code(&b).as_str()), (StatusCode::BAD_REQUEST, error_codes::CONFIRMATION_REQUIRED));
    let (s, _) = call(&e, "GET", "/v1/snapshots/replication/docs/hdd/snapshots/other.20261108T0100/browse", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "another job's names");
    // removing a target removes its task, removing the job removes the rest
    let (s, _) = call(&e, "DELETE", "/v1/snapshots/replication/docs/offsite", None).await;
    assert_eq!(s, StatusCode::OK);
    apply(&e);
    let (_, b) = call(&e, "GET", "/v1/scheduler/tasks", None).await;
    assert!(!String::from_utf8_lossy(&b).contains("replica-docs-offsite"));
    assert!(!e._dir.path().join("root/etc/rynas/ssh/known_hosts").exists(), "no remote target: no known_hosts");
    let (s, _) = call(&e, "DELETE", "/v1/snapshots/jobs/docs", None).await;
    assert_eq!(s, StatusCode::OK);
    apply(&e);
    let (_, b) = call(&e, "GET", "/v1/snapshots/replication", None).await;
    assert_eq!(b, b"[]");
}

/// A manual task "hello" of alice, applied.
async fn applied_hello(e: &Env) {
    let hello = json!({"id": "hello", "action": {"kind": "command", "command": "rsync -a --progress /a/ /b/", "user": "alice"},
        "trigger": {"kind": "manual"}});
    let (s, b) = call(e, "POST", "/v1/scheduler/tasks", Some(hello)).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    apply(e);
}

#[tokio::test]
async fn live_output_follows_a_run_until_it_ends() {
    let script: Vec<(u64, Vec<u8>)> = vec![
        (0, b"sending incremental file list".to_vec()),
        (100, b"     1,024  10%\r    10,240 100% (xfr#1)".to_vec()),
        (200, b"caf\xe9.png".to_vec()),
    ];
    let e = env_live(Arc::new(Demo::with_script(script)));
    applied_hello(&e).await;
    // nothing runs: one `end` event and the stream closes (a plain user reads it too, like the
    // history)
    let (s, b) = call_as(&e, "bob", "GET", "/v1/scheduler/tasks/hello/live", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&b), "event: end\ndata: {\"ran\":false}\n\n");
    let (s, _) = call(&e, "GET", "/v1/scheduler/tasks/Bad/live", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/hello/run", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, b) = call(&e, "GET", "/v1/scheduler/tasks/hello", None).await;
    assert!(serde_json::from_slice::<TaskView>(&b).unwrap().status.running);
    // the whole run: `run`, the lines (CR and the replaced byte escaped inside one data line),
    // then `end`
    let (s, b) = call_as(&e, "bob", "GET", "/v1/scheduler/tasks/hello/live", None).await;
    assert_eq!(s, StatusCode::OK);
    let text = String::from_utf8(b).unwrap();
    assert!(text.starts_with("event: run\ndata: {\"invocation_id\":\""), "{text}");
    assert!(text.ends_with("event: end\ndata: {\"ran\":true}\n\n"), "{text}");
    let mut lines = Vec::new();
    for frame in text.split("\n\n").filter(|f| f.starts_with("event: lines\n")) {
        let data: Vec<&str> = frame.lines().filter_map(|l| l.strip_prefix("data: ")).collect();
        assert_eq!(data.len(), 1, "one data line per event: {frame:?}");
        lines.extend(serde_json::from_str::<Vec<String>>(data[0]).unwrap());
    }
    assert_eq!(
        lines,
        ["sending incremental file list", "     1,024  10%\r    10,240 100% (xfr#1)", "caf\u{fffd}.png"]
    );
    // the run became a normal history entry
    let mut runs = Vec::new();
    for _ in 0..50 {
        let (_, b) = call(&e, "GET", "/v1/scheduler/tasks/hello/history", None).await;
        runs = serde_json::from_slice::<Vec<TaskRun>>(&b).unwrap();
        if runs.first().is_some_and(|r| r.output_bytes > 0) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(runs.len(), 1);
    assert!(runs[0].ok);
    let (_, b) = call(&e, "GET", &format!("/v1/scheduler/tasks/hello/history/{}", runs[0].id), None).await;
    let run: TaskRun = serde_json::from_slice(&b).unwrap();
    assert!(run.output.unwrap().starts_with("sending incremental file list\n"));
    let (_, b) = call(&e, "GET", "/v1/scheduler/tasks/hello", None).await;
    assert!(!serde_json::from_slice::<TaskView>(&b).unwrap().status.running);
}

/// Processes whose parent is this test process and whose name is `journalctl`, with their
/// state letter (`Z`: not reaped yet).
fn journalctl_children() -> Vec<(u32, char)> {
    let me = std::process::id();
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        // pid (comm) state ppid ...
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let comm = &stat[open + 1..close];
        let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        if comm == "journalctl" && rest.get(1).and_then(|p| p.parse::<u32>().ok()) == Some(me) {
            let pid = stat[..open].trim().parse().unwrap_or(0);
            out.push((pid, rest[0].chars().next().unwrap_or('?')));
        }
    }
    out
}

#[tokio::test]
async fn closing_the_live_output_kills_journalctl() {
    use futures_util::StreamExt;
    if std::process::Command::new("journalctl").arg("--version").output().is_err() {
        eprintln!("no journalctl here: skipped");
        return;
    }
    let e = env();
    applied_hello(&e).await;
    {
        let mut s = e.probe.state.lock().unwrap();
        s.running.push("rynas-task-hello.service".into());
        // an invocation nothing logged under: journalctl waits with --follow
        s.invocations.insert("rynas-task-hello.service".into(), "0123456789abcdef0123456789abcdef".into());
        s.starts.insert("rynas-task-hello.service".into(), 1_800_000_000_000);
    }
    let req = Request::builder()
        .uri("/v1/scheduler/tasks/hello/live")
        .header("x-rynas-user", "bob")
        .body(Body::empty())
        .unwrap();
    let r = e.app.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()[header::CONTENT_TYPE], "text/event-stream");
    let mut body = r.into_body().into_data_stream();
    let first = body.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).starts_with("event: run\n"));
    // journalctl runs while the stream is open
    let mut seen = false;
    for _ in 0..50 {
        if journalctl_children().iter().any(|(_, st)| *st != 'Z') {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(seen, "journalctl did not start");
    // the browser goes away: the body is dropped, the child is killed and reaped
    drop(body);
    let mut left = journalctl_children();
    for _ in 0..100 {
        if left.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        left = journalctl_children();
    }
    assert!(left.is_empty(), "journalctl left behind: {left:?}");
}

#[tokio::test]
async fn systemd_not_answering_is_not_the_end_of_a_run() {
    use futures_util::StreamExt;
    let e = env();
    applied_hello(&e).await;
    // at open: `failed`, not "nothing running"
    e.probe.state.lock().unwrap().current_run_errors = 1;
    let (s, b) = call_as(&e, "bob", "GET", "/v1/scheduler/tasks/hello/live", None).await;
    assert_eq!(s, StatusCode::OK);
    let text = String::from_utf8(b).unwrap();
    assert!(text.starts_with("event: failed\n"), "{text}");
    if std::process::Command::new("journalctl").arg("--version").output().is_err() {
        eprintln!("no journalctl here: the rest is skipped");
        return;
    }
    // while following: failed reads are asked again, the stream ends only when the unit stops
    {
        let mut s = e.probe.state.lock().unwrap();
        s.running.push("rynas-task-hello.service".into());
        s.invocations.insert("rynas-task-hello.service".into(), "fedcba9876543210fedcba9876543210".into());
    }
    let req = Request::builder()
        .uri("/v1/scheduler/tasks/hello/live")
        .header("x-rynas-user", "bob")
        .body(Body::empty())
        .unwrap();
    let r = e.app.clone().oneshot(req).await.unwrap();
    let mut body = r.into_body().into_data_stream();
    let first = body.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).starts_with("event: run\n"));
    e.probe.state.lock().unwrap().current_run_errors = 3;
    let early = tokio::time::timeout(Duration::from_millis(3500), body.next()).await;
    assert!(early.is_err(), "the stream ended or sent something while systemd did not answer: {early:?}");
    e.probe.state.lock().unwrap().running.clear();
    let mut rest = String::new();
    while let Ok(Some(Ok(chunk))) = tokio::time::timeout(Duration::from_secs(10), body.next()).await {
        rest.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(rest.contains("event: end\ndata: {\"ran\":true}"), "{rest}");
}
