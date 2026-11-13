//! What the PC backup end-to-end tests on real btrfs share: rynasd wired as main.rs does
//! (on the real host: [`HostReceive`]), an agent folder with its paired PC, the routes served
//! on 127.0.0.1 as rynas-web would.

#![allow(dead_code)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::TaskStarted;
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_notify::MemoryNotifier;
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::ops::Ops;
use rynasd::pc_backup::{HostReceive, PcBackup};
use rynasd::roles::AdminPolicy;
use rynasd::server::{DaemonState, router};
use serde_json::{Value, json};
use tower::ServiceExt;

pub struct NoAuth;

impl Authenticator for NoAuth {
    fn realm(&self) -> &str {
        "none"
    }
    fn authenticate<'a>(&'a self, _: &'a str, _: &'a str) -> BoxFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Err(AuthError::InvalidCredentials) })
    }
}

pub fn groups(u: &str) -> Option<Vec<String>> {
    (u == "admin").then(|| vec!["rynas-admin".into()])
}

/// Runs `cmd`, which must succeed; its stdout.
pub fn sh(cmd: &mut Command) -> String {
    let o = cmd.output().unwrap();
    assert!(o.status.success(), "{cmd:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// A console call as the administrator.
pub async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-rynas-user", "admin")
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec())
}

pub fn info(p: &Path) -> rynas_btrfs::ioctl::SubvolInfo {
    rynas_btrfs::ioctl::subvol_info(&rynas_btrfs::ioctl::open_dir(p).unwrap()).unwrap()
}

/// Waits for the task a call started; it must end without error.
pub async fn wait_task(ops: &Ops, b: &[u8]) {
    let id = serde_json::from_slice::<TaskStarted>(b).unwrap().task_id;
    for _ in 0..500 {
        if ops.tasks.get(&id).is_some_and(|t| t.status.is_finished()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let t = ops.tasks.get(&id).unwrap();
    assert!(t.error.is_none(), "{:?}", t.error);
}

/// rynasd with the PC backup on the real host.
pub struct Nas {
    pub app: axum::Router,
    pub ops: Arc<Ops>,
    _dir: tempfile::TempDir,
}

impl Nas {
    pub fn new() -> Nas {
        let dir = tempfile::tempdir().unwrap();
        let (root, state_dir) = (dir.path().join("root"), dir.path().join("state"));
        std::fs::create_dir_all(&root).unwrap();
        let config = Arc::new(Config::open(ConfigOptions::new(&state_dir, &root), Arc::new(MemorySystem::new("nas")), Vec::new()).unwrap());
        let mut ops = Ops::new(config, TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap());
        ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], groups);
        let ops = Arc::new(ops);
        let notifier = Arc::new(MemoryNotifier::default());
        let clock = Arc::new(ManualClock::new(1_800_000_000_000));
        let totp = Arc::new(TotpManager::new(MemoryStorage::default(), clock.clone(), "rynas"));
        let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
        st.ops = Some(ops.clone());
        let svc = PcBackup::new(ops.clone(), Arc::new(HostReceive), notifier.clone(), state_dir.clone(), AdminPolicy::new(vec!["rynas-admin".into()], groups), "nas".into());
        let agent = rynasd::pc_agent::PcAgent::new(
            ops.clone(),
            notifier,
            state_dir,
            true,
            rynas_backup::Launch::Direct { exe: "/nonexistent".into() },
            Arc::new(tokio::sync::Semaphore::new(1)),
            None,
            "nas".into(),
        );
        agent.bind_btrfs(svc.clone());
        st.pc_backup = Some(svc);
        st.pc_agent = Some(agent);
        Nas { app: router(st), ops, _dir: dir }
    }

    /// An agent folder for `device` at `folder` (no key, no account).
    pub async fn folder(&self, device: &str, folder: &Path) {
        let (s, b) = call(&self.app, "POST", "/v1/pc-backups/targets", Some(json!({"device": device, "path": folder, "agent": true}))).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
        wait_task(&self.ops, &b).await;
    }

    /// The PC paired with the btrfs method on the folder of `device`: its token.
    pub async fn pair(&self, device: &str) -> String {
        let (s, b) = call(&self.app, "POST", "/v1/pcbackup/pairings", Some(json!({"btrfs_target": device}))).await;
        assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
        let code = serde_json::from_slice::<Value>(&b).unwrap()["code"].as_str().unwrap().to_string();
        let (s, b) = call(&self.app, "POST", "/v1/pcbackup/agent/pair", Some(json!({"code": code, "hostname": device}))).await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        serde_json::from_slice::<Value>(&b).unwrap()["token"].as_str().unwrap().to_string()
    }

    /// A restore code for `device`.
    pub async fn restore_code(&self, device: &str) -> String {
        let (s, b) = call(&self.app, "POST", &format!("/v1/pcbackup/devices/{device}/restore-code"), None).await;
        assert_eq!(s, StatusCode::CREATED, "{}", String::from_utf8_lossy(&b));
        serde_json::from_slice::<Value>(&b).unwrap()["code"].as_str().unwrap().to_string()
    }

    /// The routes served on 127.0.0.1 below `/api`, as rynas-web would: the NAS' address.
    pub async fn serve(&self) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = axum::Router::new().nest("/api", self.app.clone());
        tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
        format!("http://{addr}")
    }
}

/// `rynas-agent btrfs-send` of `subvols` (name, path) with the snapshot folder `snap_dir`:
/// its output (it must succeed).
pub fn agent_send(agent_bin: &str, nas: &str, token: &str, round: &str, snap_dir: &Path, subvols: &[(&str, &Path)]) -> String {
    let mut c = Command::new(agent_bin);
    c.args(["btrfs-send", "--nas", nas, "--allow-http", "--token", token, "--snapshot-dir"]).arg(snap_dir);
    for (n, p) in subvols {
        c.arg("--subvol").arg(format!("{n}={}", p.display()));
    }
    let out = c.output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    println!("{round}: {text}");
    assert!(out.status.success(), "{round}: {text}");
    text
}

/// `rynas-agent btrfs-restore-pool` into the btrfs top level `pool`: its output (it must
/// succeed).
pub fn agent_restore_pool(agent_bin: &str, nas: &str, code: &str, pool: &Path, extra: &[&str]) -> String {
    let out = Command::new(agent_bin).args(["btrfs-restore-pool", "--nas", nas, "--allow-http", "--code", code, "--pool"]).arg(pool).args(extra).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    println!("restore: {text}");
    assert!(out.status.success(), "restore: {text}");
    text
}

/// A transient unit whose name starts with `prefix` is active.
pub fn unit_active(prefix: &str) -> bool {
    let o = Command::new("systemctl").args(["list-units", "--no-legend", "--plain", "--state=active", &format!("{prefix}*")]).output().unwrap();
    !String::from_utf8_lossy(&o.stdout).trim().is_empty()
}
