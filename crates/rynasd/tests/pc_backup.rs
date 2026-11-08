//! PC backup, receive side (`/v1/pc-backups/...`) on the memory host: administrators only,
//! new and adopted folders, key lines with the filter and the NAS rules, settings, removal
//! (data kept), the late notification, browse and copy-out, setup problems. Nothing here
//! touches a disk, sshd or the system's accounts.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::notify::EventState;
use rynas_api_types::pcbackup::*;
use rynas_api_types::snapshots::SnapshotListing;
use rynas_api_types::{ApiError, TaskInfo, TaskStarted, TaskStatus};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_btrfs::ioctl::SubvolInfo;
use rynas_btrfs::receive::Entry;
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_notify::MemoryNotifier;
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
        "none"
    }
    fn authenticate<'a>(&'a self, _: &'a str, _: &'a str) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Err(AuthError::InvalidCredentials) })
    }
}

struct Env {
    _dir: tempfile::TempDir,
    app: axum::Router,
    ops: Arc<Ops>,
    host: Arc<MemoryReceive>,
    svc: Arc<PcBackup>,
    notifier: Arc<MemoryNotifier>,
}

fn groups(u: &str) -> Option<Vec<String>> {
    match u {
        "admin" => Some(vec!["rynas-admin".into()]),
        "bob" => Some(vec!["users".into()]),
        _ => None,
    }
}

const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl marco@laptop";

fn now_s() -> i64 {
    (rynas_tasks::now_ms() / 1000) as i64
}

fn snap(group: &str, days_ago: i64) -> Entry {
    let t = now_s() - days_ago * 86_400;
    Entry {
        name: format!("{group}.{}", rynas_btrfs::name::long_timestamp(t)),
        info: Some(SubvolInfo {
            readonly: true,
            received_uuid: Some("6f1c2a8e-0d3b-4c5e-9a7f-000000000001".into()),
            otime: t,
            ..Default::default()
        }),
    }
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let state_dir = dir.path().join("state");
    std::fs::create_dir_all(&root).unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let config = Arc::new(Config::open(ConfigOptions::new(&state_dir, &root), sys, Vec::new()).unwrap());
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let ops = Arc::new(Ops::new(config, tasks));
    let notifier = Arc::new(MemoryNotifier::default());
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(MemoryStorage::default(), clock.clone(), "rynas"));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    let host = Arc::new(MemoryReceive::new());
    let svc = PcBackup::new(
        ops.clone(),
        host.clone(),
        notifier.clone(),
        state_dir,
        AdminPolicy::new(vec!["rynas-admin".into()], groups),
        "nas".into(),
    );
    st.pc_backup = Some(svc.clone());
    Env { _dir: dir, app: router(st), ops, host, svc, notifier }
}

async fn call_as(e: &Env, who: &str, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-rynas-user", who)
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    let r = e.app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec())
}

async fn call(e: &Env, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    call_as(e, "admin", method, uri, body).await
}

fn err(b: &[u8]) -> ApiError {
    serde_json::from_slice(b).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(b)))
}

