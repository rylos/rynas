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
use rynasd::pc_backup::{MemoryReceive, PcBackup, ReceiveHost};
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
    // deletes off: the PC can only remove its own interrupted receives
    assert!(k.contains("--root /volume1/backup/laptop --delete-interrupted\" ssh-ed25519") && !k.contains("--delete "), "{k}");
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

/// A complete snapshot received `days_ago` with its own subvolume UUID.
fn snap_u(group: &str, days_ago: i64) -> Entry {
    let mut s = snap(group, days_ago);
    s.info.as_mut().unwrap().uuid = Some(format!("{group}-{days_ago}"));
    s
}

fn folder(e: &Env, path: &str, mut entries: Vec<Entry>) {
    entries.sort_by(|a, b| rynas_btrfs::name::compare_names(&b.name, &a.name));
    e.host.folders.lock().unwrap().insert(path.into(), (true, entries));
}

async fn detail(e: &Env, device: &str) -> PcTargetDetail {
    let (s, b) = call(e, "GET", &format!("/v1/pc-backups/targets/{device}"), None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    serde_json::from_slice(&b).unwrap()
}

#[tokio::test]
async fn nas_retention_keeps_the_parent_and_the_pc_can_only_add() {
    let e = env();
    let path = "/volume1/backup/pc";
    // one run a day for 20 days, home and root
    folder(&e, path, (1..=20).flat_map(|d| [snap_u("home", d), snap_u("root", d)]).collect());
    let retention = json!({"owner": "nas", "gfs": {"hourly": 0, "daily": 6, "weekly": 0, "monthly": 0, "yearly": 0, "min": "latest"}});
    let t = task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "pc", "path": path, "adopt": true, "public_key": KEY, "retention": retention}))).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    // the key cannot delete snapshots, only interrupted receives
    let k = keys(&e);
    assert!(k.contains("--root /volume1/backup/pc --delete-interrupted\"") && !k.contains("--delete "), "{k}");
    let d = detail(&e, "pc").await;
    assert!(d.client_config.contains("target_preserve_min    all"));
    assert_eq!(d.runs.len(), 20);
    assert!(d.runs.iter().all(|r| r.lacks.is_empty()));
    // the preview: per subvolume the parent and 4 more dailies stay
    let (s, b) = call(&e, "POST", "/v1/pc-backups/plan/retention", Some(json!({"device": "pc", "retention": retention}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let p: PcRetentionPreview = serde_json::from_slice(&b).unwrap();
    assert_eq!(p.items.len(), 40);
    assert_eq!(p.delete, 30, "{:?}", p.items);
    assert_eq!(p.frees_at_least_bytes, None);
    let parents: Vec<&RetentionItem> = p.items.iter().filter(|i| i.reason == RetentionReason::Parent).collect();
    assert_eq!(parents.len(), 2);
    let (s, b) = call(&e, "POST", "/v1/pc-backups/plan/retention", Some(json!({"device": "pc", "retention": {"owner": "nas", "gfs": {"min": "1x"}}}))).await;
    assert_eq!((s, err(&b).field.as_deref()), (StatusCode::BAD_REQUEST, Some("retention.gfs.min")));
    // the parent cannot be deleted by hand; an old snapshot can
    let parent = parents[0].name.clone();
    let (s, b) = call(&e, "DELETE", &format!("/v1/pc-backups/targets/pc/snapshots/{parent}"), None).await;
    assert_eq!((s, err(&b).field.as_deref()), (StatusCode::CONFLICT, Some("snapshot")));
    let old = snap("home", 20).name;
    let t = task(&e, "DELETE", &format!("/v1/pc-backups/targets/pc/snapshots/{old}"), None).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    // a pass while a receive runs does nothing; then it deletes what the preview said
    e.host.busy.lock().unwrap().push(path.into());
    let t = task(&e, "POST", "/v1/pc-backups/targets/pc/prune", None).await;
    assert_eq!(t.status, TaskStatus::Error);
    assert!(list(&e).await[0].last_prune.as_ref().unwrap().error.is_some());
    e.host.busy.lock().unwrap().clear();
    let t = task(&e, "POST", "/v1/pc-backups/targets/pc/prune", None).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    assert_eq!(e.host.deleted.lock().unwrap().len(), 30);
    let d = detail(&e, "pc").await;
    assert_eq!(d.list.len(), 10);
    assert!(d.list.iter().any(|s| s.name == parent && s.parent));
    assert_eq!(d.target.last_prune.as_ref().map(|r| (r.deleted, r.error.is_none())), Some((29, true)));
    // a PC that deletes for itself has no NAS passes
    task(&e, "PUT", "/v1/pc-backups/targets/pc", Some(json!({"public_key": KEY, "alert_days": 3, "delete": {"allowed": true, "keep_last": 1, "min_age_days": 0}, "retention": {"owner": "pc"}}))).await;
    let (s, _) = call(&e, "POST", "/v1/pc-backups/targets/pc/prune", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(keys(&e).contains("--delete --keep-last 1"));
}

#[tokio::test]
async fn sizes_bare_metal_and_the_new_alerts() {
    let e = env();
    let path = "/volume1/backup/pc";
    let mut entries: Vec<Entry> = (1..=3).flat_map(|d| [snap_u("@", d), snap_u("@home", d)]).collect();
    // @games stopped 10 days ago; an interrupted receive from yesterday
    entries.push(snap_u("@games", 10));
    let mut broken = snap_u("@home", 0);
    broken.info.as_mut().unwrap().readonly = false;
    broken.info.as_mut().unwrap().received_uuid = None;
    broken.info.as_mut().unwrap().otime -= 3600;
    entries.push(broken);
    folder(&e, path, entries);
    // the newest @ snapshot carries a manifest that needs @ and @home
    let newest_root = snap("@", 1).name;
    e.host.manifests.lock().unwrap().insert(
        PathBuf::from(path).join(&newest_root),
        serde_json::to_vec(&json!({"format": 1, "boot": {"loader": "grub"}, "btrbk": {"subvolumes": [{"name": "@", "mount": "/"}, {"name": "@home", "mount": "/home"}, {"name": "@games", "mount": "/home/games"}]}})).unwrap(),
    );
    *e.host.space.lock().unwrap() = Some((10 << 30, 1 << 40));
    task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "pc", "path": path, "adopt": true, "alert_hours": 48}))).await;
    let d = detail(&e, "pc").await;
    let b = &d.target.bare_metal;
    assert_eq!(b.manifest_snapshot.as_deref(), Some(newest_root.as_str()));
    assert_eq!(b.loader.as_deref(), Some("grub"));
    // @games has a snapshot: ready, but the newest run lacks it
    assert!(b.ready, "{b:?}");
    assert_eq!(b.warnings.iter().map(|w| (w.code, w.subject.clone())).collect::<Vec<_>>(), [(BareMetalCode::LatestRunIncomplete, Some("@games".into()))]);
    let codes: Vec<PcIssueCode> = d.target.issues.iter().map(|i| i.code).collect();
    assert_eq!(codes, [PcIssueCode::SubvolumeLate, PcIssueCode::ReceiveFailed, PcIssueCode::LowSpace]);
    assert!(d.list.iter().all(|s| s.size.is_none()) && !d.sizes_from_quota);
    // measure: every complete snapshot once; the second run has nothing to do
    let t = task(&e, "POST", "/v1/pc-backups/targets/pc/sizes", None).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    assert_eq!(e.host.measured.lock().unwrap().len(), 7);
    task(&e, "POST", "/v1/pc-backups/targets/pc/sizes", None).await;
    assert_eq!(e.host.measured.lock().unwrap().len(), 7);
    let d = detail(&e, "pc").await;
    assert!(d.list.iter().filter(|s| !s.incomplete).all(|s| s.size.is_some_and(|z| !z.from_quota)));
    assert!(d.runs[1].referenced_bytes.is_some());
    // the alerts fire once each and close when it is over
    e.svc.check_freshness();
    e.svc.check_freshness();
    let codes = |e: &Env| e.notifier.0.lock().unwrap().iter().map(|ev| (ev.code.clone(), ev.state)).collect::<Vec<_>>();
    let c = codes(&e);
    assert_eq!(c.len(), 3, "{c:?}");
    assert!(c.iter().all(|x| x.1 == EventState::Firing));
    assert!(c.iter().any(|x| x.0 == "pcbackup.subvolume_late") && c.iter().any(|x| x.0 == "pcbackup.receive_failed") && c.iter().any(|x| x.0 == "pcbackup.low_space"));
    *e.host.space.lock().unwrap() = Some((900 << 30, 1 << 40));
    task(&e, "PUT", "/v1/pc-backups/targets/pc", Some(json!({"alert_days": 2, "alert_hours": 48, "delete": {"allowed": true, "keep_last": 1, "min_age_days": 0}, "ignored_groups": ["@games"]}))).await;
    e.svc.check_freshness();
    let c = codes(&e);
    assert_eq!(c.len(), 5, "{c:?}");
    assert!(c[3..].iter().all(|x| x.1 == EventState::Resolved));
    assert!(c[3..].iter().any(|x| x.0 == "pcbackup.space_ok") && c[3..].iter().any(|x| x.0 == "pcbackup.subvolume_resumed"));
    // quota groups win when they are on
    *e.host.quota.lock().unwrap() = Some(d.list.iter().map(|s| (s.name.clone(), (5, 1))).collect());
    let d = detail(&e, "pc").await;
    assert!(d.sizes_from_quota && d.list.iter().all(|s| s.size.is_some_and(|z| z.from_quota && z.referenced_bytes == 5)));
}

