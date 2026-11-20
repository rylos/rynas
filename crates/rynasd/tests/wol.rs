//! Wake-on-LAN through rynasd's router: devices written straight into `wol` (with a
//! revision), what each user sees, a wake as a task against a stand-in network (no packet
//! leaves, nothing is pinged), the agent's facts and the power commands an agent gets only
//! from 0.8.26.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rynas_api_types::pcagent::{AgentCommand, AgentPlan, SleepMode};
use rynas_api_types::wol::{WolConfig, WolDevice, WolOverview, WolStatus, WolWakeEnd, WolWakeStarted};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Config, ConfigOptions, MemorySystem};
use rynas_notify::MemoryNotifier;
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::ops::Ops;
use rynasd::pc_agent::{Dev, PcAgent, sha_hex};
use rynasd::roles::AdminPolicy;
use rynasd::server::{DaemonState, router};
use rynasd::wol::probe::Neighbor;
use rynasd::wol::send::NasIface;
use rynasd::wol::{Net, Wol, WolDomain};
use serde_json::{Value, json};
use tower::ServiceExt;

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
        "root" | "admin" => Some(vec!["rynas-admin".to_string()]),
        "bob" | "carol" => Some(vec!["users".to_string()]),
        _ => None,
    }
}

fn user_exists(u: &str) -> bool {
    groups(u).is_some()
}

/// The stand-in network: one interface, the device answers ping once a packet was sent.
#[derive(Default)]
struct FakeNet {
    sent: AtomicUsize,
    awake: AtomicBool,
    ifaces_sent: Mutex<Vec<String>>,
    /// More neighbor entries (an old address another host took).
    extra: Mutex<Vec<Neighbor>>,
}

impl Net for FakeNet {
    fn ifaces(&self) -> Vec<NasIface> {
        vec![NasIface {
            name: "eth0".into(),
            index: 2,
            mac: Some("02:00:00:00:00:01".into()),
            up: true,
            broadcast: true,
            ipv4: vec![(Ipv4Addr::new(192, 0, 2, 5), 24)],
            ..Default::default()
        }]
    }
    fn neighbors(&self) -> Vec<Neighbor> {
        let mut v = vec![Neighbor { ip: Ipv4Addr::new(192, 0, 2, 77), dev: "eth0".into(), mac: "aa:bb:cc:dd:ee:77".into(), reachable: true }];
        v.extend(self.extra.lock().unwrap().iter().cloned());
        v
    }
    fn ping(&self, _ip: Ipv4Addr) -> bool {
        self.awake.load(Ordering::SeqCst)
    }
    fn port(&self, _ip: Ipv4Addr, _ports: &[u16]) -> bool {
        false
    }
    fn send(&self, iface: &NasIface, payload: &[u8], _port: u16, _ip: Option<Ipv4Addr>) -> Result<(), String> {
        assert_eq!(payload.len(), 102);
        self.sent.fetch_add(1, Ordering::SeqCst);
        self.ifaces_sent.lock().unwrap().push(iface.name.clone());
        self.awake.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn resolve(&self, host: &str) -> Option<Ipv4Addr> {
        host.parse().ok()
    }
    fn reverse(&self, _ip: Ipv4Addr) -> Option<String> {
        Some("tv.lan".into())
    }
}

async fn call(app: &axum::Router, who: &str, method: &str, uri: &str, body: Option<Value>, token: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri).header("x-rynas-user", who);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    if body.is_some() {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    let body = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
    let r = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec())
}

struct World {
    app: axum::Router,
    ops: Arc<Ops>,
    agent: Arc<PcAgent>,
    net: Arc<FakeNet>,
    wol: Arc<Wol>,
    _dir: tempfile::TempDir,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let (root, state_dir) = (dir.path().join("root"), dir.path().join("state"));
    std::fs::create_dir_all(&root).unwrap();
    let (domain, rx) = WolDomain::new();
    let config = Arc::new(Config::open(ConfigOptions::new(&state_dir, &root), Arc::new(MemorySystem::new("nas")), vec![Arc::new(domain)]).unwrap());
    let mut ops = Ops::new(config, TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap());
    ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], groups);
    let ops = Arc::new(ops);
    let notifier = Arc::new(MemoryNotifier::default());
    let agent = PcAgent::new(
        ops.clone(),
        notifier.clone(),
        state_dir.clone(),
        true,
        rynas_backup::Launch::Direct { exe: "/nonexistent/rynas-backup-worker".into() },
        Arc::new(tokio::sync::Semaphore::new(1)),
        None,
        "nas".into(),
    );
    // a paired PC with an agent of 0.8.25 (its token: "tok")
    agent
        .edit(|r| {
            r.devices.insert(
                "office".into(),
                Dev { hostname: "office".into(), agent_version: "0.8.25".into(), token_sha: sha_hex(b"tok"), group: "office".into(), ..Default::default() },
            );
            Ok(())
        })
        .unwrap();
    let net = Arc::new(FakeNet::default());
    let wol = Wol::new(ops.clone(), notifier, state_dir, rx, Some(agent.clone()), net.clone(), user_exists);
    let clock = Arc::new(ManualClock::new(1_800_000_000_000));
    let totp = Arc::new(TotpManager::new(MemoryStorage::default(), clock.clone(), "rynas"));
    let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
    st.ops = Some(ops.clone());
    st.pc_agent = Some(agent.clone());
    st.wol = Some(wol.clone());
    World { app: router(st), ops, agent, net, wol, _dir: dir }
}