async fn task(e: &Env, method: &str, uri: &str, body: Option<Value>) -> TaskInfo {
    let (s, b) = call(e, method, uri, body).await;
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

async fn list(e: &Env) -> Vec<PcTarget> {
    let (s, b) = call(e, "GET", "/v1/pc-backups/targets", None).await;
    assert_eq!(s, StatusCode::OK);
    serde_json::from_slice(&b).unwrap()
}

fn keys(e: &Env) -> String {
    e.host.keys.lock().unwrap().clone()
}

#[tokio::test]
async fn administrators_only() {
    let e = env();
    for (m, uri) in [("GET", "/v1/pc-backups/targets"), ("GET", "/v1/pc-backups/setup"), ("POST", "/v1/pc-backups/targets")] {
        let (s, _) = call_as(&e, "bob", m, uri, Some(json!({}))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{m} {uri}");
    }
}

#[tokio::test]
async fn new_folder_with_key_line() {
    let e = env();
    let body = json!({"device": "laptop", "description": "Marco's laptop", "path": "/volume1/backup/laptop", "public_key": KEY, "delete": {"allowed": true, "keep_last": 3, "min_age_days": 7}});
    let t = task(&e, "POST", "/v1/pc-backups/targets", Some(body.clone())).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    assert!(*e.host.account.lock().unwrap());
    assert!(e.host.folders.lock().unwrap().contains_key(&PathBuf::from("/volume1/backup/laptop")));
    let k = keys(&e);
    assert!(k.contains(&format!("restrict,command=\"{FILTER} --device laptop --root /volume1/backup/laptop --delete --keep-last 3 --min-age-days 7\" ssh-ed25519 AAAAC3")), "{k}");
    assert!(k.trim_end().ends_with("rynas:laptop") && !k.contains("marco@laptop"));
    let l = list(&e).await;
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].state, ReceiveState::Waiting);
    assert_eq!(l[0].alert_days, 3);
    assert!(l[0].key_fingerprint.as_deref().unwrap().starts_with("ssh-ed25519 SHA256:"));
    // same device, overlapping folder, bad key, bad path
    let (s, b) = call(&e, "POST", "/v1/pc-backups/targets", Some(body)).await;
    assert_eq!((s, err(&b).field.as_deref()), (StatusCode::CONFLICT, Some("device")));
    let (s, b) = call(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "other", "path": "/volume1/backup/laptop/x"}))).await;
    assert_eq!((s, err(&b).field.as_deref()), (StatusCode::BAD_REQUEST, Some("path")));
    let (s, b) = call(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "other", "path": "/volume1/backup/other", "public_key": "ssh-ed25519 nope"}))).await;
    assert_eq!((s, err(&b).field.as_deref()), (StatusCode::BAD_REQUEST, Some("public_key")));
    let (s, b) = call(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "other", "path": "/volume1/backup/my pc"}))).await;
    assert_eq!((s, err(&b).field.as_deref()), (StatusCode::BAD_REQUEST, Some("path")));
    let (s, b) = call(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "Bad_Name", "path": "/volume1/backup/x"}))).await;
    assert_eq!((s, err(&b).field.as_deref()), (StatusCode::BAD_REQUEST, Some("device")));
    // an existing folder is never taken over by "new"
    e.host.folders.lock().unwrap().insert("/volume1/backup/desk".into(), (true, vec![snap("home", 1)]));
    let t = task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "desk", "path": "/volume1/backup/desk"}))).await;
    assert_eq!(t.status, TaskStatus::Error);
    assert!(t.error.unwrap().contains("adopt"));
    assert_eq!(list(&e).await.len(), 1);
}

#[tokio::test]
async fn adopt_keeps_the_chain() {
    let e = env();
    let entries = vec![snap("home", 1), snap("home", 2), snap("home", 9), snap("rootfs", 1)];
    let mut sorted = entries.clone();
    sorted.sort_by(|a, b| rynas_btrfs::name::compare_names(&b.name, &a.name));
    e.host.folders.lock().unwrap().insert("/volume1/backup/Old_Laptop".into(), (true, sorted));
    let (s, b) = call(&e, "POST", "/v1/pc-backups/plan/adopt", Some(json!({"path": "/volume1/backup/Old_Laptop"}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let p: AdoptPreview = serde_json::from_slice(&b).unwrap();
    assert_eq!(p.suggested_device.as_deref(), Some("old-laptop"));
    assert_eq!(p.list.len(), 4);
    assert_eq!(p.groups.iter().map(|g| (g.name.as_str(), g.snapshots)).collect::<Vec<_>>(), [("home", 3), ("rootfs", 1)]);
    let (s, _) = call(&e, "POST", "/v1/pc-backups/plan/adopt", Some(json!({"path": "/volume1/backup/none"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let t = task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "old-laptop", "path": "/volume1/backup/Old_Laptop", "adopt": true, "public_key": KEY}))).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    // nothing inside changed
    assert_eq!(e.host.folders.lock().unwrap()["/volume1/backup/Old_Laptop".as_ref() as &std::path::Path].1.len(), 4);
    let (s, b) = call(&e, "GET", "/v1/pc-backups/targets/old-laptop?host=nas.lan", None).await;
    assert_eq!(s, StatusCode::OK);
    let d: PcTargetDetail = serde_json::from_slice(&b).unwrap();
    assert!(d.target.adopted);
    assert!(d.client_config.contains("target ssh://nas.lan/volume1/backup/Old_Laptop"));
    assert!(d.key_line.as_deref().unwrap().starts_with("restrict,command="));
    assert!(d.test_commands.contains("rynas-pcbackup@nas.lan"));
    assert_eq!(d.target.state, ReceiveState::Ok);
    assert_eq!(d.target.snapshots, 4);
    // default rules: the newest of each subvolume is protected
    let mut protected: Vec<&str> = d.list.iter().filter(|s| s.protected).map(|s| s.group.as_str()).collect();
    protected.sort();
    assert_eq!(protected, ["home", "rootfs"]);
    // missing folder
    let t = task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "ghost", "path": "/volume1/backup/ghost", "adopt": true}))).await;
    assert_eq!(t.status, TaskStatus::Error);
}

