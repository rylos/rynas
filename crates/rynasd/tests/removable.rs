//! Removable drives through the internal API, on a fake sysfs/udev/procfs tree whose mount
//! table and USB devices change with the commands rynasd runs (a simulated host): state,
//! mount and unmount tasks, safe removal with its notification, unplugging while mounted,
//! auto-mount of an arriving drive, refusals, settings, the SMB include, the event stream.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::removable::{
    RemovableMount, RemovableSettings, RemovableSettingsState, RemovableState,
};
use rynas_api_types::{ApiError, TaskInfo, TaskStarted, TaskStatus, error_codes};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Applier, Config, ConfigOptions, MemorySystem};
use rynas_notify::MemoryNotifier;
use rynas_removable::Removable;
use rynas_removable::fixture::{Fixture, Fs};
use rynas_removable::host::{Host, Output};
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::ops::Ops;
use rynasd::removable::{RemovableDomain, RemovableSvc};
use rynasd::server::{DaemonState, router};
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

/// A machine whose mount table and USB ports follow the commands.
struct SimHost {
    root: std::path::PathBuf,
    fx: Arc<Mutex<Fixture>>,
    runs: Mutex<Vec<String>>,
    files: Mutex<Vec<(String, String)>>,
}

impl Host for SimHost {
    fn root(&self) -> &Path {
        &self.root
    }

    fn run(&self, argv: &[String], _stdin: Option<&[u8]>, _t: Duration) -> Result<Output, String> {
        self.runs.lock().unwrap().push(argv.join(" "));
        let ok = Output {
            code: Some(0),
            ..Default::default()
        };
        let mut fx = self.fx.lock().unwrap();
        match argv.first().map(String::as_str) {
            Some("systemd-mount") if argv.get(1).map(String::as_str) == Some("--umount") => {
                fx.unmount_path(&argv[2]);
            }
            Some("systemd-mount") => {
                let n = argv.len();
                let (dev, dir) = (&argv[n - 2], &argv[n - 1]);
                let fs = &argv[argv.iter().position(|a| a == "-t").unwrap() + 1];
                let opts = &argv[argv.iter().position(|a| a == "-o").unwrap() + 1];
                let mm = fx
                    .majmin(dev.trim_start_matches("/dev/"))
                    .ok_or("no such device")?;
                fx.mount(&mm, dir, fs, opts.starts_with("ro,"));
            }
            _ => {}
        }
        Ok(ok)
    }

    fn write_sysfs(&self, path: &str, value: &str) -> Result<(), String> {
        if path.ends_with("/remove") && value == "1" {
            self.fx
                .lock()
                .unwrap()
                .unplug_below(path.trim_end_matches("/remove"));
        }
        Ok(())
    }

    fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        self.files
            .lock()
            .unwrap()
            .push((path.into(), content.into()));
        Ok(())
    }

    fn make_dir(&self, _path: &str) -> Result<(), String> {
        Ok(())
    }

    fn remove_empty_dir(&self, _path: &str) {}

    fn flush_device(&self, _dev: &str) -> Result<(), String> {
        Ok(())
    }

    fn scsi_stop(&self, _dev: &str, _eject: bool) -> Result<(), String> {
        Ok(())
    }

    fn cdrom_eject(&self, _dev: &str) -> Result<(), String> {
        Ok(())
    }

    fn systemd(&self) -> bool {
        true
    }

    fn has_program(&self, _name: &str) -> bool {
        true
    }
}

struct Env {
    _dir: tempfile::TempDir,
    app: axum::Router,
    fx: Arc<Mutex<Fixture>>,
    host: Arc<SimHost>,
    svc: Arc<RemovableSvc>,
    ops: Arc<Ops>,
    config: Arc<Config>,
    notifier: Arc<MemoryNotifier>,
}

