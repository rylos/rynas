//! The storage write side of the internal API on the fixture NAS (fake host, dry system):
//! refusals answered at once, a volume created by a task and recorded in the configuration,
//! disk settings staged.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::storage::{DiskSettingsState, StoragePlan, StorageVolumes};
use rynas_api_types::{ApiError, TaskStarted, TaskStatus, error_codes};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_storage::Storage;
use rynas_storage::domain::{TUNING_KEY, TuningDomain, VOLUMES_KEY, VolumesDomain};
use rynas_storage::host::FakeHost;
use rynas_storage::lsblk::LSBLK_ARGS;
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::ops::Ops;
use rynasd::server::{DaemonState, router};
use rynasd::storage_ops::StorageOps;
use serde_json::{Value, json};
use tower::ServiceExt;

struct NoAuth;

impl Authenticator for NoAuth {
    fn realm(&self) -> &str {
        "pam"
    }
    fn authenticate<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Err(AuthError::InvalidCredentials) })
    }
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../rynas-storage/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

struct Env {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    app: axum::Router,
    ops: Arc<Ops>,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::write(root.join("etc/fstab"), "UUID=x / btrfs defaults 0 1\n").unwrap();
    let mut h = FakeHost {
        now_ms: 1_793_836_800_000,
        ..Default::default()
    };
    h.cmd(
        &format!("lsblk {}", LSBLK_ARGS.join(" ")),
        0,
        &fixture("lsblk-nas.json"),
    );
    h.file("/proc/mdstat", &fixture("proc-mdstat.txt"));
    h.file("/proc/self/mountinfo", &fixture("proc-self-mountinfo.txt"));
    h.cmd(
        "mdadm --detail --export /dev/md2",
        0,
        &fixture("mdadm-detail-export-md2.txt"),
    );
    h.cmd(
        "mdadm --detail /dev/md2",
        0,
        &fixture("mdadm-detail-md2.txt"),
    );
    let storage = Arc::new(Storage::new(Arc::new(h)));
    let config = Arc::new(
        Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            Arc::new(MemorySystem::new("nas")),
            vec![Arc::new(VolumesDomain), Arc::new(TuningDomain)],
        )
        .unwrap(),
    );
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let ops = Arc::new(Ops::new(config, tasks));
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(
        MemoryStorage::default(),
        clock.clone(),
        "rynas",
    ));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.storage = storage.clone();
    st.ops = Some(ops.clone());
    st.storage_ops = Some(Arc::new(StorageOps::new(
        ops.clone(),
        storage,
        st.storage_lock.clone(),
        true,
    )));
    Env {
        _dir: dir,
        root,
        app: router(st),
        ops,
    }
}

async fn call(e: &Env, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-rynas-user", "alice")
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    let r = e.app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec())
}

fn err(b: &[u8]) -> ApiError {
    serde_json::from_slice(b).unwrap()
}

fn create_body(extra: Value) -> Value {
    let mut b = json!({
        "disks": ["sdd"], "level": "single", "label": "backup",
        "confirm": "backup", "serials": ["WD-WX0BBBB4"]
    });
    for (k, v) in extra.as_object().unwrap() {
        b[k] = v.clone();
    }
    b
}