#[tokio::test]
async fn settings_rules_and_removal() {
    let e = env();
    task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "laptop", "path": "/volume1/backup/laptop", "public_key": KEY}))).await;
    assert!(keys(&e).contains("--delete --keep-last 1"));
    let t = task(&e, "PUT", "/v1/pc-backups/targets/laptop", Some(json!({"public_key": KEY, "alert_days": 0, "delete": {"allowed": false, "keep_last": 5, "min_age_days": 30}}))).await;
    assert_eq!(t.status, TaskStatus::Ok);
    let k = keys(&e);
    assert!(k.contains("--root /volume1/backup/laptop\" ssh-ed25519") && !k.contains("--delete"), "{k}");
    // no key: no line, the PC cannot log in
    task(&e, "PUT", "/v1/pc-backups/targets/laptop", Some(json!({"public_key": "", "alert_days": 3, "delete": {"allowed": true, "keep_last": 1, "min_age_days": 0}}))).await;
    assert!(!keys(&e).contains("ssh-ed25519"));
    let (s, b) = call(&e, "PUT", "/v1/pc-backups/targets/laptop", Some(json!({"alert_days": 999, "delete": {"allowed": true, "keep_last": 1, "min_age_days": 0}}))).await;
    assert_eq!((s, err(&b).field.as_deref()), (StatusCode::BAD_REQUEST, Some("alert_days")));
    let (s, _) = call(&e, "PUT", "/v1/pc-backups/targets/nope", Some(json!({"alert_days": 1, "delete": {"allowed": true, "keep_last": 1, "min_age_days": 0}}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // removal: the key goes, the folder stays
    task(&e, "PUT", "/v1/pc-backups/targets/laptop", Some(json!({"public_key": KEY, "alert_days": 3, "delete": {"allowed": true, "keep_last": 1, "min_age_days": 0}}))).await;
    let t = task(&e, "DELETE", "/v1/pc-backups/targets/laptop", None).await;
    assert_eq!(t.status, TaskStatus::Ok);
    assert!(!keys(&e).contains("rynas:laptop"));
    assert!(e.host.folders.lock().unwrap().contains_key(&PathBuf::from("/volume1/backup/laptop")));
    assert!(list(&e).await.is_empty());
}

#[tokio::test]
async fn late_notification_opens_once_and_closes() {
    let e = env();
    e.host.folders.lock().unwrap().insert("/volume1/backup/pc".into(), (true, vec![snap("home", 5)]));
    task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "pc", "path": "/volume1/backup/pc", "adopt": true, "alert_days": 2}))).await;
    assert_eq!(list(&e).await[0].state, ReceiveState::Late);
    e.svc.check_freshness();
    e.svc.check_freshness();
    {
        let evs = e.notifier.0.lock().unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].code, "pcbackup.late");
        assert_eq!(evs[0].state, EventState::Firing);
        assert_eq!(evs[0].fields["device"], "pc");
        assert_eq!(evs[0].fields["days"], "5");
    }
    e.host.folders.lock().unwrap().get_mut(&PathBuf::from("/volume1/backup/pc")).unwrap().1.insert(0, snap("home", 0));
    e.svc.check_freshness();
    let evs = e.notifier.0.lock().unwrap();
    assert_eq!(evs.len(), 2);
    assert_eq!((evs[1].code.as_str(), evs[1].state), ("pcbackup.resumed", EventState::Resolved));
}