async fn env(auto_mount: bool, smb: bool) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let hw = dir.path().join("hw");
    let fx = Fixture::demo(&hw);
    fx.put("/proc/uptime", "100000.0 1.0\n");
    fx.put("/etc/group", "rynas-admin:x:997:\n");
    let fx = Arc::new(Mutex::new(fx));
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let (domain, settings) = RemovableDomain::new();
    let tx = domain.tx.clone();
    let domains: Vec<Arc<dyn Applier>> = vec![Arc::new(domain)];
    let config = Arc::new(
        Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            Arc::new(MemorySystem::new("nas")),
            domains,
        )
        .unwrap(),
    );
    tx.send_replace(RemovableSettings {
        auto_mount,
        ..Default::default()
    });
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let ops = Arc::new(Ops::new(config.clone(), tasks));
    let host = Arc::new(SimHost {
        root: hw.clone(),
        fx: fx.clone(),
        runs: Mutex::default(),
        files: Mutex::default(),
    });
    let notifier = Arc::new(MemoryNotifier::default());
    let svc = RemovableSvc::new(
        ops.clone(),
        Arc::new(Removable::new(host.clone(), None)),
        notifier.clone(),
        settings,
        smb,
        Vec::new(),
    );
    let _ = ops.removable.set(svc.clone());
    // the first reading (what the watcher does at start)
    svc.refresh(true).await.unwrap();
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(
        MemoryStorage::default(),
        clock.clone(),
        "rynas",
    ));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    st.removable = Some(svc.clone());
    Env {
        _dir: dir,
        app: router(st),
        fx,
        host,
        svc,
        ops,
        config,
        notifier,
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
    serde_json::from_slice::<ApiError>(b)
        .map(|e| e.code)
        .unwrap_or_else(|_| String::from_utf8_lossy(b).into_owned())
}

async fn wait_task(e: &Env, id: &str) -> TaskInfo {
    for _ in 0..300 {
        if let Some(t) = e.ops.tasks.get(id)
            && t.status.is_finished()
        {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {id} still running");
}

async fn action(e: &Env, uri: &str, body: Option<Value>) -> TaskInfo {
    let (s, b) = call(e, "POST", uri, body).await;
    assert_eq!(
        s,
        StatusCode::ACCEPTED,
        "{uri}: {}",
        String::from_utf8_lossy(&b)
    );
    let t: TaskStarted = serde_json::from_slice(&b).unwrap();
    wait_task(e, &t.task_id).await
}

async fn state(e: &Env) -> RemovableState {
    let (s, b) = call(e, "GET", "/v1/removable", None).await;
    assert_eq!(s, StatusCode::OK);
    serde_json::from_slice(&b).unwrap()
}

fn mount_of(st: &RemovableState, vol: &str) -> Option<String> {
    st.drives
        .iter()
        .flat_map(|d| &d.volumes)
        .find(|v| v.name == vol)
        .and_then(|v| v.mount_point.clone())
}

#[tokio::test]
async fn mount_unmount_and_remove_safely() {
    let e = env(false, false).await;
    let st = state(&e).await;
    assert_eq!(st.drives.len(), 3);
    assert_eq!(st.mount_root, "/media");
    assert_eq!(mount_of(&st, "sdc1").as_deref(), Some("/media/SANDISK"));

    // the file manager's list
    let (_, b) = call(&e, "GET", "/v1/removable/mounts", None).await;
    let m: Vec<RemovableMount> = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        (m.len(), m[0].device.as_str(), m[0].drive.as_str()),
        (1, "sdc1", "sdc")
    );

    let t = action(&e, "/v1/removable/volumes/sdc1/unmount", None).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    assert_eq!(t.kind, "removable.unmount");
    assert_eq!(mount_of(&state(&e).await, "sdc1"), None);

    let t = action(
        &e,
        "/v1/removable/volumes/sdd1/mount",
        Some(json!({"read_only": true})),
    )
    .await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    let st = state(&e).await;
    assert_eq!(mount_of(&st, "sdd1").as_deref(), Some("/media/Backup"));
    let v = st
        .drives
        .iter()
        .flat_map(|d| &d.volumes)
        .find(|v| v.name == "sdd1")
        .unwrap();
    assert!(v.read_only);
    assert!(
        e.host
            .runs
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.contains("-t ntfs3 -o ro,nosuid"))
    );

    // safe removal: unmounted, powered off, gone; notified as safe, not as unplugged
    let t = action(&e, "/v1/removable/drives/sdd/remove", None).await;
    assert_eq!(t.status, TaskStatus::Ok, "{:?}", t.error);
    let st = state(&e).await;
    assert!(!st.drives.iter().any(|d| d.name == "sdd"));
    let ev = e.notifier.take();
    let codes: Vec<&str> = ev.iter().map(|n| n.code.as_str()).collect();
    assert_eq!(codes, ["removable.safe_to_remove"], "{ev:#?}");
    assert_eq!(ev[0].fields["drive"], "WD Elements 25A3");
}

