//! Space through rynasd's router, on a canned volume (fake subvolume markers): the overview
//! before and after a scan, pins, protected deletions refused, a preview, the settings and a
//! safe action, the low-space notification with its hysteresis.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use rynas_api_types::TaskStatus;
use rynas_api_types::space::{SpaceOverview, SpacePreview, SpaceSettings};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_notify::MemoryNotifier;
use rynas_tasks::{Cmd, TaskConfig, TaskManager};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::discover::fake::{TreeProbe, subvol};
use super::discover::Probe;
use super::{Env, Measured, Space};
use crate::ops::Ops;
use crate::roles::AdminPolicy;
use crate::server::{DaemonState, router};

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
    match u {
        "admin" => Some(vec!["rynas-admin".to_string()]),
        "bob" => Some(vec!["users".to_string()]),
        _ => None,
    }
}

/// A volume in a temporary folder; sizes set by the test.
struct TestEnv {
    vol: PathBuf,
    root: PathBuf,
    space: Mutex<(u64, u64)>,
    ran: Mutex<Vec<String>>,
}

impl Env for TestEnv {
    fn volumes(&self) -> Vec<(String, PathBuf, PathBuf)> {
        vec![("vol1".into(), self.vol.clone(), self.vol.clone())]
    }
    fn space(&self, _: &Path) -> Option<(u64, u64)> {
        Some(*self.space.lock().unwrap())
    }
    fn probe(&self) -> &dyn Probe {
        &TreeProbe
    }
    fn measure(&self, _: &Path, set: &[PathBuf], _keep: &[PathBuf], _: &dyn Fn() -> bool) -> Measured {
        Measured { freed: Some(set.len() as u64 * 100), at_least: Some(set.len() as u64 * 10), method: rynas_api_types::space::MeasureMethod::ExtentWalk }
    }
    fn tree_bytes(&self, path: &Path, stop: &dyn Fn() -> bool) -> Option<u64> {
        rynas_btrfs::extents::tree_bytes(path, stop).ok().map(|x| x.0)
    }
    fn delete_subvolume(&self, path: &Path) -> Result<(), String> {
        std::fs::remove_dir_all(path).map_err(|e| e.to_string())
    }
    fn remove_tree(&self, path: &Path) -> Result<(), String> {
        if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) }.map_err(|e| e.to_string())
    }
    fn sync(&self, _: &Path) -> Result<(), String> {
        Ok(())
    }
    fn root(&self) -> PathBuf {
        self.root.clone()
    }
    fn run(&self, c: Cmd) -> Result<String, String> {
        self.ran.lock().unwrap().push(c.display());
        Ok(String::new())
    }
}

struct World {
    app: axum::Router,
    ops: Arc<Ops>,
    env: Arc<TestEnv>,
    space: Arc<Space>,
    notes: Arc<MemoryNotifier>,
    _dir: tempfile::TempDir,
}

const RO: &str = r#"{"readonly": true}"#;

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let (root, state_dir, vol) = (dir.path().join("root"), dir.path().join("state"), dir.path().join("vol1"));
    std::fs::create_dir_all(&root).unwrap();
    // a deleted job's snapshots, a removed PC's received snapshots, a rollback copy
    for t in ["20261001T0100", "20261002T0100"] {
        subvol(&vol.join(format!(".snapshots/old/old.{t}")), RO);
    }
    subvol(&vol.join("pcs/gone/root.20261001T0100"), r#"{"readonly": true, "received_uuid": "x"}"#);
    std::fs::create_dir_all(vol.join("docs.rynas-rollback-20261105T101010/sub")).unwrap();
    std::fs::write(vol.join("docs.rynas-rollback-20261105T101010/sub/f"), vec![1u8; 9000]).unwrap();
    let config = Arc::new(
        Config::open(ConfigOptions::new(&state_dir, &root), Arc::new(MemorySystem::new("nas")), vec![Arc::new(super::domain::SpaceDomain)]).unwrap(),
    );
    let mut ops = Ops::new(config, TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap());
    ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], groups);
    let ops = Arc::new(ops);
    let notes = Arc::new(MemoryNotifier::default());
    let env = Arc::new(TestEnv { vol, root, space: Mutex::new((1000, 500)), ran: Mutex::new(Vec::new()) });
    let thumbs = dir.path().join("thumbs");
    std::fs::create_dir_all(thumbs.join("1000")).unwrap();
    std::fs::write(thumbs.join("1000/a.jpg"), vec![0u8; 4096]).unwrap();
    let pins = Arc::new(super::pins::Pins::at(super::pins::file(&state_dir)));
    let space = Space::new(ops.clone(), notes.clone(), env.clone(), state_dir, thumbs, dir.path().join("catalog"), pins);
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(MemoryStorage::default(), clock.clone(), "rynas"));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    st.space = Some(space.clone());
    World { app: router(st), ops, env, space, notes, _dir: dir }
}

