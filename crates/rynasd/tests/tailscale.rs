//! Tailscale through the internal API, `--dry-system` behaviour: status without Tailscale and
//! with a fake LocalAPI socket, the install as a dry run, login (the auth key never logged),
//! options with validation, the forwarding sysctl and the timed confirmation, admin checks.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::tailscale::{TailscaleState, TailscaleStatus};
use rynas_api_types::{ApiError, TaskInfo, TaskStarted, TaskStatus, error_codes};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::ops::Ops;
use rynasd::roles::AdminPolicy;
use rynasd::server::{DaemonState, router};
use rynasd::tailscale::TailscaleSvc;
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

const STATUS: &str = r#"{"Version":"1.90.6-t1","BackendState":"Running","AuthURL":"",
 "TailscaleIPs":["100.101.102.103","fd7a:115c:a1e0::1234"],
 "Self":{"HostName":"nas","DNSName":"nas.tail1234.ts.net.","Online":true,"KeyExpiry":"2027-05-01T10:00:00Z","AllowedIPs":[],"PrimaryRoutes":[]},
 "CurrentTailnet":{"Name":"user@example.com","MagicDNSEnabled":true},"Peer":{"a":{"Online":true}},
 "ClientVersion":{"RunningLatest":true}}"#;
const PREFS: &str = r#"{"ControlURL":"https://controlplane.tailscale.com","RouteAll":false,"CorpDNS":false,"RunSSH":false,"AdvertiseRoutes":null}"#;

/// A fake tailscaled LocalAPI: status and prefs, one request per connection.
fn fake_tailscaled(path: PathBuf) {
    let l = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        for c in l.incoming() {
            let Ok(mut c) = c else { break };
            let mut req = Vec::new();
            let mut b = [0u8; 1];
            while !req.ends_with(b"\r\n\r\n") {
                if c.read_exact(&mut b).is_err() {
                    break;
                }
                req.push(b[0]);
            }
            let line = String::from_utf8_lossy(&req).to_string();
            let body = if line.starts_with("GET /localapi/v0/prefs ") { PREFS } else { STATUS };
            let _ = write!(c, "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{body}");
        }
    });
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    app: axum::Router,
    ops: Arc<Ops>,
}

/// `installed`: the CLI exists below the root and a fake daemon answers.
fn env(installed: bool) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let host = dir.path().join("host");
    std::fs::create_dir_all(host.join("etc")).unwrap();
    std::fs::write(host.join("etc/os-release"), "ID=debian\nVERSION_CODENAME=trixie\n").unwrap();
    let socket = installed.then(|| {
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::write(root.join("usr/bin/tailscale"), "").unwrap();
        let p = dir.path().join("ts.sock");
        fake_tailscaled(p.clone());
        p
    });
    let config = Arc::new(
        Config::open(ConfigOptions::new(dir.path().join("state"), &root), Arc::new(MemorySystem::new("nas")), vec![]).unwrap(),
    );
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let mut ops = Ops::new(config, tasks);
    ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], |u| match u {
        "admin" => Some(vec!["rynas-admin".into()]),
        "bob" => Some(vec!["users".into()]),
        _ => None,
    });
    let ops = Arc::new(ops);
    let svc = TailscaleSvc::new(ops.clone(), root.clone(), true, socket, dir.path().join("state/tailscale"), host);
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(MemoryStorage::default(), clock.clone(), "rynas"));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    st.tailscale = Some(svc);
    Env { _dir: dir, root, app: router(st), ops }
}

async fn call_full(e: &Env, who: &str, method: &str, uri: &str, body: Option<Value>, local: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-rynas-user", who);
    if let Some(l) = local {
        req = req.header("x-rynas-local-addr", l);
    }
    let req = req.body(body.map(|b| Body::from(b.to_string())).unwrap_or_default()).unwrap();
    let r = e.app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 4 << 20).await.unwrap().to_vec())
}

async fn call(e: &Env, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    call_full(e, "admin", method, uri, body, None).await
}

fn err(b: &[u8]) -> ApiError {
    serde_json::from_slice(b).unwrap_or_else(|_| panic!("not an ApiError: {}", String::from_utf8_lossy(b)))
}