#[tokio::test]
async fn unplugged_while_mounted_is_notified() {
    let e = env(false, false).await;
    e.fx.lock().unwrap().unplug("sdc");
    e.svc.refresh(false).await.unwrap();
    let ev = e.notifier.take();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].code, "removable.unplugged");
    assert_eq!(ev[0].fields["mount_points"], "/media/SANDISK");
    // an unmounted drive pulled out: nothing
    e.fx.lock().unwrap().unplug("mmcblk0");
    e.svc.refresh(false).await.unwrap();
    assert!(e.notifier.take().is_empty());
}

#[tokio::test]
async fn arriving_drive_is_mounted_automatically() {
    let e = env(true, false).await;
    {
        let mut fx = e.fx.lock().unwrap();
        fx.usb_disk(
            "sde",
            64,
            "2-3",
            "Kingston",
            "DataTraveler",
            "K1",
            32_000_000_000,
            480,
            false,
        );
        fx.partition(
            "sde",
            "sde1",
            8,
            65,
            32_000_000_000,
            Fs {
                fs_type: "exfat",
                label: "KINGSTON",
                uuid: "K-1",
            },
        );
    }
    e.svc.refresh(false).await.unwrap();
    for _ in 0..200 {
        if mount_of(&state(&e).await, "sde1").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        mount_of(&state(&e).await, "sde1").as_deref(),
        Some("/media/KINGSTON")
    );
    // drives already there at start (not a boot) were left alone
    assert_eq!(mount_of(&state(&e).await, "mmcblk0p1"), None);
}