#[tokio::test]
async fn browse_copy_out_usage_and_setup() {
    let e = env();
    let s1 = snap("home", 1);
    e.host.folders.lock().unwrap().insert("/volume1/backup/pc".into(), (true, vec![s1.clone()]));
    task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "pc", "path": "/volume1/backup/pc", "adopt": true}))).await;
    let (s, b) = call(&e, "GET", &format!("/v1/pc-backups/targets/pc/browse?snapshot={}&path=", s1.name), None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let l: SnapshotListing = serde_json::from_slice(&b).unwrap();
    assert_eq!(l.entries.len(), 3);
    for bad in ["../x", "home.20261108T0100/x", "notasnapshot"] {
        let (s, _) = call(&e, "GET", &format!("/v1/pc-backups/targets/pc/browse?snapshot={bad}"), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
    }
    let (s, _) = call(&e, "GET", &format!("/v1/pc-backups/targets/pc/browse?snapshot={}&path=../etc", s1.name), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let t = task(&e, "POST", "/v1/pc-backups/targets/pc/copy-out", Some(json!({"snapshot": s1.name, "path": "Documents", "destination": "/volume1/Restore Here"}))).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    assert_eq!(e.host.copies.lock().unwrap().len(), 1);
    for dest in ["/volume1/backup/pc/x", "/etc/x", "relative/x"] {
        let (s, b) = call(&e, "POST", "/v1/pc-backups/targets/pc/copy-out", Some(json!({"snapshot": s1.name, "path": "Documents", "destination": dest}))).await;
        assert_eq!((s, err(&b).field.as_deref()), (StatusCode::BAD_REQUEST, Some("destination")), "{dest}");
    }
    let t = task(&e, "POST", "/v1/pc-backups/targets/pc/usage", None).await;
    assert_eq!(t.status, TaskStatus::Ok);
    assert!(list(&e).await[0].usage.is_some_and(|u| u.total_bytes > 0));

    let (_, b) = call(&e, "GET", "/v1/pc-backups/setup", None).await;
    let s: ReceiveSetup = serde_json::from_slice(&b).unwrap();
    assert!(s.problems.is_empty());
    assert_eq!((s.account.as_str(), s.host.as_str(), s.volumes.len()), (ACCOUNT, "nas", 1));
    {
        let mut f = e.host.facts.lock().unwrap();
        f.sshd_config = "port 2222\nallowusers marco\n".into();
        f.sudo = false;
    }
    let (_, b) = call(&e, "GET", "/v1/pc-backups/setup", None).await;
    let s: ReceiveSetup = serde_json::from_slice(&b).unwrap();
    assert_eq!(s.ssh_port, Some(2222));
    let codes: Vec<SetupProblemCode> = s.problems.iter().map(|p| p.code).collect();
    assert_eq!(codes, [SetupProblemCode::SshdAllowList, SetupProblemCode::SudoMissing]);
}

#[tokio::test]
async fn adopt_refuses_homes_and_asks_before_changing_permissions() {
    let e = env();
    e.host.homes.lock().unwrap().push("/volume1/homes/marco".into());
    for p in ["/volume1/homes/marco", "/volume1/homes", "/volume1/backup/pc"] {
        e.host.folders.lock().unwrap().insert(p.into(), (true, vec![snap("home", 1)]));
    }
    // a user's home, or a folder holding homes: never adopted
    for p in ["/volume1/homes/marco", "/volume1/homes"] {
        let (s, b) = call(&e, "POST", "/v1/pc-backups/plan/adopt", Some(json!({"path": p}))).await;
        assert_eq!((s, err(&b).field.as_deref()), (StatusCode::BAD_REQUEST, Some("path")), "{p}");
        let (s, _) = call(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "x", "path": p, "adopt": true}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{p}");
    }
    let (s, b) = call(&e, "POST", "/v1/pc-backups/plan/adopt", Some(json!({"path": "/var/backups/pc"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&b));
    // owned by marco 0755: the plan shows the change, the create needs the confirmation
    e.host.meta.lock().unwrap().insert("/volume1/backup/pc".into(), (1000, 1000, 0o755));
    let (s, b) = call(&e, "POST", "/v1/pc-backups/plan/adopt", Some(json!({"path": "/volume1/backup/pc"}))).await;
    assert_eq!(s, StatusCode::OK);
    let p: AdoptPreview = serde_json::from_slice(&b).unwrap();
    let c = p.permission_change.expect("a change");
    assert_eq!((c.uid, c.gid, c.mode, c.owner.as_deref()), (1000, 1000, 0o755, Some("marco")));
    assert_eq!((c.new_owner.as_str(), c.new_group.as_str(), c.new_mode), ("root", "rynas-pcbackup", 0o750));
    let body = json!({"device": "pc", "path": "/volume1/backup/pc", "adopt": true, "public_key": KEY});
    let (s, b) = call(&e, "POST", "/v1/pc-backups/targets", Some(body.clone())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(err(&b).message.contains("confirm"));
    let t = task(&e, "POST", "/v1/pc-backups/targets?confirm_permission_change=true", Some(body)).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    // done: nothing left to change
    let (_, b) = call(&e, "POST", "/v1/pc-backups/plan/adopt", Some(json!({"path": "/volume1/backup/pc"}))).await;
    assert!(serde_json::from_slice::<AdoptPreview>(&b).unwrap().permission_change.is_none());
}

#[tokio::test]
async fn sudoers_follow_the_devices_and_the_last_removal_cleans_up() {
    let e = env();
    task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "a", "path": "/volume1/backup/a", "public_key": KEY}))).await;
    task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "b", "path": "/volume1/backup/b", "public_key": KEY}))).await;
    let mut roots = e.host.sudo_roots.lock().unwrap().clone().unwrap();
    roots.sort();
    assert_eq!(roots, ["/volume1/backup/a", "/volume1/backup/b"]);
    // an overlapping folder is refused before the task starts
    let (s, _) = call(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "c", "path": "/volume1/backup/a/sub"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    task(&e, "DELETE", "/v1/pc-backups/targets/a", None).await;
    assert_eq!(e.host.sudo_roots.lock().unwrap().clone().unwrap(), ["/volume1/backup/b"]);
    assert!(*e.host.account.lock().unwrap());
    task(&e, "DELETE", "/v1/pc-backups/targets/b", None).await;
    // the last one: account, sudoers and keys go; the folders go back to root
    assert!(!*e.host.account.lock().unwrap());
    assert!(e.host.sudo_roots.lock().unwrap().is_none());
    assert!(keys(&e).is_empty());
    assert_eq!(e.host.released.lock().unwrap().len(), 2);
}