async fn run_task(e: &Env, b: &[u8]) -> (TaskInfo, String) {
    let id = serde_json::from_slice::<TaskStarted>(b).unwrap().task_id;
    for _ in 0..500 {
        if let Some(t) = e.ops.tasks.get(&id)
            && t.status.is_finished()
        {
            let log = e.ops.tasks.read_log(&id, 0, usize::MAX).unwrap().text;
            return (t, log);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {id} still running");
}

#[tokio::test]
async fn without_tailscale_install_is_offered_and_dry() {
    let e = env(false);
    let (s, b) = call(&e, "GET", "/v1/tailscale", None).await;
    assert_eq!(s, StatusCode::OK);
    let st: TailscaleStatus = serde_json::from_slice(&b).unwrap();
    assert!(!st.installed && !st.running && st.install_block.is_none());
    assert_eq!(st.state, TailscaleState::Unavailable);
    // the other actions need Tailscale
    for (m, u) in [("POST", "/v1/tailscale/login"), ("POST", "/v1/tailscale/logout"), ("POST", "/v1/tailscale/uninstall")] {
        let (s, b) = call(&e, m, u, None).await;
        assert_eq!(s, StatusCode::CONFLICT, "{u}");
        assert_eq!(err(&b).code, error_codes::TAILSCALE_NOT_INSTALLED);
    }
    let (s, b) = call(&e, "POST", "/v1/tailscale/install", None).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (t, log) = run_task(&e, &b).await;
    assert_eq!(t.status, TaskStatus::Ok, "{log}");
    assert_eq!(t.kind, "tailscale.install");
    assert!(log.contains("trixie.noarmor.gpg"), "{log}");
    assert!(log.contains("install -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold tailscale (dry system: not run)"), "{log}");
    let src = std::fs::read_to_string(e.root.join("etc/apt/sources.list.d/tailscale.sources")).unwrap();
    assert!(src.contains("Signed-By: /usr/share/keyrings/tailscale-archive-keyring.gpg"));
    // administrators only
    let e2 = env(false);
    let (s, _) = call_full(&e2, "bob", "POST", "/v1/tailscale/install", None, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn status_options_and_confirmation() {
    let e = env(true);
    let (s, b) = call_full(&e, "admin", "GET", "/v1/tailscale", None, Some("100.101.102.103:443")).await;
    assert_eq!(s, StatusCode::OK);
    let st: TailscaleStatus = serde_json::from_slice(&b).unwrap();
    assert!(st.installed && st.running && st.online && st.via_tailscale);
    assert_eq!(st.state, TailscaleState::Running);
    assert_eq!(st.dns_name.as_deref(), Some("nas.tail1234.ts.net"));
    assert_eq!(st.version.as_deref(), Some("1.90.6"));
    assert!(st.confirm.is_none());
    let (_, b) = call_full(&e, "admin", "GET", "/v1/tailscale", None, Some("192.168.1.10:443")).await;
    assert!(!serde_json::from_slice::<TailscaleStatus>(&b).unwrap().via_tailscale);
    // the install is refused once installed
    let (s, _) = call(&e, "POST", "/v1/tailscale/install", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // a subnet with host bits set
    let (s, b) = call(&e, "PUT", "/v1/tailscale/options", Some(json!({"advertise_routes": ["192.168.1.5/24"]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err(&b).field.as_deref(), Some("advertise_routes"));
    let (s, _) = call(&e, "POST", "/v1/tailscale/options/confirm", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, b) = call(&e, "PUT", "/v1/tailscale/options", Some(json!({"advertise_routes": ["192.168.1.0/24"], "exit_node": true}))).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (t, log) = run_task(&e, &b).await;
    assert_eq!(t.status, TaskStatus::Ok, "{log}");
    assert!(log.contains("tailscale set --accept-routes=false --accept-dns=false --advertise-exit-node=true --advertise-routes=192.168.1.0/24 --ssh=false"), "{log}");
    let sysctl = std::fs::read_to_string(e.root.join("etc/sysctl.d/60-rynas-tailscale.conf")).unwrap();
    assert!(sysctl.contains("net.ipv4.ip_forward = 1"));
    let (_, b) = call(&e, "GET", "/v1/tailscale", None).await;
    assert!(serde_json::from_slice::<TailscaleStatus>(&b).unwrap().confirm.is_some());
    let (s, _) = call(&e, "POST", "/v1/tailscale/options/confirm", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&e, "POST", "/v1/tailscale/options/confirm", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // neither router nor exit node: the sysctl file goes
    let (_, b) = call(&e, "PUT", "/v1/tailscale/options", Some(json!({"ssh": true}))).await;
    let (t, _) = run_task(&e, &b).await;
    assert_eq!(t.status, TaskStatus::Ok);
    assert!(!e.root.join("etc/sysctl.d/60-rynas-tailscale.conf").exists());
    // reads need an administrator too
    let (s, _) = call_full(&e, "bob", "GET", "/v1/tailscale", None, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn login_never_logs_the_key() {
    let e = env(true);
    let (s, b) = call(&e, "POST", "/v1/tailscale/login", Some(json!({"hostname": "bad.name"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err(&b).field.as_deref(), Some("hostname"));
    let (s, b) = call(&e, "POST", "/v1/tailscale/login", Some(json!({"auth_key": "tskey-auth-SECRET123", "hostname": "nas"}))).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (t, log) = run_task(&e, &b).await;
    assert_eq!(t.status, TaskStatus::Ok, "{log}");
    assert!(!log.contains("SECRET123"), "{log}");
    assert!(log.contains("tailscale up --reset") && log.contains("--auth-key=file:"), "{log}");
    let (s, b) = call(&e, "POST", "/v1/tailscale/logout", None).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (t, log) = run_task(&e, &b).await;
    assert_eq!(t.status, TaskStatus::Ok);
    assert!(log.contains("tailscale logout"));
    let (s, b) = call(&e, "POST", "/v1/tailscale/uninstall", Some(json!({"purge_state": true}))).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (t, log) = run_task(&e, &b).await;
    assert_eq!(t.status, TaskStatus::Ok);
    assert!(log.contains("purge -y tailscale") && log.contains("rm -rf --one-file-system /var/lib/tailscale"), "{log}");
}
