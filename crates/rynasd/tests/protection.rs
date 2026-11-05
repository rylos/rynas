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
}

fn env() -> Env {
    env_with(RootBackend::None)
}

fn env_with(root_ops: RootBackend) -> Env {
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
    .with_root(root_ops);
    st.protection = Some(Arc::new(protection));
    Env {
        _dir: dir,
        app: router(st),
        sys,
        probe,
        notifier,
        config,
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
async fn root_rollback_needs_synodeb_and_a_bootable_restore_point() {
    use rynas_api_types::snapshots::{RollbackPreview, RollbackRefusal};
    // not synodeb: previewed and refused with a clear reason
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
    assert_eq!(pv.refusal, Some(RollbackRefusal::NotSynodeb));
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
    assert!(String::from_utf8_lossy(&b).contains("needs synodeb"));
    // synodeb (dry): the plain directory is not a read-only restore point
    let e = env_with(RootBackend::Custom(Arc::new(
        rynas_btrfs::rootfs::DryRoot::new(true),
    )));
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