#[tokio::test]
async fn no_manifest_is_not_ready() {
    let e = env();
    folder(&e, "/volume1/backup/pc", vec![snap_u("@", 1)]);
    task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "pc", "path": "/volume1/backup/pc", "adopt": true}))).await;
    let b = list(&e).await.remove(0).bare_metal;
    assert!(!b.ready);
    assert_eq!(b.missing.iter().map(|g| g.code).collect::<Vec<_>>(), [BareMetalCode::NoManifest]);
}

// ------------------------------------------------------------------ rynas-agent's btrfs method

/// The memory host plus the agent side (`/v1/pcbackup/…`) linked to it, as rynasd wires them.
fn env_agent() -> (Env, Arc<rynasd::pc_agent::PcAgent>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let state_dir = dir.path().join("state");
    std::fs::create_dir_all(&root).unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let config = Arc::new(Config::open(ConfigOptions::new(&state_dir, &root), sys, Vec::new()).unwrap());
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let mut ops = Ops::new(config, tasks);
    // the agent side's console takes the administrators from Ops
    ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], groups);
    let ops = Arc::new(ops);
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
        state_dir.clone(),
        AdminPolicy::new(vec!["rynas-admin".into()], groups),
        "nas".into(),
    );
    // no worker is ever started: a btrfs PC has no repository
    let agent = rynasd::pc_agent::PcAgent::new(
        ops.clone(),
        notifier.clone(),
        state_dir,
        true,
        rynas_backup::Launch::Direct { exe: "/nonexistent/rynas-backup-worker".into() },
        Arc::new(tokio::sync::Semaphore::new(1)),
        None,
        "nas".into(),
    );
    agent.bind_btrfs(svc.clone());
    st.pc_backup = Some(svc.clone());
    st.pc_agent = Some(agent.clone());
    (Env { _dir: dir, app: router(st), ops, host, svc, notifier }, agent)
}