async fn call(w: &World, who: &str, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri).header("x-rynas-user", who);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let body = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
    let r = w.app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 1 << 22).await.unwrap().to_vec())
}

async fn task(w: &World, b: &[u8]) -> rynas_api_types::TaskInfo {
    let id = serde_json::from_slice::<Value>(b).unwrap()["task_id"].as_str().unwrap().to_string();
    for _ in 0..500 {
        if let Some(t) = w.ops.tasks.get(&id)
            && t.status.is_finished()
        {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {id} still running");
}

async fn overview(w: &World) -> SpaceOverview {
    let (s, b) = call(w, "admin", "GET", "/v1/space", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    serde_json::from_slice(&b).unwrap()
}

#[tokio::test]
async fn scan_pin_preview_delete() {
    let w = world();
    let (s, _) = call(&w, "bob", "GET", "/v1/space", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let o = overview(&w).await;
    assert_eq!(o.volumes.len(), 1);
    assert_eq!((o.volumes[0].total_bytes, o.volumes[0].free_bytes), (1000, 500));
    assert!(o.volumes[0].scanned_at_ms.is_none());

    let (s, b) = call(&w, "admin", "POST", "/v1/space/scan", None).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert_eq!(task(&w, &b).await.status, TaskStatus::Ok);
    let o = overview(&w).await;
    let v = &o.volumes[0];
    assert!(v.scanned_at_ms.is_some());
    let old = v.sources.iter().find(|s| s.name == "old").expect("orphaned job");
    assert_eq!((old.count, old.freed_bytes, old.freed_at_least_bytes), (2, Some(200), Some(20)));
    assert!(v.sources.iter().any(|s| s.id.starts_with("pc-removed:")));
    let kinds: Vec<String> = o.review.iter().map(|r| format!("{:?}", r.kind)).collect();
    assert!(kinds.contains(&"OrphanSnapshots".to_string()) && kinds.contains(&"RollbackCopy".to_string()), "{kinds:?}");
    let leftover = o.review.iter().find(|r| r.label.contains("rynas-rollback")).unwrap();
    assert!(leftover.bytes.unwrap_or(0) > 0);
    let thumbs = o.safe_actions.iter().find(|a| a.kind == rynas_api_types::space::SafeActionKind::Thumbnails).unwrap();
    assert!(thumbs.available && thumbs.items == 1);

    // pins: only read-only snapshots; a pinned one is never deleted
    let first = old.snapshots[0].path.clone();
    let second = old.snapshots[1].path.clone();
    let (s, b) = call(&w, "admin", "POST", "/v1/space/pin", Some(json!({"path": w.env.vol.join("pcs").display().to_string()}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&b));
    assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["field"], "path");
    let (s, _) = call(&w, "admin", "POST", "/v1/space/pin", Some(json!({"path": first}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, b) = call(&w, "admin", "GET", "/v1/space/pins", None).await;
    assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["paths"], json!([first]));
    let o = overview(&w).await;
    let snap = o.volumes[0].sources.iter().flat_map(|s| &s.snapshots).find(|x| x.path == first).unwrap();
    assert!(snap.pinned);

    // confirm must be the number of snapshots
    let (s, b) = call(&w, "admin", "POST", "/v1/space/delete", Some(json!({"paths": [first, second], "confirm": 1}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["code"], rynas_api_types::error_codes::CONFIRMATION_REQUIRED);
    // a pinned one in the set: nothing at all is deleted
    let (s, b) = call(&w, "admin", "POST", "/v1/space/delete", Some(json!({"paths": [first, second], "confirm": 2}))).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let t = task(&w, &b).await;
    assert_eq!(t.status, TaskStatus::Error);
    assert!(Path::new(&first).exists() && Path::new(&second).exists());

    // the preview tells what can go and what is kept
    let (s, b) = call(&w, "admin", "POST", "/v1/space/preview", Some(json!({"paths": [first, second, "/vol9/nothing"]}))).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let id = serde_json::from_slice::<Value>(&b).unwrap()["task_id"].as_str().unwrap().to_string();
    assert_eq!(task(&w, &b).await.status, TaskStatus::Ok);
    let (_, b) = call(&w, "admin", "GET", &format!("/v1/space/previews/{id}"), None).await;
    let p: SpacePreview = serde_json::from_slice(&b).unwrap();
    assert!(p.done);
    assert_eq!(p.paths, [second.clone()]);
    assert_eq!(p.refused.len(), 2);
    assert_eq!(p.freed_bytes, Some(100));

    // unpinned: both go
    let (s, _) = call(&w, "admin", "POST", "/v1/space/unpin", Some(json!({"path": first}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, b) = call(&w, "admin", "POST", "/v1/space/delete", Some(json!({"paths": [first, second], "confirm": 2}))).await;
    let t = task(&w, &b).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    assert!(!Path::new(&first).exists() && !Path::new(&second).exists());

    // review: the rollback copy, by its id
    let (_, b) = call(&w, "admin", "POST", "/v1/space/review/delete", Some(json!({"ids": [leftover.id], "confirm": 1}))).await;
    assert_eq!(task(&w, &b).await.status, TaskStatus::Ok);
    assert!(!w.env.vol.join("docs.rynas-rollback-20261105T101010").exists());
}

#[tokio::test]
async fn settings_actions_and_low_space() {
    let w = world();
    let (s, b) = call(&w, "admin", "PUT", "/v1/space/settings", Some(json!({"low_free_pct": 30, "recover_pct": 20}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["field"], "recover_pct");
    let (s, b) = call(&w, "admin", "PUT", "/v1/space/settings", Some(json!({"auto_free": ["vol1"]}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let back: SpaceSettings = serde_json::from_slice(&b).unwrap();
    assert_eq!(back.auto_free, ["vol1"]);
    let (_, b) = call(&w, "admin", "GET", "/v1/space/settings", None).await;
    assert_eq!(serde_json::from_slice::<SpaceSettings>(&b).unwrap().auto_free, ["vol1"]);

    let (s, _) = call(&w, "admin", "POST", "/v1/space/actions/format-disk", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, b) = call(&w, "admin", "POST", "/v1/space/actions/thumbnails", None).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert_eq!(task(&w, &b).await.status, TaskStatus::Ok);
    assert!(!w.space.thumbs.join("1000/a.jpg").exists());
    let (_, b) = call(&w, "admin", "POST", "/v1/space/actions/apt-clean", None).await;
    assert_eq!(task(&w, &b).await.status, TaskStatus::Ok);
    assert!(w.env.ran.lock().unwrap().iter().any(|c| c.contains("apt-get clean")));

    // free space: 50 % → 5 %: low (once), 12 %: still low, 20 %: back to normal
    w.space.guard(&|_| {});
    assert!(w.notes.take().is_empty());
    *w.env.space.lock().unwrap() = (1000, 50);
    w.space.guard(&|_| {});
    w.space.guard(&|_| {});
    let n = w.notes.take();
    assert_eq!(n.iter().filter(|e| e.code == "space.low_free").count(), 1, "{n:?}");
    assert_eq!(n[0].actions[0].url, "/storage/space");
    *w.env.space.lock().unwrap() = (1000, 120);
    w.space.guard(&|_| {});
    assert!(w.notes.take().iter().all(|e| e.code != "space.free_ok"));
    *w.env.space.lock().unwrap() = (1000, 200);
    w.space.guard(&|_| {});
    assert_eq!(w.notes.take().iter().filter(|e| e.code == "space.free_ok").count(), 1);
}
