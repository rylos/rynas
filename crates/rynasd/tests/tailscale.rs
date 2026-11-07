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
    svc: Arc<TailscaleSvc>,
    state: PathBuf,
}

/// `installed`: the CLI exists below the root and a fake daemon answers.
fn env(installed: bool) -> Env {
    env_with(installed, |_| {})
}

/// [`env`], with `seed` run on the Tailscale state folder before the service starts.
fn env_with(installed: bool, seed: impl FnOnce(&std::path::Path)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state/tailscale");
    std::fs::create_dir_all(&state).unwrap();
    seed(&state);
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
    let svc = TailscaleSvc::new(ops.clone(), root.clone(), true, socket, state.clone(), host);
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(MemoryStorage::default(), clock.clone(), "rynas"));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    st.tailscale = Some(svc.clone());
    Env { _dir: dir, root, app: router(st), ops, svc, state }
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
    // /usr is read-only inside rynasd (ProtectSystem=yes): the download lands in the state
    // folder and the keyring is placed outside the sandbox, never written by rynasd itself
    assert!(log.contains(&format!("-o {}/archive-keyring.gpg.new", e.state.display())), "{log}");
    assert!(log.contains("-- install -D -m 0644 ") && log.contains(" /usr/share/keyrings/tailscale-archive-keyring.gpg"), "{log}");
    assert!(!e.root.join("usr").exists());
    // nothing but the source in sources.list.d (the temporary file is staged in /etc/apt)
    let names: Vec<_> = std::fs::read_dir(e.root.join("etc/apt/sources.list.d")).unwrap().map(|d| d.unwrap().file_name()).collect();
    assert_eq!(names, ["tailscale.sources"]);
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
    // through rynas-caddy: loopback local address, the client from X-Forwarded-For
    for (client, via) in [("100.100.1.2", true), ("fd7a:115c:a1e0::9", true), ("192.168.1.10", false)] {
        let req = Request::builder()
            .uri("/v1/tailscale")
            .header("x-rynas-user", "admin")
            .header("x-rynas-local-addr", "127.0.0.1:8080")
            .header("x-rynas-client-addr", client)
            .body(Body::empty())
            .unwrap();
        let r = e.app.clone().oneshot(req).await.unwrap();
        let b = to_bytes(r.into_body(), 1 << 20).await.unwrap();
        assert_eq!(serde_json::from_slice::<TailscaleStatus>(&b).unwrap().via_tailscale, via, "{client}");
    }
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
    assert!(e.state.join("confirm.json").exists());
    // while the change waits, another change, a login or a logout is refused (the previous
    // options of the revert stay those before the first change)
    for (m, u, body) in [
        ("PUT", "/v1/tailscale/options", Some(json!({"ssh": true}))),
        ("POST", "/v1/tailscale/login", None),
        ("POST", "/v1/tailscale/logout", None),
    ] {
        let (s, b) = call(&e, m, u, body).await;
        assert_eq!(s, StatusCode::CONFLICT, "{u}");
        assert!(err(&b).message.contains("confirmation"), "{u}");
    }
    let (s, _) = call(&e, "POST", "/v1/tailscale/options/confirm", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(!e.state.join("confirm.json").exists());
    let (s, _) = call(&e, "POST", "/v1/tailscale/options/confirm", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // neither router nor exit node: the sysctl file goes
    let (_, b) = call(&e, "PUT", "/v1/tailscale/options", Some(json!({"ssh": true}))).await;
    let (t, _) = run_task(&e, &b).await;
    assert_eq!(t.status, TaskStatus::Ok);
    assert!(!e.root.join("etc/sysctl.d/60-rynas-tailscale.conf").exists());
    let (s, _) = call(&e, "POST", "/v1/tailscale/options/confirm", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
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
    assert!(log.contains("-- rm -f /usr/share/keyrings/tailscale-archive-keyring.gpg"), "{log}");
}

/// The revert task of an unconfirmed change, once it started (`None` before).
async fn revert_log(e: &Env) -> Option<String> {
    let t = e.ops.tasks.list(50).into_iter().find(|t| t.kind == "tailscale.revert")?;
    let b = serde_json::to_vec(&TaskStarted { task_id: t.id.clone() }).unwrap();
    Some(run_task(e, &b).await.1)
}

async fn wait_revert(e: &Env) -> String {
    for _ in 0..200 {
        if let Some(l) = revert_log(e).await {
            return l;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no revert");
}

/// Unconfirmed: the options before the change come back (with the forwarding file), and
/// nothing is pending any more.
#[tokio::test]
async fn unconfirmed_change_is_reverted() {
    let e = env(true);
    e.svc.set_confirm_window(Duration::from_millis(300));
    let (s, b) = call(&e, "PUT", "/v1/tailscale/options", Some(json!({"exit_node": true}))).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (t, log) = run_task(&e, &b).await;
    assert_eq!(t.status, TaskStatus::Ok, "{log}");
    assert!(log.contains("waiting 0 s"), "{log}");
    assert!(e.root.join("etc/sysctl.d/60-rynas-tailscale.conf").exists());
    let log = wait_revert(&e).await;
    assert!(log.contains("not confirmed in time"), "{log}");
    // the fake daemon's prefs: nothing advertised, ssh off
    assert!(log.contains("tailscale set --accept-routes=false --accept-dns=false --advertise-exit-node=false --advertise-routes= --ssh=false"), "{log}");
    assert!(!e.root.join("etc/sysctl.d/60-rynas-tailscale.conf").exists());
    let (_, b) = call(&e, "GET", "/v1/tailscale", None).await;
    assert!(serde_json::from_slice::<TailscaleStatus>(&b).unwrap().confirm.is_none());
    assert!(!e.state.join("confirm.json").exists());
    let (s, _) = call(&e, "POST", "/v1/tailscale/options/confirm", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
}

/// A revert that finds another Tailscale task running waits for it instead of being dropped.
#[tokio::test]
async fn revert_waits_for_a_running_task() {
    let e = env(true);
    e.svc.set_confirm_window(Duration::from_millis(200));
    let (_, b) = call(&e, "PUT", "/v1/tailscale/options", Some(json!({"ssh": true}))).await;
    let (t, _) = run_task(&e, &b).await;
    assert_eq!(t.status, TaskStatus::Ok);
    e.svc.hold_task_slot(Some("other"));
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(revert_log(&e).await.is_none());
    // still pending, still refusing other changes
    let (s, _) = call(&e, "PUT", "/v1/tailscale/options", Some(json!({"ssh": false}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    e.svc.hold_task_slot(None);
    let log = wait_revert(&e).await;
    assert!(log.contains("--ssh=false"), "{log}");
}

/// A change left waiting by a previous rynasd: reverted at start when its window passed; the
/// auth key file of an interrupted login is removed.
#[tokio::test]
async fn pending_change_survives_a_restart() {
    let e = env_with(true, |state| {
        std::fs::write(
            state.join("confirm.json"),
            json!({"deadline_ms": 1000, "previous": {"accept_routes": false, "accept_dns": true, "exit_node": false, "advertise_routes": [], "ssh": true}}).to_string(),
        )
        .unwrap();
        std::fs::write(state.join("authkey"), "tskey-auth-LEFT").unwrap();
    });
    assert!(!e.state.join("authkey").exists());
    e.svc.resume_pending();
    let log = wait_revert(&e).await;
    assert!(log.contains("--accept-dns=true") && log.contains("--ssh=true"), "{log}");
    assert!(!e.state.join("confirm.json").exists());
    // a window still open goes on after the restart
    let deadline = rynas_tasks::now_ms() + 60_000;
    let e = env_with(true, |state| {
        std::fs::write(
            state.join("confirm.json"),
            json!({"deadline_ms": deadline, "previous": {"accept_routes": false, "accept_dns": false, "exit_node": false, "advertise_routes": [], "ssh": false}}).to_string(),
        )
        .unwrap();
    });
    e.svc.resume_pending();
    let (_, b) = call(&e, "GET", "/v1/tailscale", None).await;
    let st: TailscaleStatus = serde_json::from_slice(&b).unwrap();
    assert_eq!(st.confirm.unwrap().deadline_ms, deadline);
    let (s, _) = call(&e, "POST", "/v1/tailscale/options/confirm", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}