#[tokio::test]
async fn devices_visibility_and_validation() {
    let w = world();
    let pc = json!({"name": "Office PC", "macs": ["AA-BB-CC-DD-EE-F0"], "pc_device": "office", "users": ["bob"]});
    // administrators only
    let (s, _) = call(&w.app, "bob", "POST", "/v1/wol/devices", Some(pc.clone()), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = call(&w.app, "admin", "POST", "/v1/wol/devices", Some(pc.clone()), None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let d: WolDevice = serde_json::from_slice(&b).unwrap();
    assert_eq!((d.id.as_str(), d.macs[0].as_str()), ("office-pc", "aa:bb:cc:dd:ee:f0"));
    // written at once, with a revision
    let applied: WolConfig = serde_json::from_value(w.ops.config.applied("wol").unwrap().unwrap()).unwrap();
    assert_eq!(applied.devices.len(), 1);
    assert!(!w.ops.config.revisions(10).unwrap().is_empty());
    // refusals: the same MAC, the NAS's own MAC, an unknown user or PC, a bad timeout
    for (body, field) in [
        (json!({"name": "dup", "macs": ["aa:bb:cc:dd:ee:f0"]}), "macs"),
        (json!({"name": "nas", "macs": ["02:00:00:00:00:01"]}), "macs"),
        (json!({"name": "x", "macs": ["aa:bb:cc:dd:ee:01"], "users": ["ghost"]}), "users"),
        (json!({"name": "x", "macs": ["aa:bb:cc:dd:ee:01"], "pc_device": "nope"}), "pc_device"),
        (json!({"name": "x", "macs": ["aa:bb:cc:dd:ee:01"], "wake_timeout_s": 10}), "wake_timeout_s"),
    ] {
        let (s, b) = call(&w.app, "admin", "POST", "/v1/wol/devices", Some(body), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&b));
        assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["field"], field);
    }
    let (s, b) = call(&w.app, "admin", "POST", "/v1/wol/devices", Some(json!({"name": "TV", "macs": ["aa:bb:cc:dd:ee:77"]})), None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    // what each one sees
    let view = |who: &'static str| {
        let app = w.app.clone();
        async move {
            let (s, b) = call(&app, who, "GET", "/v1/wol", None, None).await;
            assert_eq!(s, StatusCode::OK);
            serde_json::from_slice::<WolOverview>(&b).unwrap()
        }
    };
    let a = view("admin").await;
    assert_eq!(a.devices.len(), 2);
    assert!(a.devices.iter().all(|d| d.can_manage));
    assert_eq!(a.ifaces.len(), 1);
    let b = view("bob").await;
    assert_eq!(b.devices.len(), 1);
    assert_eq!(b.devices[0].device.id, "office-pc");
    assert!(!b.devices[0].can_manage && b.ifaces.is_empty());
    assert!(view("carol").await.devices.is_empty());
    // carol cannot wake bob's device (as if it did not exist), nor discover
    let (s, _) = call(&w.app, "carol", "POST", "/v1/wol/devices/office-pc/wake", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&w.app, "bob", "GET", "/v1/wol/discover", None, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // discovery: the neighbor (known as "tv"), not the NAS
    let (s, b) = call(&w.app, "admin", "GET", "/v1/wol/discover", None, None).await;
    assert_eq!(s, StatusCode::OK);
    let c: Vec<rynas_api_types::wol::WolCandidate> = serde_json::from_slice(&b).unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!((c[0].known.as_deref(), c[0].name.as_deref()), (Some("tv"), Some("tv.lan")));
    // change and remove
    let (s, b) = call(&w.app, "admin", "PUT", "/v1/wol/devices/tv", Some(json!({"name": "Living room TV", "macs": ["aa:bb:cc:dd:ee:77"], "ports": [8009]})), None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(serde_json::from_slice::<WolDevice>(&b).unwrap().id, "tv");
    let (s, _) = call(&w.app, "admin", "DELETE", "/v1/wol/devices/tv", None, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&w.app, "admin", "DELETE", "/v1/wol/devices/tv", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn facts_and_power_commands() {
    let w = world();
    let (s, b) = call(&w.app, "admin", "POST", "/v1/wol/devices", Some(json!({"name": "Office", "macs": ["aa:bb:cc:dd:ee:f0"], "pc_device": "office", "users": ["bob"]})), None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    // an agent of 0.8.25 gets no power command
    let (s, b) = call(&w.app, "bob", "POST", "/v1/wol/devices/office/sleep", Some(json!({"mode": "suspend"})), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{}", String::from_utf8_lossy(&b));
    // the agent updates and reports its facts (its token, no session user needed)
    let facts = json!({"agent_version": "0.8.26", "root_btrfs": true, "sleep": ["suspend", "poweroff"],
        "nics": [{"name": "enp3s0", "mac": "AA:BB:CC:DD:EE:F0", "ipv4": ["192.0.2.30/24"], "up": true, "wol_supported": true, "wol_enabled": false},
                 {"name": "wlan0", "mac": "aa:bb:cc:dd:ee:f9", "wireless": true, "up": true}]});
    let (s, _) = call(&w.app, "", "POST", "/v1/pcbackup/agent/facts", Some(facts.clone()), Some("wrong")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, b) = call(&w.app, "", "POST", "/v1/pcbackup/agent/facts", Some(facts), Some("tok")).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{}", String::from_utf8_lossy(&b));
    let d = w.agent.device("office").unwrap();
    assert_eq!(d.agent_version, "0.8.26");
    assert_eq!(d.root_btrfs, Some(true));
    let f = d.facts.unwrap();
    assert_eq!(f.nics[0].mac, "aa:bb:cc:dd:ee:f0");
    // stored with the devices: read back after a restart of the service
    let reread: serde_json::Value = serde_json::from_slice(&std::fs::read(w._dir.path().join("state/pc-agent/registry.json")).unwrap()).unwrap();
    assert_eq!(reread["devices"]["office"]["facts"]["nics"][1]["name"], "wlan0");
    // the overview: the agent, its address, the warning
    let (_, b) = call(&w.app, "bob", "GET", "/v1/wol", None, None).await;
    let o: WolOverview = serde_json::from_slice(&b).unwrap();
    let v = &o.devices[0];
    assert_eq!(v.ip.as_deref(), Some("192.0.2.30"));
    assert_eq!(v.send_iface.as_deref(), Some("eth0"));
    assert_eq!(v.agent.as_ref().unwrap().sleep, [SleepMode::Suspend, SleepMode::Poweroff]);
    // (aa:… is a locally administered MAC)
    assert_eq!(v.warnings, [rynas_api_types::wol::WolWarning::Disabled, rynas_api_types::wol::WolWarning::RandomMac]);
    // its agent's requests (the facts just now) count as on, also when ping does not answer
    let (wol, devices) = (w.wol.clone(), w.wol.devices());
    tokio::task::spawn_blocking(move || wol.round(&devices)).await.unwrap();
    let (_, b) = call(&w.app, "bob", "GET", "/v1/wol", None, None).await;
    let v = serde_json::from_slice::<WolOverview>(&b).unwrap().devices.remove(0);
    assert_eq!((v.status, v.probe), (WolStatus::Online, Some(rynas_api_types::wol::WolProbe::Agent)));
    assert!(v.agent.as_ref().unwrap().power_commands);
    // a mode the PC cannot do; then suspend
    let (s, _) = call(&w.app, "bob", "POST", "/v1/wol/devices/office/sleep", Some(json!({"mode": "hibernate"})), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, b) = call(&w.app, "bob", "POST", "/v1/wol/devices/office/sleep", Some(json!({"mode": "suspend"})), None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&b));
    let (_, b) = call(&w.app, "", "GET", "/v1/pcbackup/agent/plan", None, Some("tok")).await;
    let plan: AgentPlan = serde_json::from_slice(&b).unwrap();
    assert!(matches!(plan.command, Some(AgentCommand::Sleep { mode: SleepMode::Suspend, .. })));
    // enabling is for administrators; it names the wired card that supports it
    let (s, _) = call(&w.app, "bob", "POST", "/v1/wol/devices/office/enable", None, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(&w.app, "admin", "POST", "/v1/wol/devices/office/enable", None, None).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let d = w.agent.device("office").unwrap();
    assert!(matches!(&d.command, Some(AgentCommand::EnableWol { nics, .. }) if nics == &["enp3s0".to_string()]));
}

#[tokio::test]
async fn a_wake_is_a_task_with_lines_and_history() {
    let w = world();
    let (s, _) = call(&w.app, "admin", "POST", "/v1/wol/devices", Some(json!({"name": "TV", "macs": ["aa:bb:cc:dd:ee:77"], "users": ["bob"]})), None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = call(&w.app, "bob", "POST", "/v1/wol/devices/tv/wake", None, None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let started: WolWakeStarted = serde_json::from_slice(&b).unwrap();
    assert!(!started.joined);
    // a second click joins it
    let (_, b) = call(&w.app, "admin", "POST", "/v1/wol/devices/tv/wake", None, None).await;
    let again: WolWakeStarted = serde_json::from_slice(&b).unwrap();
    assert!(again.joined && again.upid == started.upid);
    let (_, b) = call(&w.app, "bob", "GET", "/v1/wol", None, None).await;
    let v = serde_json::from_slice::<WolOverview>(&b).unwrap().devices.remove(0);
    assert_eq!(v.status, WolStatus::Waking);
    assert!(v.waking_since_ms.is_some());
    let info = w.ops.tasks.wait(&started.upid).await.unwrap();
    assert!(info.error.is_none(), "{:?}", info.error);
    let log = w.ops.tasks.read_log(&started.upid, 0, 1 << 16).unwrap().text;
    assert!(log.contains("Sent on eth0 (3×)"), "{log}");
    assert!(log.contains("Online after"), "{log}");
    assert_eq!(w.net.sent.load(Ordering::SeqCst), 3);
    assert!(w.net.ifaces_sent.lock().unwrap().iter().all(|i| i == "eth0"));
    let (_, b) = call(&w.app, "bob", "GET", "/v1/wol", None, None).await;
    let v = serde_json::from_slice::<WolOverview>(&b).unwrap().devices.remove(0);
    assert_eq!(v.status, WolStatus::Online);
    assert_eq!(v.history.len(), 1);
    assert_eq!(v.history[0].end, WolWakeEnd::Online);
    assert_eq!(v.history[0].by, rynas_api_types::wol::WolTrigger::User { name: "bob".into() });
    // on already: a quick end
    let (_, b) = call(&w.app, "bob", "POST", "/v1/wol/devices/tv/wake", None, None).await;
    let s2: WolWakeStarted = serde_json::from_slice(&b).unwrap();
    w.ops.tasks.wait(&s2.upid).await.unwrap();
    let (_, b) = call(&w.app, "bob", "GET", "/v1/wol", None, None).await;
    let v = serde_json::from_slice::<WolOverview>(&b).unwrap().devices.remove(0);
    assert_eq!(v.history[0].end, WolWakeEnd::AlreadyOn);
    // the packets went out anyway (it may have just gone to sleep)
    assert_eq!(w.net.sent.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn an_old_address_taken_by_another_host_is_not_the_device() {
    let w = world();
    let (s, b) = call(&w.app, "admin", "POST", "/v1/wol/devices", Some(json!({"name": "Old", "macs": ["a8:00:00:00:00:88"], "host": "192.0.2.88"})), None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    // something answers at the address, but the neighbor table names another card
    w.net.awake.store(true, Ordering::SeqCst);
    *w.net.extra.lock().unwrap() = vec![Neighbor { ip: Ipv4Addr::new(192, 0, 2, 88), dev: "eth0".into(), mac: "a8:00:00:00:00:99".into(), reachable: true }];
    let status = || {
        let (wol, app) = (w.wol.clone(), w.app.clone());
        async move {
            let devices = wol.devices();
            tokio::task::spawn_blocking(move || wol.round(&devices)).await.unwrap();
            let (_, b) = call(&app, "admin", "GET", "/v1/wol", None, None).await;
            let v = serde_json::from_slice::<WolOverview>(&b).unwrap().devices.remove(0);
            (v.status, v.ip)
        }
    };
    assert_eq!(status().await.0, WolStatus::Offline);
    // the device itself at a new address: found by its MAC, and on
    *w.net.extra.lock().unwrap() = vec![Neighbor { ip: Ipv4Addr::new(192, 0, 2, 99), dev: "eth0".into(), mac: "a8:00:00:00:00:88".into(), reachable: false }];
    assert_eq!(status().await, (WolStatus::Online, Some("192.0.2.99".into())));
}