/// An agent call: device token (or none), raw body.
async fn agent_call(e: &Env, token: Option<&str>, method: &str, uri: &str, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let ct = if body.first() == Some(&b'{') { "application/json" } else { "application/zstd" };
    let mut req = Request::builder().method(method).uri(uri).header(header::CONTENT_TYPE, ct);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let r = e.app.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec())
}

fn zst(data: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(data, 3).unwrap()
}

/// A btrbk `long` name for the subvolume, `hours_ago` back (the receive refuses future names).
fn agent_name(prefix: &str, hours_ago: i64) -> String {
    format!("{prefix}.{}", rynas_btrfs::name::long_timestamp(now_s() - hours_ago * 3600))
}

async fn listing(e: &Env, token: &str) -> rynas_api_types::pcagent::AgentBtrfsListing {
    let (s, b) = agent_call(e, Some(token), "GET", "/v1/pcbackup/agent/btrfs/snapshots", Vec::new()).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    serde_json::from_slice(&b).unwrap()
}

#[tokio::test]
async fn agent_folder_pairing_listing_and_receive() {
    use rynas_btrfs::send_stream::fake::stream;
    let (e, _agent) = env_agent();
    let path = "/volume1/backup/laptop";
    // an agent folder takes no key
    let (s, b) = call(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "laptop", "path": path, "agent": true, "public_key": KEY}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&b));
    let t = task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "laptop", "path": path, "agent": true, "retention": {"owner": "pc"}}))).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    let tg = list(&e).await.remove(0);
    assert!(tg.agent);
    assert_eq!(tg.retention.owner, RetentionOwner::Nas, "the agent never deletes: retention is the NAS's");
    // no SSH account, key line or sudoers line for it
    assert!(!keys(&e).contains("laptop"));
    assert!(e.host.sudo_roots.lock().unwrap().is_none());
    assert_eq!(e.host.folder_meta(std::path::Path::new(path)), Some((0, 0, 0o700)));
    assert!(detail(&e, "laptop").await.manifest_hook.is_empty());

    // pairing into it: the device takes the folder's name, no repository
    let (s, _) = call(&e, "POST", "/v1/pcbackup/pairings", Some(json!({"btrfs_target": "nope"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, b) = call(&e, "POST", "/v1/pcbackup/pairings", Some(json!({"btrfs_target": "laptop", "description": "desk"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let code = serde_json::from_slice::<rynas_api_types::pcagent::PairingCreated>(&b).unwrap().code;
    let pair = json!({"code": code, "hostname": "some-host"}).to_string().into_bytes();
    let (s, b) = agent_call(&e, None, "POST", "/v1/pcbackup/agent/pair", pair).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let p: rynas_api_types::pcagent::PairResponse = serde_json::from_slice(&b).unwrap();
    assert_eq!((p.device.as_str(), p.group.as_str(), p.repo_password.as_str()), ("laptop", "", ""));
    assert_eq!(p.plan.method, rynas_api_types::pcagent::PcMethod::Btrfs);
    let tok = p.token.clone();
    // a second PC cannot take the name
    let (s, _) = call(&e, "POST", "/v1/pcbackup/pairings", Some(json!({"btrfs_target": "laptop"}))).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // listing: token needed
    let (s, _) = agent_call(&e, None, "GET", "/v1/pcbackup/agent/btrfs/snapshots", Vec::new()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = agent_call(&e, Some("0000"), "GET", "/v1/pcbackup/agent/btrfs/snapshots", Vec::new()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert!(listing(&e, &tok).await.snapshots.is_empty());

    // a full send, then an incremental one on it
    let n1 = agent_name("@home", 2);
    let recv = |snap: &str, parent: &str| format!("/v1/pcbackup/agent/btrfs/receive?snapshot={snap}&parent={parent}");
    let u1 = "0d7dc2d7-878a-5446-8596-6b5b6b021473";
    let (s, b) = agent_call(&e, Some(&tok), "POST", &recv(&n1, "none"), zst(&stream(&n1, u1, None))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let r1: rynas_api_types::pcagent::AgentBtrfsReceived = serde_json::from_slice(&b).unwrap();
    assert_eq!(r1.name, n1);
    assert!(r1.bytes > 0);
    let l = listing(&e, &tok).await;
    assert_eq!(l.snapshots.len(), 1);
    assert!(l.snapshots[0].complete);
    assert_eq!(l.snapshots[0].received_uuid.as_deref(), Some(r1.received_uuid.as_str()));
    // the same name again
    let (s, b) = agent_call(&e, Some(&tok), "POST", &recv(&n1, "none"), zst(&stream(&n1, u1, None))).await;
    assert_eq!((s, err(&b).code.as_str()), (StatusCode::CONFLICT, "already_exists"));
    let n2 = agent_name("@home", 1);
    let u2 = "4704ff2c-f735-d741-b649-cc1f58aea7f6";
    let (s, b) = agent_call(&e, Some(&tok), "POST", &recv(&n2, &n1), zst(&stream(&n2, u2, Some(&r1.received_uuid)))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(listing(&e, &tok).await.snapshots.len(), 2);

    // refused before anything is received: bad names, another subvolume's parent, a parent
    // the NAS does not have, a busy folder
    let n3 = agent_name("@home", 0);
    for (uri, code) in [
        (recv("x", "none"), StatusCode::BAD_REQUEST),
        (recv("@home.29991231T0000", "none"), StatusCode::BAD_REQUEST),
        (recv("../x.20261112T0300", "none"), StatusCode::BAD_REQUEST),
        (recv(&n3, &agent_name("@", 2)), StatusCode::BAD_REQUEST),
        (recv(&n3, &n3), StatusCode::BAD_REQUEST),
    ] {
        let (s, b) = agent_call(&e, Some(&tok), "POST", &uri, zst(&stream(&n3, u2, None))).await;
        assert_eq!(s, code, "{uri}: {}", String::from_utf8_lossy(&b));
    }
    // a parent the NAS does not have: its own code, so the agent sends in full
    let (s, b) = agent_call(&e, Some(&tok), "POST", &recv(&n3, &agent_name("@home", 30)), zst(&stream(&n3, u2, None))).await;
    assert_eq!((s, err(&b).code.as_str()), (StatusCode::CONFLICT, rynas_api_types::pcagent::PARENT_MISSING));
    e.host.busy.lock().unwrap().push(path.into());
    let (s, b) = agent_call(&e, Some(&tok), "POST", &recv(&n3, "none"), zst(&stream(&n3, u2, None))).await;
    assert_eq!((s, err(&b).code.as_str()), (StatusCode::CONFLICT, "busy"));
    e.host.busy.lock().unwrap().clear();

    // streams that do not match what was announced fail, and what they left goes at once
    for body in [
        zst(&stream("evil.20261112T0300", u2, None)),
        zst(&stream(&n3, u2, Some(u1))),
        zst(b"not a stream"),
        b"not zstd".to_vec(),
        Vec::new(),
    ] {
        let (s, b) = agent_call(&e, Some(&tok), "POST", &recv(&n3, "none"), body).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{}", String::from_utf8_lossy(&b));
    }
    let l = listing(&e, &tok).await;
    assert_eq!(l.snapshots.len(), 2, "{l:?}");
    // what they left was in the staging folder, and went with it
    let deleted = e.host.deleted.lock().unwrap().clone();
    assert!(!deleted.is_empty());
    assert!(deleted.iter().all(|d| d.starts_with(&format!("{path}/.incoming/"))), "{deleted:?}");
    assert!(e.host.staged.lock().unwrap().is_empty());
    // a stream that leaves something next to its subvolume (a file through a symlink, say):
    // the snapshot never reaches the folder
    e.host.staged_extra.lock().unwrap().push("big-file".into());
    let (s, b) = agent_call(&e, Some(&tok), "POST", &recv(&n3, "none"), zst(&stream(&n3, u2, None))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{}", String::from_utf8_lossy(&b));
    assert!(err(&b).message.contains("entries"), "{}", String::from_utf8_lossy(&b));
    assert_eq!(listing(&e, &tok).await.snapshots.len(), 2);
    assert!(e.host.staged.lock().unwrap().is_empty());
    e.host.staged_extra.lock().unwrap().clear();
    // a stream that names another subvolume (the receive created another name)
    *e.host.receive_as.lock().unwrap() = Some("other.20261112T0300".into());
    let (s, _) = agent_call(&e, Some(&tok), "POST", &recv(&n3, "none"), zst(&stream(&n3, u2, None))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    *e.host.receive_as.lock().unwrap() = None;
    assert_eq!(listing(&e, &tok).await.snapshots.len(), 2);
    // incremental on a parent whose UUID the stream does not carry
    let (s, _) = agent_call(&e, Some(&tok), "POST", &recv(&n3, &n2), zst(&stream(&n3, u2, Some(u1)))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    // not enough room on the volume
    *e.host.space.lock().unwrap() = Some((512 << 20, 4 << 30));
    let (s, _) = agent_call(&e, Some(&tok), "POST", &recv(&n3, "none"), zst(&stream(&n3, u2, None))).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    *e.host.space.lock().unwrap() = None;

    // the PC keeps the btrfs method (no repository to fall back to), gets a restore code
    // for its btrfs snapshots (B3), and its folder's runs and retention are the B1 ones
    let dev: Value = serde_json::from_slice(&call(&e, "GET", "/v1/pcbackup/devices/laptop", None).await.1).unwrap();
    let mut settings = dev["device"]["settings"].clone();
    settings["method"] = json!("files");
    let (s, _) = call(&e, "PUT", "/v1/pcbackup/devices/laptop", Some(settings)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&e, "POST", "/v1/pcbackup/devices/laptop/restore-code", None).await;
    assert_eq!(s, StatusCode::CREATED);
    let d = detail(&e, "laptop").await;
    assert_eq!(d.list.len(), 2);
    assert_eq!(d.runs.len(), 2);
    assert!(d.list.iter().any(|x| x.parent && x.name == n2));

    // switching the folder to btrbk over SSH: the key line comes, the agent's receive stops
    task(&e, "PUT", "/v1/pc-backups/targets/laptop", Some(json!({"agent": false, "public_key": KEY, "alert_days": 3, "delete": {"allowed": false, "keep_last": 1, "min_age_days": 0}}))).await;
    assert!(keys(&e).contains("--device laptop"));
    let (s, _) = agent_call(&e, Some(&tok), "GET", "/v1/pcbackup/agent/btrfs/snapshots", Vec::new()).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // and back: the key line goes, the chain is still there
    let t = task(&e, "PUT", "/v1/pc-backups/targets/laptop", Some(json!({"agent": true, "alert_days": 3, "delete": {"allowed": false, "keep_last": 1, "min_age_days": 0}}))).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    assert!(!keys(&e).contains("--device laptop"));
    assert_eq!(listing(&e, &tok).await.snapshots.len(), 2);

    // revoked: the token stops, the snapshots stay
    let t = task(&e, "DELETE", "/v1/pcbackup/devices/laptop", None).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    let (s, _) = agent_call(&e, Some(&tok), "GET", "/v1/pcbackup/agent/btrfs/snapshots", Vec::new()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(detail(&e, "laptop").await.list.len(), 2);
}

#[tokio::test]
async fn an_agent_folder_removed_and_made_again_does_not_take_the_old_token() {
    use rynas_btrfs::send_stream::fake::stream;
    let (e, agent) = env_agent();
    let mk = |dev: &'static str| json!({"device": dev, "path": format!("/volume1/backup/{dev}"), "agent": true});
    for d in ["laptop", "desk"] {
        let t = task(&e, "POST", "/v1/pc-backups/targets", Some(mk(d))).await;
        assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    }
    let (_, b) = call(&e, "POST", "/v1/pcbackup/pairings", Some(json!({"btrfs_target": "laptop"}))).await;
    let code = serde_json::from_slice::<rynas_api_types::pcagent::PairingCreated>(&b).unwrap().code;
    let (_, b) = agent_call(&e, None, "POST", "/v1/pcbackup/agent/pair", json!({"code": code, "hostname": "h"}).to_string().into_bytes()).await;
    let tok = serde_json::from_slice::<rynas_api_types::pcagent::PairResponse>(&b).unwrap().token;
    let n1 = agent_name("@", 2);
    let uri = format!("/v1/pcbackup/agent/btrfs/receive?snapshot={n1}&parent=none");
    let (s, b) = agent_call(&e, Some(&tok), "POST", &uri, zst(&stream(&n1, "0d7dc2d7-878a-5446-8596-6b5b6b021473", None))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    // the receive did not see the other PC's folder
    assert_eq!(*e.host.hidden.lock().unwrap(), ["/volume1/backup/desk"]);

    // the folder goes: the PC (btrfs method only) is revoked with it
    let t = task(&e, "DELETE", "/v1/pc-backups/targets/laptop", None).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    assert!(agent.snapshot().devices["laptop"].revoked);
    // made again under the same name: the old token gets nowhere
    let t = task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "laptop", "path": "/volume1/backup/laptop2", "agent": true}))).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    let (s, _) = agent_call(&e, Some(&tok), "GET", "/v1/pcbackup/agent/btrfs/snapshots", Vec::new()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // a PC still bound to an older folder of the same name (the binding is the folder's
    // identity, not its name) is refused until the method is chosen again
    agent
        .edit(|r| {
            let d = r.devices.get_mut("laptop").unwrap();
            d.revoked = false;
            d.token_sha = rynasd::pc_agent::sha_hex(tok.as_bytes());
            Ok(())
        })
        .unwrap();
    let (s, b) = agent_call(&e, Some(&tok), "GET", "/v1/pcbackup/agent/btrfs/snapshots", Vec::new()).await;
    assert_eq!(s, StatusCode::CONFLICT, "{}", String::from_utf8_lossy(&b));
    let dev: Value = serde_json::from_slice(&call(&e, "GET", "/v1/pcbackup/devices/laptop", None).await.1).unwrap();
    let mut settings = dev["device"]["settings"].clone();
    settings["method"] = json!("files");
    // (no repository: files is refused) — choosing btrfs again rebinds only when it changes
    let (s, _) = call(&e, "PUT", "/v1/pcbackup/devices/laptop", Some(settings.clone())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    agent.edit(|r| {
        r.devices.get_mut("laptop").unwrap().btrfs_folder = None;
        Ok(())
    })
    .unwrap();
    settings["method"] = json!("btrfs");
    let (s, b) = call(&e, "PUT", "/v1/pcbackup/devices/laptop", Some(settings)).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(listing(&e, &tok).await.snapshots.len(), 0);
}

#[tokio::test]
async fn switching_a_btrbk_folder_to_the_agent_keeps_the_chain() {
    let (e, agent) = env_agent();
    folder(&e, "/volume1/backup/pc", vec![snap_u("@", 2), snap_u("@home", 2)]);
    task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": "pc", "path": "/volume1/backup/pc", "adopt": true, "public_key": KEY}))).await;
    assert!(keys(&e).contains("--device pc"));
    // a file PC of that name cannot switch to btrfs while the folder is btrbk's
    assert!(!agent.btrfs.get().unwrap().has_agent_target("pc"));
    let t = task(&e, "PUT", "/v1/pc-backups/targets/pc", Some(json!({"agent": true, "public_key": "", "alert_days": 3, "delete": {"allowed": true, "keep_last": 1, "min_age_days": 0}}))).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    assert!(agent.btrfs.get().unwrap().has_agent_target("pc"));
    // the last SSH PC went: no account, sudoers or keys left behind
    assert!(e.host.sudo_roots.lock().unwrap().is_none());
    let tg = list(&e).await.remove(0);
    assert!(tg.agent && tg.public_key.is_empty());
    assert_eq!(tg.retention.owner, RetentionOwner::Nas);
    assert_eq!(tg.snapshots, 2);
    // a key with the agent is refused
    let (s, _) = call(&e, "PUT", "/v1/pc-backups/targets/pc", Some(json!({"public_key": KEY, "alert_days": 3, "delete": {"allowed": true, "keep_last": 1, "min_age_days": 0}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}


/// B3: a restore session reads the btrfs snapshots of its own PC only (a btrbk folder with
/// no PC record here): listing with runs, the manifest, the legacy files, the send stream
/// (full and on a parent), its refusals, the scope, the report that ends it.
#[tokio::test]
async fn btrfs_restore_session_reads_only_its_own_pc() {
    use rynas_api_types::pcagent::{RestoreBtrfsListing, RestoreChoice, RestoreLegacy, RestoreSession};
    let (e, _agent) = env_agent();
    let (path, other) = ("/volume1/backup/casa", "/volume1/backup/other");
    for (d, p) in [("casa", path), ("other", other)] {
        let t = task(&e, "POST", "/v1/pc-backups/targets", Some(json!({"device": d, "path": p, "public_key": KEY}))).await;
        assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    }
    let mut broken = snap_u("@home", 0);
    broken.info.as_mut().unwrap().readonly = false;
    broken.info.as_mut().unwrap().received_uuid = None;
    let broken_name = broken.name.clone();
    folder(&e, path, vec![snap_u("@", 1), snap_u("@home", 1), snap_u("@", 2), snap_u("@home", 2), broken]);
    folder(&e, other, vec![snap_u("@", 1)]);
    let (newest, older, home) = (snap("@", 1).name, snap("@", 2).name, snap("@home", 1).name);
    e.host.manifests.lock().unwrap().insert(
        PathBuf::from(path).join(&newest),
        serde_json::to_vec(&json!({"format": 1, "written_at_ms": 1, "hostname": "casa", "boot": {"uefi": true, "loader": "systemd_boot", "initramfs": "mkinitcpio"}, "disks": [], "btrbk": {"snapshot_dir": "/mnt/pool/btrbk_snapshots", "subvolumes": [{"name": "@", "path": "@", "mount": "/"}, {"name": "@home", "path": "@home", "mount": "/home"}]}})).unwrap(),
    );
    e.host.legacy.lock().unwrap().insert(
        PathBuf::from(path).join(&older),
        RestoreLegacy { snapshot: older.clone(), fstab: Some("UUID=x / btrfs subvol=@ 0 0\n".into()), boot_backups: vec!["20261101_post".into()], ..Default::default() },
    );

    // a restore code for a folder of btrfs snapshots only (no PC record), none for nothing
    let (s, _) = call(&e, "POST", "/v1/pcbackup/devices/nope/restore-code", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, b) = call(&e, "POST", "/v1/pcbackup/devices/casa/restore-code", None).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    let code = serde_json::from_slice::<Value>(&b).unwrap()["code"].as_str().unwrap().to_string();
    let (s, b) = agent_call(&e, None, "POST", "/v1/pcbackup/agent/restore/login", json!({"code": code}).to_string().into_bytes()).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let sess: RestoreSession = serde_json::from_slice(&b).unwrap();
    let d = &sess.devices[0];
    assert_eq!((d.device.as_str(), d.group.as_str(), d.repo_password.as_str(), d.btrfs), ("casa", "", "", true));
    let tok = sess.token.clone();
    // the code was used up
    let (s, _) = agent_call(&e, None, "POST", "/v1/pcbackup/agent/restore/login", json!({"code": code}).to_string().into_bytes()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    let base = "/v1/pcbackup/agent/restore/btrfs";
    // the listing: every snapshot with its subvolume, the runs, the manifest's snapshot
    let (s, b) = agent_call(&e, Some(&tok), "GET", &format!("{base}/casa/snapshots"), Vec::new()).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let l: RestoreBtrfsListing = serde_json::from_slice(&b).unwrap();
    assert_eq!(l.snapshots.len(), 5);
    assert!(l.snapshots.iter().any(|x| x.name == broken_name && x.incomplete));
    assert!(l.runs.len() >= 2);
    assert_eq!(l.manifest_snapshot.as_deref(), Some(newest.as_str()));
    assert!(!l.agent);
    // scope: only this session's PC, only with the session's token
    let (s, _) = agent_call(&e, Some(&tok), "GET", &format!("{base}/other/snapshots"), Vec::new()).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = agent_call(&e, None, "GET", &format!("{base}/casa/snapshots"), Vec::new()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = agent_call(&e, Some("0000"), "GET", &format!("{base}/casa/send?snapshot={newest}"), Vec::new()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // the manifest as the PC wrote it; none, or a bad name
    let (s, b) = agent_call(&e, Some(&tok), "GET", &format!("{base}/casa/manifest?snapshot={newest}"), Vec::new()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["format"], 1);
    let (s, _) = agent_call(&e, Some(&tok), "GET", &format!("{base}/casa/manifest?snapshot={home}"), Vec::new()).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    for bad in ["../x.20261101T0300", "x", ""] {
        let (s, _) = agent_call(&e, Some(&tok), "GET", &format!("{base}/casa/manifest?snapshot={bad}"), Vec::new()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
    }
    // the legacy files of a root snapshot without manifest
    let (s, b) = agent_call(&e, Some(&tok), "GET", &format!("{base}/casa/legacy?snapshot={older}"), Vec::new()).await;
    assert_eq!(s, StatusCode::OK);
    let lg: RestoreLegacy = serde_json::from_slice(&b).unwrap();
    assert_eq!(lg.boot_backups, vec!["20261101_post".to_string()]);

    // the stream: zstd of what btrfs send writes for a received snapshot
    let r = e.app.clone().oneshot(Request::builder().uri(format!("{base}/casa/send?snapshot={newest}")).header(header::AUTHORIZATION, format!("Bearer {tok}")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers().get(header::CONTENT_TYPE).unwrap(), "application/zstd");
    let raw = zstd::stream::decode_all(&to_bytes(r.into_body(), 1 << 20).await.unwrap()[..]).unwrap();
    let (name, uuid, clone) = rynas_btrfs::send_stream::first_subvolume(&raw).unwrap();
    assert_eq!((name.as_str(), uuid.as_deref(), clone), (newest.as_str(), Some("6f1c2a8e-0d3b-4c5e-9a7f-000000000001"), None));
    // on a parent of the same subvolume
    let (s, b) = agent_call(&e, Some(&tok), "GET", &format!("{base}/casa/send?snapshot={newest}&parent={older}"), Vec::new()).await;
    assert_eq!(s, StatusCode::OK);
    let raw = zstd::stream::decode_all(&b[..]).unwrap();
    assert!(rynas_btrfs::send_stream::first_subvolume(&raw).unwrap().2.is_some());
    assert_eq!(e.host.sent.lock().unwrap().len(), 2);
    // refused: an interrupted receive, another subvolume's parent, itself, a missing one
    for (q, code) in [
        (format!("snapshot={broken_name}"), StatusCode::NOT_FOUND),
        (format!("snapshot={newest}&parent={home}"), StatusCode::BAD_REQUEST),
        (format!("snapshot={newest}&parent={newest}"), StatusCode::BAD_REQUEST),
        ("snapshot=@.20200101T0000".to_string(), StatusCode::NOT_FOUND),
    ] {
        let (s, b) = agent_call(&e, Some(&tok), "GET", &format!("{base}/casa/send?{q}"), Vec::new()).await;
        assert_eq!(s, code, "{q}: {}", String::from_utf8_lossy(&b));
    }
    // a receive or a retention pass holds the folder: busy
    e.host.busy.lock().unwrap().push(path.into());
    let (s, b) = agent_call(&e, Some(&tok), "GET", &format!("{base}/casa/send?snapshot={newest}"), Vec::new()).await;
    assert_eq!((s, err(&b).code.as_str()), (StatusCode::CONFLICT, "busy"));
    e.host.busy.lock().unwrap().clear();
    assert_eq!(e.host.sent.lock().unwrap().len(), 2, "nothing was sent while busy");

    // the report ends the session (a btrbk folder has no PC history: the log is kept)
    let rep = json!({"device": "casa", "version": "btrfs x", "ok": true, "message": "restored", "log": "..."}).to_string().into_bytes();
    let (s, _) = agent_call(&e, Some(&tok), "POST", "/v1/pcbackup/agent/restore/report", rep).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = agent_call(&e, Some(&tok), "GET", &format!("{base}/casa/snapshots"), Vec::new()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // the administrator login's choice lists the btrfs folders too
    let (s, b) = call(&e, "POST", "/v1/pcbackup/restore-choice", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let c: RestoreChoice = serde_json::from_slice(&b).unwrap();
    let names: Vec<&str> = c.devices.iter().map(|d| d.device.as_str()).collect();
    assert_eq!(names, vec!["casa", "other"]);
}
