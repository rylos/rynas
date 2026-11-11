//! Deduplication endpoints and runs with a scripted engine ([`MemoryDedup`]): settings
//! staged with their scheduler task, the command line of a run, outcomes and their
//! history and notifications, preemption by other work on the filesystem, stop, roots
//! that leave the job. No disk, no systemd, no oans.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::dedup::{DedupBlock, DedupKind, DedupOutcome, DedupOverview, DedupRecord, DedupWarning};
use rynas_api_types::scheduler::{SchedulerConfig, TaskAction, TaskTrigger};
use rynas_api_types::{ApiError, TaskStarted, TaskStatus, error_codes};
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
use rynasd::dedup::{DedupSvc, MemoryDedup, Script};
use rynasd::fslock::FsLocks;
use rynasd::ops::Ops;
use rynasd::protection::Protection;
use rynasd::roles::AdminPolicy;
use rynasd::server::{DaemonState, router};
use serde_json::{Value, json};
use tower::ServiceExt;

struct NoAuth;

impl Authenticator for NoAuth {
    fn realm(&self) -> &str {
        "none"
    }
    fn authenticate<'a>(&'a self, _: &'a str, _: &'a str) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Err(AuthError::InvalidCredentials) })
    }
}

struct Env {
    _dir: tempfile::TempDir,
    app: axum::Router,
    config: Arc<Config>,
    ops: Arc<Ops>,
    host: Arc<MemoryDedup>,
    notifier: Arc<MemoryNotifier>,
    locks: Arc<FsLocks>,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let acc = Arc::new(MemoryAccounts::new());
    let shares_host = Arc::new(MemoryShares::new());
    for (p, u) in [("/vol1/docs", Some("u1")), ("/vol1/media", Some("u2")), ("/vol1/vms", Some("u3")), ("/vol2/old", None)] {
        shares_host.add_dir(p, u);
    }
    let snapshots = Arc::new(SnapshotsDomain::new(Arc::new(MemorySnapshots::new())));
    let scheduler = Arc::new(SchedulerDomain::new(Paths::default(), Arc::new(|_: &str| None)));
    let config = Arc::new(
        Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            sys,
            vec![
                Arc::new(UsersDomain::new(acc.clone())),
                Arc::new(SharesDomain::new(shares_host, acc)),
                snapshots.clone(),
                Arc::new(rynas_dedup::DedupDomain),
                scheduler.clone(),
            ],
        )
        .unwrap(),
    );
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let mut ops = Ops::new(config.clone(), tasks);
    ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], |u| match u {
        "admin" => Some(vec!["rynas-admin".into()]),
        "bob" => Some(vec!["users".into()]),
        _ => None,
    });
    let ops = Arc::new(ops);
    let notifier = Arc::new(MemoryNotifier::default());
    let protection = Arc::new(Protection::new(
        ops.clone(),
        scheduler,
        snapshots,
        Arc::new(FakeProbe::default()),
        Arc::new(History::open(&dir.path().join("scheduler.db")).unwrap()),
        notifier.clone(),
        dir.path().join("state"),
    ));
    let host = Arc::new(MemoryDedup::new(
        true,
        vec![("vol1".into(), "/vol1".into(), "btrfs".into()), ("vol2".into(), "/vol2".into(), "ext4".into())],
    ));
    host.set_nocow("/vol1/vms");
    let locks = Arc::new(FsLocks::default());
    let svc = Arc::new(DedupSvc::new(
        protection.clone(),
        host.clone(),
        Arc::new(rynas_dedup::Oans::default()),
        dir.path().join("state/dedup"),
        vec!["@eaDir/".into()],
        locks.clone(),
    ));
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(MemoryStorage::default(), clock.clone(), "rynas"));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    st.protection = Some(protection);
    st.dedup = Some(svc);
    Env { _dir: dir, app: router(st), config, ops, host, notifier, locks }
}

async fn call_as(e: &Env, user: &str, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-rynas-user", user)
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    let r = e.app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 4 << 20).await.unwrap().to_vec())
}

async fn call(e: &Env, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    call_as(e, "admin", method, uri, body).await
}

fn code(b: &[u8]) -> String {
    serde_json::from_slice::<ApiError>(b).unwrap().code
}

fn apply(e: &Env) {
    e.config.apply("admin", "admin", "test", &|l| eprintln!("{l}"), &|| false).unwrap();
}

/// Shares docs (dedup on, an exclude), media (off), vms (no copy-on-write), old (ext4).
fn shares(e: &Env, media_on: bool) {
    let doc = json!({"shares": [
        {"name": "docs", "path": "/vol1/docs", "dedup": {"enabled": true, "excludes": ["cache/"]}},
        {"name": "media", "path": "/vol1/media", "dedup": {"enabled": media_on, "include_recycle": true}},
        {"name": "vms", "path": "/vol1/vms", "dedup": {"enabled": true}},
        {"name": "old", "path": "/vol2/old"},
    ]});
    e.config.stage("admin", rynas_services::SHARES_KEY, Some(doc)).unwrap();
    apply(e);
}