#[tokio::test]
async fn refusals() {
    let e = env(false, false).await;
    let (s, b) = call(&e, "POST", "/v1/removable/volumes/sdd2/mount", None).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, error_codes::CONFLICT),
        "locked LUKS"
    );
    let (s, b) = call(&e, "POST", "/v1/removable/volumes/sdc1/mount", None).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::CONFLICT, error_codes::CONFLICT),
        "already mounted"
    );
    let (s, _) = call(&e, "POST", "/v1/removable/volumes/sdz9/unmount", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, b) = call(&e, "POST", "/v1/removable/volumes/..%2Fx/unmount", None).await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::BAD_REQUEST)
    );
    let (s, b) = call(
        &e,
        "POST",
        "/v1/removable/volumes/sdd2/unlock",
        Some(json!({"passphrase": ""})),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_str()),
        (StatusCode::BAD_REQUEST, error_codes::INVALID_VALUE)
    );
    let (s, _) = call(
        &e,
        "POST",
        "/v1/removable/volumes/sdd1/unlock",
        Some(json!({"passphrase": "x"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "not encrypted");
    let (s, b) = call(&e, "GET", "/v1/removable/volumes/sdc1/holders", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(serde_json::from_slice::<Vec<Value>>(&b).unwrap().len(), 0);
}

#[tokio::test]
async fn unlock_then_mount() {
    let e = env(false, false).await;
    let t = action(
        &e,
        "/v1/removable/volumes/sdd2/unlock",
        Some(json!({"passphrase": "correct horse"})),
    )
    .await;
    // the simulated host has no cleartext device: the unlock ran, the mount found it locked
    let runs = e.host.runs.lock().unwrap().clone();
    assert!(
        runs.iter()
            .any(|r| r == "cryptsetup open --type luks --key-file - /dev/sdd2 luks-5e0d1f62-luks"),
        "{runs:?}"
    );
    assert!(!runs.iter().any(|r| r.contains("correct horse")));
    assert_eq!(t.status, TaskStatus::Error);
    let log = e.ops.tasks.read_log(&t.id, 0, 1 << 20).unwrap().text;
    assert!(!log.contains("correct horse"), "{log}");
}

#[tokio::test]
async fn settings_are_staged() {
    let e = env(false, false).await;
    let (s, b) = call(&e, "GET", "/v1/removable/settings", None).await;
    assert_eq!(s, StatusCode::OK);
    let st: RemovableSettingsState = serde_json::from_slice(&b).unwrap();
    assert!(st.applied.auto_mount && st.pending.is_none() && !st.smb_available);
    let mut want = st.applied.clone();
    want.share.enabled = true;
    want.auto_mount = false;
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/removable/settings",
        Some(serde_json::to_value(&want).unwrap()),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let st: RemovableSettingsState = serde_json::from_slice(&b).unwrap();
    assert_eq!(st.pending.as_ref(), Some(&want));
    want.share.group = "not a group!".into();
    let (s, b) = call(
        &e,
        "PUT",
        "/v1/removable/settings",
        Some(serde_json::to_value(&want).unwrap()),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_slice::<ApiError>(&b)
            .unwrap()
            .field
            .as_deref(),
        Some("share.group")
    );
    // apply: the watcher gets the new value
    e.config
        .apply("admin", "admin", "test", &|_| {}, &|| false)
        .unwrap();
    assert!(!e.svc.settings().auto_mount && e.svc.settings().share.enabled);
}

#[tokio::test]
async fn smb_include_follows_the_mounts() {
    let e = env(false, true).await;
    // sharing off: the include exists, without shares
    let files = e.host.files.lock().unwrap().clone();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, "/etc/samba/rynas-removable.conf");
    assert!(!files[0].1.contains('['));
    let (s, _) = call(&e, "GET", "/v1/removable/settings", None).await;
    assert_eq!(s, StatusCode::OK);
    let mut v = RemovableSettings::default();
    v.share.enabled = true;
    let (s, _) = call(
        &e,
        "PUT",
        "/v1/removable/settings",
        Some(serde_json::to_value(&v).unwrap()),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    e.config
        .apply("admin", "admin", "test", &|_| {}, &|| false)
        .unwrap();
    e.svc.refresh(true).await.unwrap();
    let last = e.host.files.lock().unwrap().last().unwrap().1.clone();
    assert!(
        last.contains("[usb-SANDISK]\n   path = /media/SANDISK\n"),
        "{last}"
    );
    assert!(last.contains("valid users = @rynas-admin"));
    assert!(
        e.host
            .runs
            .lock()
            .unwrap()
            .iter()
            .any(|r| r == "smbcontrol smbd reload-config")
    );
    let st = state(&e).await;
    let sh = st
        .drives
        .iter()
        .flat_map(|d| &d.volumes)
        .find(|v| v.name == "sdc1")
        .unwrap()
        .share
        .clone();
    assert_eq!(sh.as_deref(), Some("usb-SANDISK"));
    // unmounting drops the share and disconnects its clients first
    let t = action(&e, "/v1/removable/volumes/sdc1/unmount", None).await;
    assert_eq!(t.status, TaskStatus::Ok);
    let runs = e.host.runs.lock().unwrap().clone();
    let close = runs
        .iter()
        .position(|r| r == "smbcontrol smbd close-share usb-SANDISK")
        .unwrap();
    let umount = runs
        .iter()
        .position(|r| r == "systemd-mount --umount /media/SANDISK")
        .unwrap();
    assert!(close < umount, "{runs:?}");
    assert!(
        !e.host
            .files
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .1
            .contains("[usb-")
    );
}

#[tokio::test]
async fn task_stream_carries_the_state() {
    use http_body_util::BodyExt;
    let e = env(false, false).await;
    let req = Request::builder()
        .uri("/v1/tasks/stream?config=1&removable=1")
        .header("x-rynas-user", "admin")
        .body(Body::empty())
        .unwrap();
    let r = e.app.clone().oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let mut body = r.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
    assert!(text.starts_with("event: removable\ndata: {"), "{text}");
    let data = text.lines().nth(1).unwrap().strip_prefix("data: ").unwrap();
    let st: RemovableState = serde_json::from_str(data).unwrap();
    assert_eq!(st.drives.len(), 3);
    // a change comes as the next event
    e.fx.lock().unwrap().unplug("mmcblk0");
    e.svc.refresh(false).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
    assert!(text.contains("event: removable"));
    assert!(!text.contains("mmcblk0"));
}

#[test]
fn include_paths_agree() {
    assert_eq!(
        rynas_services::smb::REMOVABLE_INCLUDE,
        rynas_removable::smb::INCLUDE
    );
}
