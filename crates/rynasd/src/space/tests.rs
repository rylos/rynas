//! Space through rynasd's router, on a canned volume (fake subvolume markers): the overview
//! before and after a scan, pins, protected deletions refused, a preview, the settings and a
//! safe action, the low-space notification with its hysteresis.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
use super::{Claim, Env, Measured, Space};
use rynas_api_types::notify::EventState;
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
    /// Folders a `btrfs receive` writes into now.
    receiving: Mutex<Vec<PathBuf>>,
    /// rynasd's state folder, and the day's count of automatic deletions the history file
    /// held at each deletion (it must be written after every one).
    state: PathBuf,
    count_at_delete: Mutex<Vec<u32>>,
    /// The kept members of every measurement.
    kept: Mutex<Vec<Vec<PathBuf>>>,
    /// The lines of every preview measurement.
    lines: Mutex<Vec<Vec<rynas_btrfs::freed::Line>>>,
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
    fn measure(&self, _: &Path, set: &[PathBuf], keep: &[PathBuf], stop: &dyn Fn() -> bool, progress: &dyn Fn(&str)) -> Measured {
        self.kept.lock().unwrap().push(keep.to_vec());
        if stop() {
            return Measured::default();
        }
        progress("measured");
        Measured { freed: Some(set.len() as u64 * 100), at_least: Some(set.len() as u64 * 10), method: rynas_api_types::space::MeasureMethod::ExtentWalk }
    }
    fn measure_lines(&self, mount: &Path, lines: &[rynas_btrfs::freed::Line], keep: &[PathBuf], stop: &dyn Fn() -> bool, progress: &dyn Fn(&str)) -> Measured {
        self.lines.lock().unwrap().push(lines.to_vec());
        let set: Vec<PathBuf> = lines.iter().flatten().filter(|x| x.1).map(|x| x.0.clone()).collect();
        self.measure(mount, &set, keep, stop, progress)
    }
    fn tree_bytes(&self, path: &Path, stop: &dyn Fn() -> bool) -> Option<u64> {
        rynas_btrfs::extents::tree_bytes(path, stop).ok().map(|x| x.0)
    }
    fn delete_subvolume(&self, path: &Path) -> Result<(), String> {
        let h: Value = std::fs::read(self.state.join("space/history.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        self.count_at_delete.lock().unwrap().push(h["thin"]["vol1"]["count"].as_u64().unwrap_or(0) as u32);
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
    fn receiving(&self, dir: &Path) -> bool {
        self.receiving.lock().unwrap().iter().any(|r| r.starts_with(dir))
    }
    fn fs_id(&self, path: &Path) -> Option<String> {
        path.exists().then(|| "test-fs".to_string())
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
    subvol(&vol.join("pcs/gone/root.20260901T0100"), r#"{"readonly": true, "received_uuid": "y"}"#);
    std::fs::create_dir_all(vol.join("docs.rynas-rollback-20261105T101010/sub")).unwrap();
    std::fs::write(vol.join("docs.rynas-rollback-20261105T101010/sub/f"), vec![1u8; 9000]).unwrap();
    let config = Arc::new(
        Config::open(ConfigOptions::new(&state_dir, &root), Arc::new(MemorySystem::new("nas")), vec![Arc::new(super::domain::SpaceDomain)]).unwrap(),
    );
    let mut ops = Ops::new(config, TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap());
    ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], groups);
    let ops = Arc::new(ops);
    let notes = Arc::new(MemoryNotifier::default());
    let env = Arc::new(TestEnv {
        vol,
        root,
        space: Mutex::new((1000, 500)),
        ran: Mutex::new(Vec::new()),
        receiving: Mutex::new(Vec::new()),
        state: state_dir.clone(),
        count_at_delete: Mutex::new(Vec::new()),
        kept: Mutex::new(Vec::new()),
        lines: Mutex::new(Vec::new()),
    });
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
    // received snapshots of a PC rynas has no record of: "owner unknown", the newest kept
    let received = v.sources.iter().find(|s| s.id.starts_with("received:")).expect("received source");
    assert_eq!(received.snapshots[0].protected, Some(rynas_api_types::space::Protection::Newest));
    assert_eq!(received.snapshots[1].protected, None);
    let kinds: Vec<String> = o.review.iter().map(|r| format!("{:?}", r.kind)).collect();
    assert!(kinds.contains(&"OrphanSnapshots".to_string()) && kinds.contains(&"RollbackCopy".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"ReceivedUnknownOwner".to_string()), "{kinds:?}");
    // never while a receive writes into the folder
    let old_received = received.snapshots[1].path.clone();
    w.env.receiving.lock().unwrap().push(w.env.vol.join("pcs/gone/.incoming"));
    let (_, b) = call(&w, "admin", "POST", "/v1/space/delete", Some(json!({"paths": [old_received], "confirm": 1}))).await;
    assert_eq!(task(&w, &b).await.status, TaskStatus::Error);
    assert!(Path::new(&old_received).exists());
    w.env.receiving.lock().unwrap().clear();
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
    assert_eq!(p.paths, std::slice::from_ref(&second));
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
async fn pins_go_only_with_their_snapshot_and_unreadable_config_stops_everything() {
    let w = world();
    let snaps = w.env.vol.join(".snapshots/old");
    let gone = snaps.join("old.20260101T0100").display().to_string();
    let kept_parent_missing = w.env.vol.join(".snapshots/nofolder/x.20260101T0100").display().to_string();
    let elsewhere = "/vol9/.snapshots/a/a.20260101T0100".to_string();
    let here = snaps.join("old.20261001T0100").display().to_string();
    for p in [&gone, &kept_parent_missing, &elsewhere, &here] {
        w.space.pins.set(p, true).unwrap();
    }
    w.space.prune_pins();
    let left = w.space.pins.all();
    // the folder is there on the mounted volume, the snapshot is not: forgotten
    assert!(!left.contains(&gone));
    // no folder, or a volume rynas does not see mounted: kept
    assert!(left.contains(&kept_parent_missing) && left.contains(&elsewhere) && left.contains(&here), "{left:?}");

    // a `snapshots` value that does not read: discovery stops, nothing is called "deleted job"
    let db = rusqlite::Connection::open(w._dir.path().join("state/config.db")).unwrap();
    db.execute("INSERT OR REPLACE INTO config (key, value, rev) VALUES ('snapshots', '{\"jobs\": 5}', 1)", []).unwrap();
    assert!(w.space.discover().is_err());
    let (_, b) = call(&w, "admin", "POST", "/v1/space/scan", None).await;
    assert_eq!(task(&w, &b).await.status, TaskStatus::Error);
    let (_, b) = call(&w, "admin", "POST", "/v1/space/delete", Some(json!({"paths": [here], "confirm": 1}))).await;
    assert_eq!(task(&w, &b).await.status, TaskStatus::Error);
    assert!(Path::new(&here).exists());
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

/// The claim of the automatic cleanup in the tests: given back when dropped.
struct Held(Arc<AtomicBool>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn automatic_cleanup_one_deletion_at_a_time() {
    let w = world();
    // a share job with four snapshots (its own lock file name: tests run in parallel)
    let job = format!("thin{}", std::process::id());
    let snaps = w.env.vol.join(format!(".snapshots/{job}"));
    subvol(&w.env.vol.join("docs"), r#"{"generation": 9}"#);
    for t in ["20261001T0100", "20261002T0100", "20261003T0100", "20261004T0100"] {
        subvol(&snaps.join(format!("{job}.{t}")), RO);
    }
    let cfg = json!({"jobs": [{"name": job, "subvolume": w.env.vol.join("docs"), "snapshot_dir": snaps}]});
    let db = rusqlite::Connection::open(w._dir.path().join("state/config.db")).unwrap();
    db.execute("INSERT OR REPLACE INTO config (key, value, rev) VALUES ('snapshots', ?1, 1)", [cfg.to_string()]).unwrap();
    *w.env.space.lock().unwrap() = (1000, 50);
    let settings = SpaceSettings { auto_free: vec!["vol1".into()], ..Default::default() };
    let (held, claims) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
    let claim = || -> Result<Claim, String> {
        assert!(!held.swap(true, Ordering::SeqCst), "the volume is given back between deletions");
        claims.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Held(held.clone())))
    };
    // the space never comes back: everything but the protected newest goes, one at a time
    let gone = w.space.thin("vol1", &w.env.vol, &settings, &claim, &|_| {});
    assert_eq!(gone.len(), 3, "{gone:?}");
    assert!(snaps.join(format!("{job}.20261004T0100")).exists());
    // the day's count was on disk before each next deletion
    assert_eq!(*w.env.count_at_delete.lock().unwrap(), [0, 1, 2]);
    assert_eq!(claims.load(Ordering::SeqCst), 4);
    let n = w.notes.take();
    assert_eq!(n.iter().filter(|e| e.code == "space.auto_freed").count(), 1, "{n:?}");
    let stuck: Vec<&rynas_api_types::notify::NewEvent> = n.iter().filter(|e| e.code == "space.auto_free_stuck").collect();
    assert_eq!(stuck.len(), 1);
    assert_eq!(stuck[0].key.as_deref(), Some("space:vol1:auto_free_stuck"));
    assert_eq!(stuck[0].state, EventState::Firing);
    // the next pass waits an hour; run anyway, it notifies nothing more within the hour
    assert!(w.space.thin("vol1", &w.env.vol, &settings, &claim, &|_| {}).is_empty());
    assert_eq!(claims.load(Ordering::SeqCst), 4);
    w.space.history.lock().unwrap().thin.get_mut("vol1").unwrap().cooldown_until_ms = 0;
    assert!(w.space.thin("vol1", &w.env.vol, &settings, &claim, &|_| {}).is_empty());
    assert!(w.notes.take().iter().all(|e| e.code != "space.auto_free_stuck"));
    // free space back: the problem resolves, quietly
    w.space.guard(&|_| {});
    *w.env.space.lock().unwrap() = (1000, 200);
    w.space.guard(&|_| {});
    let n = w.notes.take();
    assert!(n.iter().any(|e| e.key.as_deref() == Some("space:vol1:auto_free_stuck") && e.quiet && e.state == EventState::Resolved), "{n:?}");
    assert!(w.space.history.lock().unwrap().stuck_notified_ms.is_empty());
    // discovery of the affected volume only
    assert!(w.space.discover_in(Some("vol9")).unwrap().sources.is_empty());
    assert!(!w.space.discover_in(Some("vol1")).unwrap().sources.is_empty());
    // a pass out of time deletes nothing; a suspended volume is not thinned
    *w.env.space.lock().unwrap() = (1000, 50);
    w.space.history.lock().unwrap().thin.get_mut("vol1").unwrap().cooldown_until_ms = 0;
    let before = claims.load(Ordering::SeqCst);
    let (gone, stop) = w.space.thin_within("vol1", &w.env.vol, &settings, &claim, &|_| {}, Duration::ZERO);
    assert!(gone.is_empty());
    // the reason the task turns into "partial"
    assert_eq!(stop, Some(super::model::ThinStop::TimeCap));
    assert_eq!(w.space.thin_waits("vol1"), None);
    w.space.history.lock().unwrap().thin_suspended.insert("vol1".into());
    assert!(w.space.thin("vol1", &w.env.vol, &settings, &claim, &|_| {}).is_empty());
    assert_eq!(claims.load(Ordering::SeqCst), before);
    assert!(w.notes.take().is_empty());
}

#[tokio::test]
async fn the_preview_keeps_only_the_neighbours_and_has_a_time_limit() {
    let w = world();
    for t in ["20261003T0100", "20261004T0100", "20261005T0100", "20261006T0100"] {
        subvol(&w.env.vol.join(format!(".snapshots/old/old.{t}")), RO);
    }
    let p = |t: &str| w.env.vol.join(format!(".snapshots/old/old.{t}"));
    let set: Vec<String> = [p("20261004T0100"), p("20261003T0100")].iter().map(|x| x.display().to_string()).collect();
    let lines = Mutex::new(Vec::<String>::new());
    let log = |l: &str| lines.lock().unwrap().push(l.to_string());
    let r = w.space.run_preview(&set, &|| false, &log);
    assert_eq!(r.paths.len(), 2, "{:?}", r.refused);
    assert_eq!(r.freed_bytes, Some(200));
    assert_eq!(r.timed_out_minutes, None);
    // the kept snapshot on each side of the run, not the farther ones
    let mut kept = w.env.kept.lock().unwrap().last().cloned().unwrap();
    kept.sort();
    assert_eq!(kept, vec![p("20261002T0100"), p("20261005T0100")]);
    assert!(lines.lock().unwrap().iter().any(|l| l == "measured"));
    // the diff gets the whole line, oldest first, the selection marked
    let line = w.env.lines.lock().unwrap().last().cloned().unwrap();
    let marks: Vec<(String, bool)> = line[0].iter().map(|(x, s)| (x.file_name().unwrap().to_string_lossy().into_owned(), *s)).collect();
    let want: Vec<(String, bool)> = ["01", "02", "03", "04", "05", "06"].iter().map(|d| (format!("old.202610{d}T0100"), *d == "03" || *d == "04")).collect();
    assert_eq!((line.len(), marks), (1, want));

    // past the time limit: no number, and why
    let r = w.space.run_preview_within(&set, &|| false, &log, Duration::ZERO);
    assert_eq!((r.freed_bytes, r.freed_at_least_bytes), (None, None));
    assert_eq!(r.method, rynas_api_types::space::MeasureMethod::None);
    assert_eq!(r.timed_out_minutes, Some(0));
    assert!(r.done && r.error.is_none());
    assert!(lines.lock().unwrap().iter().any(|l| l.starts_with("not measured")));
}
