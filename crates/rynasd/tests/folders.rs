//! Share folders end to end with the in-memory system and filesystem: the snapshot folder
//! of a share, deleting a share with its folder (plan, typed confirmation, staged removal
//! of its snapshot job, commands at the end of the apply), the record of a share removed
//! without its folder, and the folders without a share (list, plan, deletion task).

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::{
    ApiError, FolderDeletePlan, FolderUsageKind, PendingChanges, ShareView, StagedChanges,
    TaskInfo, TaskStarted, UnsharedFolder, error_codes,
};
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
use rynas_services::removed::RemovedShares;
use rynas_services::{MemoryAccounts, MemoryShares, SharesDomain, SubvolumeCreator, UsersDomain};
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::ops::Ops;
use rynasd::protection::Protection;
use rynasd::roles::AdminPolicy;
use rynasd::server::{DaemonState, router};
use rynasd::services::Services;
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

/// Volume "vol1" mounted at /srv/vol1.
struct Volumes;

impl SubvolumeCreator for Volumes {
    fn volume_path(&self, volume: &str) -> Result<String, String> {
        (volume == "vol1")
            .then(|| "/srv/vol1".to_string())
            .ok_or_else(|| format!("volume {volume} is not mounted"))
    }
    fn create(&self, _: &str, _: bool, _: &dyn Fn(&str)) -> Result<(), String> {
        Ok(())
    }
    fn volumes(&self) -> Vec<(String, String)> {
        vec![("vol1".into(), "/srv/vol1".into())]
    }
}

struct Env {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    app: axum::Router,
    sys: Arc<MemorySystem>,
    host: Arc<MemoryShares>,
    config: Arc<Config>,
    ops: Arc<Ops>,
}