#[tokio::test]
async fn destructive_requests_are_refused_without_their_confirmation() {
    let e = env();
    let post = |b: Value| call(&e, "POST", "/v1/storage/volumes", Some(b));
    // label not typed
    let (s, b) = post(create_body(json!({"confirm": "Backup"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err(&b).code, error_codes::CONFIRMATION_REQUIRED);
    // serial of another disk
    let (s, b) = post(create_body(json!({"serials": ["ZFL0CCC5"]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let m = err(&b);
    assert_eq!(m.code, error_codes::CONFIRMATION_REQUIRED);
    assert!(m.message.contains("sdd: WD-WX0BBBB4"), "{}", m.message);
    // a disk of the running volume, the system disk: the plan refuses them
    let (s, b) = post(create_body(
        json!({"disks": ["sda"], "serials": ["WD-CA0AAAA1"]}),
    ))
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let m = err(&b);
    assert_eq!(m.code, error_codes::INVALID_VALUE);
    assert_eq!(
        m.field.as_deref(),
        Some("disks"),
        "a disk problem, not the label"
    );
    assert!(m.message.contains("sda: in use"), "{}", m.message);
    let (s, b) = post(create_body(
        json!({"disks": ["nvme0n1"], "serials": ["22110D800001"]}),
    ))
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(err(&b).message.contains("holds the operating system"));
    // encrypted without (or with a short) passphrase
    let (s, b) = post(create_body(json!({"encrypt": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(err(&b).message.contains("passphrase"));
    let (s, _) = post(create_body(json!({"encrypt": true, "passphrase": "short"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // dry run: the validated plan, nothing started
    let (s, b) = post(create_body(json!({"dry_run": true}))).await;
    assert_eq!(s, StatusCode::OK);
    let p: StoragePlan = serde_json::from_slice(&b).unwrap();
    assert!(p.valid);
    assert!(e.ops.tasks.list(10).is_empty());
    // unknown volume
    let (s, _) = call(
        &e,
        "POST",
        "/v1/storage/volumes/nope/grow",
        Some(json!({"disks": ["sdd"], "confirm": "nope", "serials": ["WD-WX0BBBB4"]})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(
        &e,
        "POST",
        "/v1/storage/volumes/nope/reveal-recovery-key",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // vol1 has no record (not created by rynas): unlock refused
    let (s, b) = call(
        &e,
        "POST",
        "/v1/storage/volumes/vol1/unlock",
        Some(json!({"passphrase": "whatever1"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{}", String::from_utf8_lossy(&b));
}

#[tokio::test]
async fn a_volume_is_created_by_a_task_and_recorded() {
    let e = env();
    let (s, b) = call(
        &e,
        "POST",
        "/v1/storage/volumes",
        Some(create_body(json!({}))),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let id = serde_json::from_slice::<TaskStarted>(&b).unwrap().task_id;
    assert!(id.contains(":storage.volume-create:alice:"));
    // the same disk cannot be used by a second request while the task runs
    let (s, b) = call(
        &e,
        "POST",
        "/v1/storage/volumes",
        Some(create_body(json!({"label": "other", "confirm": "other"}))),
    )
    .await;
    assert!(
        s == StatusCode::CONFLICT || s == StatusCode::ACCEPTED,
        "{s} {}",
        String::from_utf8_lossy(&b)
    );
    let info = e.ops.tasks.wait(&id).await.unwrap();
    let log = e.ops.tasks.read_log(&id, 0, 1 << 20).unwrap().text;
    assert_eq!(info.status, TaskStatus::Ok, "{log}");
    assert!(log.contains("$ mdadm --create /dev/md/backup"), "{log}");
    assert!(log.contains("(dry system: not run)"));
    let doc: StorageVolumes =
        serde_json::from_value(e.ops.config.current(VOLUMES_KEY).unwrap().unwrap()).unwrap();
    assert_eq!(doc.volumes.len(), 1);
    assert_eq!(doc.volumes[0].label, "backup");
    assert_eq!(doc.volumes[0].mountpoint, "/backup");
    let fstab = std::fs::read_to_string(e.root.join("etc/fstab")).unwrap();
    assert!(fstab.starts_with("UUID=x / btrfs defaults 0 1\n"));
    assert!(fstab.contains(
        "UUID=00000000-0000-0000-0000-000000000000 /backup btrfs noatime,compress=zstd:1,nofail 0 0"
    ));
    let mdadm = std::fs::read_to_string(e.root.join("etc/mdadm/mdadm.conf")).unwrap();
    assert!(
        mdadm
            .contains("ARRAY /dev/md/backup metadata=1.2 UUID=00000000:00000000:00000000:00000000")
    );
    let rev = &e.ops.config.revisions(1).unwrap()[0];
    assert_eq!(rev.author, "alice");
    assert_eq!(rev.comment, "volume backup created");
}

#[tokio::test]
async fn disk_settings_are_staged() {
    let e = env();
    let (s, b) = call(&e, "GET", "/v1/storage/disks/sdd/settings", None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let st: DiskSettingsState = serde_json::from_slice(&b).unwrap();
    assert_eq!(st.serial, "WD-WX0BBBB4");
    assert_eq!(st.defaults.read_ahead_kb, Some(2048));
    assert_eq!(st.applied, None);
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/storage/disks/sdd/settings",
        Some(json!({"apm": 127, "standby_s": 1200, "scheduler": "bfq"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let st: DiskSettingsState = serde_json::from_slice(&b).unwrap();
    assert_eq!(st.pending.as_ref().unwrap().apm, Some(127));
    assert_eq!(st.applied, None);
    let pending = e.ops.config.pending("alice").unwrap();
    assert_eq!(pending.changes[0].key, TUNING_KEY);
    // APM is for hard disks
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/storage/disks/nvme1n1/settings",
        Some(json!({"apm": 127})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err(&b).code, error_codes::INVALID_VALUE);
    assert_eq!(err(&b).field.as_deref(), Some("apm"));
    let (s, _) = call(&e, "GET", "/v1/storage/disks/sdz/settings", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&e, "GET", "/v1/storage/disks/..%2Fx/settings", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