async fn overview(e: &Env) -> DedupOverview {
    let (s, b) = call(e, "GET", "/v1/storage/dedup", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    serde_json::from_slice(&b).unwrap()
}

async fn set_up(e: &Env, window: &str) {
    let (s, b) = call(e, "PUT", "/v1/storage/dedup/jobs/vol1", Some(json!({"window": window, "only_if_awake": true}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    apply(e);
}

async fn start(e: &Env, what: &str, body: Value) -> String {
    let (s, b) = call(e, "POST", &format!("/v1/storage/dedup/jobs/vol1/{what}"), Some(body)).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id
}

fn in_an_hour() -> u64 {
    rynas_tasks::now_ms() + 3_600_000
}

async fn history(e: &Env) -> Vec<DedupRecord> {
    let (s, b) = call(e, "GET", "/v1/storage/dedup/jobs/vol1/history", None).await;
    assert_eq!(s, StatusCode::OK);
    serde_json::from_slice(&b).unwrap()
}

fn codes(e: &Env) -> Vec<String> {
    e.notifier.0.lock().unwrap().iter().map(|n| n.code.clone()).collect()
}

#[tokio::test]
async fn overview_settings_and_the_scheduler_task() {
    let e = env();
    shares(&e, false);
    let (s, _) = call_as(&e, "bob", "GET", "/v1/storage/dedup", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let o = overview(&e).await;
    assert!(o.engine.installed && o.engine.supported);
    // vol2 is ext4: no job, its share is listed apart
    assert_eq!(o.jobs.iter().map(|j| j.key.as_str()).collect::<Vec<_>>(), ["vol1"]);
    let j = &o.jobs[0];
    assert!(j.settings.is_none() && j.hashfile == "/vol1/@rynas-cache/dedup/volume.hash", "{}", j.hashfile);
    let block = |n: &str| j.shares.iter().find(|s| s.name == n).unwrap().block;
    assert_eq!(block("docs"), None);
    assert_eq!(block("vms"), Some(DedupBlock::Nocow));
    assert!(j.warnings.contains(&DedupWarning::SharesBlocked));
    assert_eq!(o.other_shares[0].name, "old");
    assert_eq!(o.other_shares[0].block, Some(DedupBlock::Filesystem));
    assert_eq!(&o.always_excluded[..2], ["#snapshot/", "#recycle/"]);
    assert!(o.always_excluded.contains(&"@eaDir/".to_string()));

    // a run needs settings; bad settings and unknown filesystems are refused
    let (s, b) = call(&e, "POST", "/v1/storage/dedup/jobs/vol1/run", Some(json!({}))).await;
    assert_eq!((s, code(&b).as_str()), (StatusCode::CONFLICT, error_codes::CONFLICT));
    let (s, _) = call(&e, "PUT", "/v1/storage/dedup/jobs/vol1", Some(json!({"window": "0".repeat(168)}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&e, "PUT", "/v1/storage/dedup/jobs/vol2", Some(json!({}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // defaults: every night 01–06, the task with it, staged together
    let (s, b) = call(&e, "PUT", "/v1/storage/dedup/jobs/vol1", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    apply(&e);
    let sched: SchedulerConfig = serde_json::from_value(e.config.applied(rynas_scheduler::SCHEDULER_KEY).unwrap().unwrap()).unwrap();
    let t = sched.tasks.iter().find(|t| t.id == "dedup-vol1").unwrap();
    assert_eq!(t.action, TaskAction::Dedup { volume: "vol1".into() });
    assert!(matches!(&t.trigger, TaskTrigger::Calendar { on_calendar, persistent: false, .. } if on_calendar == "*-*-* 01:00:00"));
    let o = overview(&e).await;
    let s1 = o.jobs[0].settings.clone().unwrap();
    assert!(s1.only_if_awake && s1.enabled);
    assert!(o.jobs[0].next_window_ms.is_some());

    // the scheduler leaves the task to Storage
    let (s, _) = call(&e, "DELETE", "/v1/scheduler/tasks/dedup-vol1", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(&e, "POST", "/v1/scheduler/tasks/dedup-vol1/disable", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(
        &e,
        "POST",
        "/v1/scheduler/tasks",
        Some(json!({"id": "mine", "action": {"kind": "dedup", "volume": "vol1"}, "trigger": {"kind": "manual"}})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // off: settings and task go
    let (s, _) = call(&e, "DELETE", "/v1/storage/dedup/jobs/vol1", None).await;
    assert_eq!(s, StatusCode::OK);
    apply(&e);
    let sched: SchedulerConfig = serde_json::from_value(e.config.applied(rynas_scheduler::SCHEDULER_KEY).unwrap().unwrap()).unwrap();
    assert!(!sched.tasks.iter().any(|t| t.id == "dedup-vol1"));
}

#[tokio::test]
async fn shares_refuse_dedup_where_nothing_can_be_shared() {
    let e = env();
    let doc = json!({"shares": [{"name": "docs", "path": "/vol1/docs", "dedup": {"enabled": true, "excludes": ["!keep"]}}]});
    assert!(e.config.stage("admin", rynas_services::SHARES_KEY, Some(doc)).is_err());
    let doc = json!({"shares": [{"name": "new", "subvolume": {"volume": "vol1", "nocow": true}, "dedup": {"enabled": true}}]});
    assert!(e.config.stage("admin", rynas_services::SHARES_KEY, Some(doc)).is_err());
}

#[tokio::test]
async fn a_run_its_command_line_history_and_notification() {
    let e = env();
    shares(&e, false);
    set_up(&e, &"1".repeat(168)).await;
    let id = start(&e, "run", json!({"until_ms": in_an_hour()})).await;
    // a second run of the same job is refused while the first one goes
    let (s, _) = call(&e, "POST", "/v1/storage/dedup/jobs/vol1/run", Some(json!({}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let info = e.ops.tasks.wait(&id).await.unwrap();
    assert_eq!(info.status, TaskStatus::Ok, "{info:?}");
    let started = e.host.started();
    assert_eq!(started.len(), 1);
    let argv = started[0].argv.join(" ");
    for want in [
        " -r -d ",
        "--skip-readonly-subvols",
        "--progress=json",
        "--exclude=/vol1/docs/\\#snapshot/",
        "--exclude=/vol1/docs/\\#recycle/",
        "--exclude=/vol1/docs/**/cache/",
        "--exclude=@eaDir/",
        "--exclude=/vol1/docker/engine/",
    ] {
        assert!(argv.contains(want), "{want} in {argv}");
    }
    // only the enabled share that can be deduplicated, after `--`
    assert!(argv.ends_with("-- /vol1/docs"), "{argv}");
    assert!(started[0].properties.contains(&"KillSignal=SIGINT".to_string()));
    assert!(started[0].properties.contains(&"ReadWritePaths=-/vol1/docs".to_string()));
    let h = history(&e).await;
    assert_eq!(h[0].outcome, DedupOutcome::Done);
    assert_eq!(h[0].reclaimed_bytes, 512 << 20);
    assert_eq!(h[0].shares, ["docs"]);
    assert_eq!(codes(&e), ["dedup.ok"]);
    let o = overview(&e).await;
    assert_eq!(o.jobs[0].report.as_ref().unwrap().reclaimed_total_bytes, 536_870_912);
    assert!(o.jobs[0].running.is_none());
    // the report was read from the hashfile after the run
    assert!(e.host.execs().iter().any(|a| a.iter().any(|x| x == "--json")));
}

#[tokio::test]
async fn an_estimate_looks_at_every_share_that_can_be_deduplicated() {
    let e = env();
    // nothing enabled, no settings: an estimate still works, on docs and media
    let doc = json!({"shares": [
        {"name": "docs", "path": "/vol1/docs"},
        {"name": "media", "path": "/vol1/media"},
        {"name": "vms", "path": "/vol1/vms"},
    ]});
    e.config.stage("admin", rynas_services::SHARES_KEY, Some(doc)).unwrap();
    apply(&e);
    e.host.set_report(r#"{"files_tracked":10,"logical_bytes":1000,"dup_files":4,"reclaimable_logical_bytes":300,"scan_configured_dedupe":false}"#);
    let id = start(&e, "estimate", json!({"until_ms": in_an_hour()})).await;
    assert_eq!(e.ops.tasks.wait(&id).await.unwrap().status, TaskStatus::Ok);
    let argv = &e.host.started()[0].argv;
    assert!(!argv.contains(&"-d".to_string()));
    assert!(argv.ends_with(&["--".into(), "/vol1/docs".into(), "/vol1/media".into()]));
    let est = overview(&e).await.jobs[0].estimate.clone().unwrap();
    assert_eq!((est.reclaimable_bytes, est.dup_files, est.files), (300, 4, 10));
    assert_eq!(history(&e).await[0].kind, DedupKind::Estimate);
    // an estimate does not notify
    assert!(codes(&e).is_empty());
    // a share that cannot be deduplicated cannot be chosen
    let (s, _) = call(&e, "POST", "/v1/storage/dedup/jobs/vol1/estimate", Some(json!({"shares": ["vms"]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn partial_and_failed_runs() {
    let e = env();
    shares(&e, false);
    set_up(&e, &"1".repeat(168)).await;
    e.host.push_script(Script { lines: vec![], code: 2, stop_after: Duration::ZERO });
    let id = start(&e, "run", json!({"until_ms": in_an_hour()})).await;
    assert_eq!(e.ops.tasks.wait(&id).await.unwrap().status, TaskStatus::Ok);
    assert_eq!(history(&e).await[0].outcome, DedupOutcome::Partial);
    e.host.push_script(Script { lines: vec![], code: 1, stop_after: Duration::ZERO });
    let id = start(&e, "run", json!({"until_ms": in_an_hour()})).await;
    assert_eq!(e.ops.tasks.wait(&id).await.unwrap().status, TaskStatus::Error);
    assert_eq!(history(&e).await[0].outcome, DedupOutcome::Failed);
    assert_eq!(codes(&e), ["dedup.partial", "dedup.failed"]);
}

/// A run that takes long: hashing lines every 200 ms.
fn long_script() -> Script {
    let line = r#"{"phase":"hashing","files":1,"files_total":2,"bytes":10,"bytes_total":20}"#;
    Script { lines: (0..200).map(|_| (Duration::from_millis(200), line.to_string())).collect(), code: 0, stop_after: Duration::from_millis(100) }
}

#[tokio::test]
async fn other_work_preempts_a_run_which_goes_on_after_it() {
    let e = env();
    shares(&e, false);
    set_up(&e, &"1".repeat(168)).await;
    e.host.push_script(long_script());
    let id = start(&e, "run", json!({"until_ms": in_an_hour()})).await;
    // wait until the engine runs and reports progress
    for _ in 0..50 {
        if overview(&e).await.jobs[0].running.as_ref().is_some_and(|r| r.progress.is_some()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let key = e.host.started()[0].key.clone();
    assert_eq!(key, "vol1");
    // a scrub of the same filesystem: the run stops and saves, the scrub goes
    let guard = e.locks.heavy(&["0:40".into()], "btrfs.scrub").await;
    let mut waited = None;
    for _ in 0..50 {
        waited = overview(&e).await.jobs[0].running.as_ref().and_then(|r| r.waiting_for.clone());
        if waited.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(waited.as_deref(), Some("btrfs.scrub"));
    drop(guard);
    // then the run starts again (the default script finishes)
    assert_eq!(e.ops.tasks.wait(&id).await.unwrap().status, TaskStatus::Ok);
    let h = history(&e).await;
    assert_eq!(h[1].outcome, DedupOutcome::Preempted);
    assert_eq!(h[1].by.as_deref(), Some("btrfs.scrub"));
    assert_eq!(h[0].outcome, DedupOutcome::Done);
    assert_eq!(e.host.started().len(), 2);
}

#[tokio::test]
async fn stop_and_roots_that_leave_the_job() {
    let e = env();
    shares(&e, true);
    set_up(&e, &"1".repeat(168)).await;
    e.host.push_script(long_script());
    let id = start(&e, "run", json!({"until_ms": in_an_hour()})).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (s, _) = call(&e, "POST", "/v1/storage/dedup/jobs/vol1/stop", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(e.ops.tasks.wait(&id).await.unwrap().status, TaskStatus::Ok);
    assert_eq!(history(&e).await[0].outcome, DedupOutcome::Stopped);
    let (s, _) = call(&e, "POST", "/v1/storage/dedup/jobs/vol1/stop", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // the stopped run still recorded its roots; media leaves the job
    let id = start(&e, "run", json!({"until_ms": in_an_hour()})).await;
    e.ops.tasks.wait(&id).await.unwrap();
    shares(&e, false);
    let id = start(&e, "run", json!({"until_ms": in_an_hour()})).await;
    e.ops.tasks.wait(&id).await.unwrap();
    let removed = e.host.execs().into_iter().find(|a| a.iter().any(|x| x == "-R")).expect("a -R");
    assert_eq!(removed.last().map(String::as_str), Some("/vol1/media"));
}

#[tokio::test]
async fn reset_and_install() {
    let e = env();
    shares(&e, false);
    let (s, _) = call(&e, "POST", "/v1/storage/dedup/jobs/vol1/reset", Some(json!({"confirm": "vol2"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, b) = call(&e, "POST", "/v1/storage/dedup/jobs/vol1/reset", Some(json!({"confirm": "vol1"}))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    assert_eq!(e.ops.tasks.wait(&id).await.unwrap().status, TaskStatus::Ok);
    assert!(e.host.removed()[0] == "/vol1/@rynas-cache/dedup/volume.hash");
    // already installed
    let (s, _) = call(&e, "POST", "/v1/storage/dedup/install", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
}