const UUID: &str = "11111111-2222-4333-8444-555555555555";

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let acc = Arc::new(MemoryAccounts::new());
    let host = Arc::new(MemoryShares::new());
    host.add_dir("/srv/vol1", Some("00000000-0000-4000-8000-000000000001"));
    host.add_dir("/srv/vol1/photos", Some(UUID));
    host.add_dir("/srv/vol1/photos/#recycle", None);
    host.add_dir("/srv/vol1/music", None);
    host.add_dir("/srv/vol1/old", None);
    host.add_dir("/srv/vol1/.snapshots", None);
    host.state().sizes.insert("/srv/vol1/photos".into(), 123_456);
    host.state().sizes.insert("/srv/vol1/old".into(), 1000);
    host.state().snapshots.insert(
        "/srv/vol1/.snapshots/photos".into(),
        vec!["photos.20261104T1200".into(), "photos.20261105T1200".into()],
    );
    let snap_host = Arc::new(MemorySnapshots::new());
    snap_host.add_subvolume("/srv/vol1/photos");
    let snapshots = Arc::new(SnapshotsDomain::new(snap_host));
    let scheduler = Arc::new(SchedulerDomain::new(Paths::default(), Arc::new(|_: &str| None)));
    let users = Arc::new(UsersDomain::new(acc.clone()));
    let shares = Arc::new(
        SharesDomain::new(host.clone(), acc)
            .with_subvolumes(Arc::new(Volumes))
            .with_removed(Arc::new(RemovedShares::in_dir(dir.path()))),
    );
    let config = Arc::new(
        Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            sys.clone(),
            vec![
                users.clone(),
                shares.clone(),
                snapshots.clone(),
                scheduler.clone(),
            ],
        )
        .unwrap(),
    );
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let mut ops = Ops::new(config.clone(), tasks);
    ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], |u| {
        (u == "admin").then(|| vec!["rynas-admin".into()])
    });
    let ops = Arc::new(ops);
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(MemoryStorage::default(), clock.clone(), "rynas"));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    st.services = Some(Arc::new(Services {
        ops: ops.clone(),
        users,
        shares,
    }));
    st.protection = Some(Arc::new(Protection::new(
        ops.clone(),
        scheduler,
        snapshots,
        Arc::new(FakeProbe::default()),
        Arc::new(History::open(&dir.path().join("scheduler.db")).unwrap()),
        Arc::new(MemoryNotifier::default()),
        dir.path().join("state"),
    )));
    Env {
        _dir: dir,
        root,
        app: router(st),
        sys,
        host,
        config,
        ops,
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
    serde_json::from_slice::<ApiError>(b).unwrap().code
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

fn runs(e: &Env) -> Vec<String> {
    std::mem::take(&mut e.sys.state().calls)
}

async fn folders(e: &Env) -> Vec<UnsharedFolder> {
    let (s, b) = call(e, "GET", "/v1/folders", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    serde_json::from_slice(&b).unwrap()
}

#[tokio::test]
async fn share_folders_from_snapshot_folder_to_deletion() {
    let e = env();
    for (name, path) in [("photos", "/srv/vol1/photos"), ("music", "/srv/vol1/music")] {
        let (s, b) = call(&e, "POST", "/v1/shares", Some(json!({"name": name, "path": path}))).await;
        assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    }
    let (s, b) = call(&e, "POST", "/v1/snapshots/jobs", Some(json!({"name": "photos", "share": "photos"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
    apply(&e);

    // the snapshot folder: a read-only bind of the job's directory
    let (_, b) = call(&e, "GET", "/v1/shares/photos", None).await;
    let mut v: ShareView = serde_json::from_slice(&b).unwrap();
    assert_eq!(v.share.smb.shadow_copy.snapshot_dir, "/srv/vol1/.snapshots/photos");
    v.share.show_snapshots = true;
    let (s, b) = call(&e, "PUT", "/v1/shares/photos", Some(serde_json::to_value(&v.share).unwrap())).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let v: ShareView = serde_json::from_slice(&b).unwrap();
    assert_eq!(v.status.warnings.len(), v.status.warning_items.len());
    runs(&e);
    apply(&e);
    let unit = e
        .root
        .join("etc/systemd/system/srv-vol1-photos-\\x23snapshot.mount");
    let text = std::fs::read_to_string(&unit).unwrap();
    assert!(text.contains("What=/srv/vol1/.snapshots/photos"), "{text}");
    assert!(runs(&e).iter().any(|c| c.contains("start --no-block srv-vol1-photos-\\x23snapshot.mount")));

    // folders without a share: "old" only (system names and shared folders stay out)
    let f = folders(&e).await;
    assert_eq!(f.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["old"]);
    assert_eq!(f[0].size_bytes, Some(1000));

    // the plan of deleting photos with its folder
    let (s, b) = call(&e, "POST", "/v1/shares/plan/delete", Some(json!({"share": "photos"}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let p: FolderDeletePlan = serde_json::from_slice(&b).unwrap();
    assert!(p.allowed, "{p:?}");
    assert_eq!((p.size_bytes, p.snapshots, p.recycle_bin, p.subvolume), (123_456, 2, true, true));
    assert!(p.usages.iter().any(|u| u.kind == FolderUsageKind::SnapshotJob && u.name == "photos"));

    // typed confirmation, then staged with its job and the job's scheduler task
    let (s, b) = call(&e, "DELETE", "/v1/shares/photos?remove_folder=true", None).await;
    assert_eq!((s, code(&b).as_str()), (StatusCode::BAD_REQUEST, error_codes::CONFIRMATION_REQUIRED));
    let (s, b) = call(&e, "DELETE", "/v1/shares/photos?remove_folder=true&confirm=photos", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(serde_json::from_slice::<StagedChanges>(&b).unwrap().staged, 3);
    let (_, b) = call(&e, "GET", "/v1/config/pending", None).await;
    let mut keys: Vec<String> = serde_json::from_slice::<PendingChanges>(&b)
        .unwrap()
        .changes
        .into_iter()
        .map(|c| c.key)
        .collect();
    keys.sort();
    assert_eq!(keys, ["scheduler", "shares", "snapshots"]);
    // the plain delete of music: its folder stays and is recorded
    let (s, _) = call(&e, "DELETE", "/v1/shares/music", None).await;
    assert_eq!(s, StatusCode::OK);
    runs(&e);
    apply(&e);
    let c = runs(&e);
    let stop = c.iter().position(|x| x.contains("stop srv-vol1-photos-\\x23snapshot.mount")).expect("unmounted");
    let snap = c.iter().position(|x| x.contains("btrfs subvolume delete /srv/vol1/.snapshots/photos/photos.20261104T1200")).expect("snapshots");
    let sub = c.iter().position(|x| x.contains("btrfs subvolume delete --recursive /srv/vol1/photos")).expect("folder");
    assert!(stop < snap && snap < sub, "{c:?}");
    assert!(!unit.exists());
    assert!(!c.iter().any(|x| x.starts_with("run ") && x.contains("/srv/vol1/music")));

    // music is now a folder without a share, with its previous settings
    let f = folders(&e).await;
    let music = f.iter().find(|x| x.name == "music").expect("music listed");
    assert_eq!(music.previous.as_ref().unwrap().name, "music");
    assert!(music.removed_at_ms.is_some());

    // deleting a folder without a share: plan, confirmation, task
    let (s, b) = call(&e, "POST", "/v1/folders/plan/delete", Some(json!({"volume": "vol1", "name": "old"}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert!(serde_json::from_slice::<FolderDeletePlan>(&b).unwrap().allowed);
    let (s, b) = call(&e, "POST", "/v1/folders/plan/delete", Some(json!({"volume": "vol1", "name": ".snapshots"}))).await;
    assert_eq!((s, code(&b).as_str()), (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE));
    let (s, b) = call(&e, "DELETE", "/v1/folders/vol1/old", None).await;
    assert_eq!((s, code(&b).as_str()), (StatusCode::BAD_REQUEST, error_codes::CONFIRMATION_REQUIRED));
    let (s, b) = call(&e, "DELETE", "/v1/folders/vol9/old?confirm=old", None).await;
    assert_eq!((s, code(&b).as_str()), (StatusCode::NOT_FOUND, error_codes::NOT_FOUND));
    let (s, b) = call(&e, "DELETE", "/v1/folders/vol1/old?confirm=old", None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    let t = wait_task(&e, &id).await;
    assert!(t.error.is_none(), "{t:?}");
    assert_eq!(t.kind, "shares.folder-delete");
    let c = runs(&e);
    assert!(
        c.iter().any(|x| x.contains("rm -rf --one-file-system --preserve-root=all -- /srv/vol1/old")),
        "{c:?}"
    );
    assert!(e.host.state().dir_log.contains(&"rmdir /srv/vol1/photos/#snapshot".to_string()));
}

#[test]
fn volumes_are_btrfs_top_levels_at_a_label() {
    let mi = "\
29 1 0:26 / / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw
40 29 0:40 / /vol1 rw,relatime shared:20 - btrfs /dev/md2 rw,space_cache=v2
41 29 0:40 /photos /srv/photos rw,relatime shared:21 - btrfs /dev/md2 rw
42 29 0:41 / /data rw,relatime shared:22 - ext4 /dev/sdc1 rw
43 40 0:40 /.snapshots/photos /vol1/photos/#snapshot ro,relatime shared:23 - btrfs /dev/md2 rw
";
    assert_eq!(
        rynasd::storage_ops::volumes_in(mi),
        [("vol1".to_string(), "/vol1".to_string())]
    );
}
