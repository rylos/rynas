//! Web access (W1): the Caddy render against a golden file, access ranges, local names and
//! their collisions, validation, applies against a fake admin socket (load, last-good,
//! certificate, refusal and rollback), anti-drift, host facts changes, external mode, the
//! timed confirmation, and the HTTP routes.

mod fake_caddy;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use fake_caddy::FakeCaddy;
use rynas_api_types::apps::WebAuth;
use rynas_api_types::web_access::*;
use rynas_api_types::{ApiError, error_codes};
use rynas_auth::ManualClock;
use rynas_auth::backend::{AuthError, Authenticator, BoxFuture, Identity};
use rynas_auth::totp::{MemoryStorage, TotpManager};
use rynas_config::{Applier, Config, ConfigOptions, MemorySystem};
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::ops::Ops;
use rynasd::roles::AdminPolicy;
use rynasd::server::{DaemonState, router};
use rynasd::web_access::diag::NetProbe;
use rynasd::web_access::model::{self, Cidr, HostFacts};
use rynasd::web_access::{WEB_ACCESS_KEY, WebAccessDomain, WebAccessSvc, avahi_block, resolve};
use serde_json::{Value, json};
use tower::ServiceExt;

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

fn facts() -> HostFacts {
    HostFacts {
        hostname: "NAS.example.lan".into(),
        lan: [
            Cidr::parse("192.0.2.0/24").unwrap(),
            Cidr::parse("2001:db8:1::/64").unwrap(),
        ]
        .into(),
        tailscale: true,
        wireguard: [Cidr::parse("10.8.0.0/24").unwrap()].into(),
        primary_v4: Some("192.0.2.10".parse().unwrap()),
        addrs: ["192.0.2.10".parse().unwrap()].into(),
    }
}

fn route(id: &str, label: &str, port: u16) -> WebRoute {
    WebRoute {
        id: id.into(),
        kind: RouteKind::App,
        stack: Some(label.into()),
        app: Some(label.into()),
        label: label.into(),
        upstream: WebUpstream {
            port,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn sample() -> WebAccessConfig {
    let mut immich = route("app-immich", "immich", 2283);
    immich.limits = RouteLimits {
        body: Some("50G".into()),
        timeout: Some("600s".into()),
        buffering: false,
    };
    immich.access = AccessLevel::LanVpn;
    immich.allow = vec!["198.51.100.7".into()];
    let mut portainer = route("app-portainer", "portainer", 9443);
    portainer.upstream.scheme = UpstreamScheme::Https;
    portainer.headers = HeaderPreset::Relaxed;
    WebAccessConfig {
        settings: WebAccessSettings::default(),
        routes: vec![portainer, immich],
    }
}

struct FakeProbe {
    resolve: Mutex<Result<Vec<IpAddr>, String>>,
    open: Mutex<Vec<SocketAddr>>,
}

struct ProbeRef(Arc<FakeProbe>);

impl NetProbe for ProbeRef {
    fn resolve(&self, _name: &str) -> Result<Vec<IpAddr>, String> {
        self.0.resolve.lock().unwrap().clone()
    }
    fn connect(&self, addr: SocketAddr) -> Result<(), String> {
        if self.0.open.lock().unwrap().contains(&addr) {
            Ok(())
        } else {
            Err("connection refused".into())
        }
    }
}

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

struct Env {
    dir: tempfile::TempDir,
    svc: Arc<WebAccessSvc>,
    ops: Arc<Ops>,
    system: Arc<MemorySystem>,
    caddy: FakeCaddy,
    facts: Arc<Mutex<HostFacts>>,
    probe: Arc<FakeProbe>,
}

impl Env {
    fn root(&self) -> PathBuf {
        self.dir.path().join("root")
    }
    fn file(&self, abs: &str) -> PathBuf {
        self.root().join(abs.trim_start_matches('/'))
    }
    fn config(&self) -> &Config {
        &self.ops.config
    }
    fn applied(&self) -> WebAccessConfig {
        serde_json::from_value(self.config().applied(WEB_ACCESS_KEY).unwrap().unwrap()).unwrap()
    }
    fn apply(&self, c: &WebAccessConfig) -> Result<u64, String> {
        self.config()
            .apply_value(
                "admin",
                WEB_ACCESS_KEY,
                Some(serde_json::to_value(c).unwrap()),
                "test",
                &|_| {},
            )
            .map_err(|e| e.to_string())
    }
    fn app(&self) -> axum::Router {
        let clock = Arc::new(ManualClock::new(1_800_000_000_000));
        let totp = Arc::new(TotpManager::new(
            MemoryStorage::default(),
            clock.clone(),
            "rynas",
        ));
        let mut st = DaemonState::new(Arc::new(NoAuth), totp, clock);
        st.ops = Some(self.ops.clone());
        st.web_access = Some(self.svc.clone());
        router(st)
    }
}

fn env_with(confirm: Duration, installed: bool) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("etc/avahi")).unwrap();
    std::fs::write(
        root.join("etc/avahi/hosts"),
        "# the administrator's line\n192.0.2.99 printer.local\n",
    )
    .unwrap();
    let caddy = FakeCaddy::start(dir.path());
    let facts = Arc::new(Mutex::new(facts()));
    let f2 = facts.clone();
    let ca = rynas_tls::generate_ca("nas", rynas_tls::now()).unwrap();
    let probe = Arc::new(FakeProbe {
        resolve: Mutex::new(Ok(vec!["192.0.2.10".parse().unwrap()])),
        open: Mutex::new(Vec::new()),
    });
    let mut svc = WebAccessSvc::new(
        root.clone(),
        caddy.socket.clone(),
        8080,
        8443,
        Box::new(move || f2.lock().unwrap().clone()),
        Box::new(move || Ok(ca.clone())),
        Box::new(ProbeRef(probe.clone())),
    );
    svc.confirm_timeout = confirm;
    svc.installed_override = Some(installed);
    let svc = Arc::new(svc);
    let system = Arc::new(MemorySystem::new("nas"));
    let domains: Vec<Arc<dyn Applier>> = vec![Arc::new(WebAccessDomain { svc: svc.clone() })];
    let config = Arc::new(
        Config::open(
            ConfigOptions::new(dir.path().join("state"), &root),
            system.clone(),
            domains,
        )
        .unwrap(),
    );
    let tasks = TaskManager::new(dir.path().join("tasks"), "nas", TaskConfig::default()).unwrap();
    let mut ops = Ops::new(config, tasks);
    ops.admins = AdminPolicy::new(vec!["rynas-admin".into()], |u| match u {
        "admin" => Some(vec!["rynas-admin".into()]),
        "bob" => Some(vec!["users".into()]),
        _ => None,
    });
    let ops = Arc::new(ops);
    let _ = svc.ops.set(ops.clone());
    Env {
        dir,
        svc,
        ops,
        system,
        caddy,
        facts,
        probe,
    }
}

fn env() -> Env {
    env_with(Duration::from_secs(120), true)
}

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

/// Compares with a golden file; `RYNAS_UPDATE_GOLDEN=1` rewrites it.
fn assert_golden(name: &str, v: &Value) {
    let path = golden(name);
    let text = serde_json::to_string_pretty(v).unwrap() + "\n";
    if std::env::var_os("RYNAS_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(text, want, "render differs from {}", path.display());
}

// ---------------------------------------------------------------------------------------------
// render
// ---------------------------------------------------------------------------------------------

#[test]
fn caddy_render_matches_the_golden_file() {
    let e = env();
    let cfg = model::validate(sample(), &[8080, 8443]).unwrap();
    let (resolved, json) = e.svc.render(&cfg, &facts());
    assert_eq!(resolved.host, "nas");
    assert_eq!(resolved.ui, vec!["nas.local", "nas"]);
    assert_golden("web_access_caddy.json", &json);

    // h3 off, other ports, UI off: no UI route, the redirect carries the port
    let mut c2 = cfg.clone();
    c2.settings.h3 = false;
    c2.settings.https_port = 8444;
    c2.settings.http_port = 8081;
    c2.settings.ui.enabled = false;
    let (_, j2) = e.svc.render(&c2, &facts());
    let https = &j2["apps"]["http"]["servers"]["rynas_https"];
    assert_eq!(https["protocols"], json!(["h1", "h2"]));
    assert_eq!(https["listen"], json!([":8444"]));
    assert!(
        https["routes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["@id"] != "rynas-ui")
    );
    assert_eq!(
        j2["apps"]["http"]["servers"]["rynas_http"]["routes"][0]["handle"][0]["headers"]["Location"]
            [0],
        "https://{http.request.host}:8444{http.request.uri}"
    );

    // nothing to serve: only the plain HTTP server (redirects to :8443), no TLS app
    let mut c3 = WebAccessConfig::default();
    c3.settings.ui.enabled = false;
    let (_, j3) = e.svc.render(&c3, &facts());
    assert!(j3["apps"]["tls"].is_null());
    assert!(j3["apps"]["http"]["servers"]["rynas_https"].is_null());

    // external mode: the engine does not run
    let mut c4 = cfg;
    c4.settings.mode = ProxyMode::External;
    assert!(e.svc.render(&c4, &facts()).1.is_null());
    assert_eq!(avahi_block(&c4, &facts()), "");
}

#[test]
fn avahi_lines_for_route_names_only() {
    let cfg = sample();
    assert_eq!(
        avahi_block(&cfg, &facts()),
        "192.0.2.10 immich-nas.local\n192.0.2.10 portainer-nas.local\n"
    );
    let mut off = cfg.clone();
    off.settings.local_names = false;
    assert_eq!(avahi_block(&off, &facts()), "");
    let mut no_v4 = facts();
    no_v4.primary_v4 = None;
    assert_eq!(avahi_block(&cfg, &no_v4), "");
}

// ---------------------------------------------------------------------------------------------
// access ranges and host facts
// ---------------------------------------------------------------------------------------------

#[test]
fn access_ranges_by_level() {
    let f = facts();
    let lan = model::access_ranges(AccessLevel::Lan, &f, &[]);
    assert_eq!(
        lan,
        [
            "127.0.0.0/8",
            "169.254.0.0/16",
            "192.0.2.0/24",
            "::1/128",
            "2001:db8:1::/64",
            "fe80::/10"
        ]
        .map(String::from)
        .to_vec()
    );
    let vpn = model::access_ranges(AccessLevel::LanVpn, &f, &["198.51.100.7".into()]);
    for want in [
        "100.64.0.0/10",
        "fd7a:115c:a1e0::/48",
        "10.8.0.0/24",
        "198.51.100.7/32",
        "192.0.2.0/24",
    ] {
        assert!(
            vpn.contains(&want.to_string()),
            "{want} missing from {vpn:?}"
        );
    }
    // without tailscale0 the tailnet ranges are never trusted
    let mut no_ts = f.clone();
    no_ts.tailscale = false;
    let vpn = model::access_ranges(AccessLevel::LanVpn, &no_ts, &[]);
    assert!(
        !vpn.iter()
            .any(|r| r.starts_with("100.64.") || r.starts_with("fd7a:"))
    );
    assert!(model::access_ranges(AccessLevel::Public, &f, &[]).is_empty());
}

#[test]
fn host_facts_from_ip_json() {
    let addr = json!([
        {"ifname": "lo", "link_type": "loopback", "addr_info": [{"local": "127.0.0.1", "prefixlen": 8}]},
        {"ifname": "enp1s0", "link_type": "ether", "addr_info": [
            {"local": "192.0.2.10", "prefixlen": 24},
            {"local": "2001:db8:1::10", "prefixlen": 64},
            {"local": "fe80::1", "prefixlen": 64}
        ]},
        {"ifname": "enp2s0", "link_type": "ether", "addr_info": [{"local": "198.51.100.4", "prefixlen": 25}]},
        {"ifname": "docker0", "link_type": "ether", "addr_info": [{"local": "172.17.0.1", "prefixlen": 16}]},
        {"ifname": "veth12", "link_type": "ether", "addr_info": [{"local": "169.254.3.3", "prefixlen": 16}]},
        {"ifname": "tailscale0", "link_type": "none", "addr_info": [{"local": "100.101.102.103", "prefixlen": 32}]},
        {"ifname": "wg0", "link_type": "none", "addr_info": [{"local": "10.8.0.1", "prefixlen": 24}]}
    ]);
    let route = json!([{"dst": "default", "gateway": "198.51.100.1", "dev": "enp2s0"}]);
    let f = model::parse_facts("Nas01\n", &addr.to_string(), &route.to_string());
    assert_eq!(f.host(), "nas01");
    assert_eq!(
        f.lan.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
        vec!["192.0.2.0/24", "198.51.100.0/25", "2001:db8:1::/64"]
    );
    assert!(f.tailscale);
    assert_eq!(
        f.wireguard
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>(),
        vec!["10.8.0.0/24"]
    );
    // the interface with the default route
    assert_eq!(f.primary_v4, Some("198.51.100.4".parse().unwrap()));
    assert!(f.addrs.contains(&"fe80::1".parse().unwrap()));
    assert!(!f.addrs.contains(&"172.17.0.1".parse().unwrap()));
    // no default route: the first LAN address
    let f = model::parse_facts("nas", &addr.to_string(), "[]");
    assert_eq!(f.primary_v4, Some("192.0.2.10".parse().unwrap()));
}

// ---------------------------------------------------------------------------------------------
// names and validation
// ---------------------------------------------------------------------------------------------

#[test]
fn local_names_and_collisions() {
    let taken = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
    assert_eq!(model::suggest_label("immich", &taken(&[])), "immich");
    assert_eq!(model::suggest_label("My_App", &taken(&[])), "my-app");
    assert_eq!(
        model::suggest_label("my_app", &taken(&["my-app"])),
        "my-app-2"
    );
    assert_eq!(
        model::suggest_label("my_app", &taken(&["my-app", "my-app-2"])),
        "my-app-3"
    );
    assert_eq!(model::suggest_label("__", &taken(&[])), "app");
    assert!(model::suggest_label(&"x".repeat(80), &taken(&[])).len() <= model::MAX_LABEL);
    assert_eq!(
        model::suggest_id("my_app", &taken(&["app-my-app"])),
        "app-my-app-2"
    );
    assert_eq!(
        model::local_name("immich", "nas").as_deref(),
        Some("immich-nas.local")
    );
    assert_eq!(model::local_name(&"a".repeat(40), &"h".repeat(23)), None);
    assert_eq!(model::host_label("NAS.example.lan"), "nas");
    assert_eq!(model::host_label("bad_name"), "nas");

    // a name too long for this host is reported, not served
    let mut f = facts();
    f.hostname = "h".repeat(30);
    let mut c = WebAccessConfig::default();
    c.routes.push(route("app-long", &"a".repeat(40), 1000));
    let r = resolve(&c, &f);
    assert!(r.routes[0].names.is_empty());
    assert_eq!(r.routes[0].problems, vec!["name_too_long"]);
}

#[test]
fn validation_refusals() {
    let ok = |c: WebAccessConfig| model::validate(c, &[8080, 8443]);
    let field = |c: WebAccessConfig| ok(c).unwrap_err().field.unwrap();
    let mut c = sample();
    c.routes[1].label = "portainer".into();
    assert_eq!(field(c), "routes[1].label");
    let mut c = sample();
    c.routes[1].id = "app-portainer".into();
    assert_eq!(field(c), "routes[1].id");
    let mut c = sample();
    c.routes[0].access = AccessLevel::Public;
    assert_eq!(field(c), "routes[0].access");
    let mut c = sample();
    c.routes[0].sso = Some(SsoRule::default());
    assert_eq!(field(c), "routes[0].sso");
    let mut c = sample();
    c.routes[0].kind = RouteKind::Custom;
    assert_eq!(field(c), "routes[0].kind");
    let mut c = sample();
    c.routes[0].upstream.host = "192.0.2.50".into();
    assert_eq!(field(c), "routes[0].upstream.host");
    let mut c = sample();
    c.routes[1].auth = WebAuth::None;
    assert_eq!(field(c), "routes[1].access");
    let mut c = sample();
    c.routes[0].limits.body = Some("5T".into());
    assert_eq!(field(c), "routes[0].limits.body");
    let mut c = sample();
    c.routes[0].allow = vec!["0.0.0.0/0".into()];
    assert_eq!(field(c), "routes[0].allow");
    let mut c = sample();
    c.settings.https_port = 8443;
    assert_eq!(field(c), "settings.https_port");
    let mut c = sample();
    c.settings.ui.access = AccessLevel::Public;
    assert_eq!(field(c), "settings.ui.access");
    // normalised: label lowercased, allow as CIDR, IPv6 upstream without brackets
    let mut c = sample();
    c.routes[0].label = "Portainer".into();
    c.routes[1].upstream.host = "[::1]".into();
    let n = ok(c).unwrap();
    assert_eq!(n.routes[0].label, "portainer");
    assert_eq!(n.routes[1].allow, vec!["198.51.100.7/32"]);
    assert_eq!(n.routes[1].upstream.host, "::1");
}

// ---------------------------------------------------------------------------------------------
// applies against the fake admin socket
// ---------------------------------------------------------------------------------------------

#[test]
fn apply_loads_writes_last_good_certificate_and_avahi() {
    let e = env();
    e.apply(&sample()).unwrap();
    assert_eq!(e.caddy.loads(), 1);
    let (cfg, _) = (e.applied(), ());
    let (_, want) = e.svc.render(&cfg, &facts());
    assert_eq!(e.caddy.config(), want);
    // the first load of a new certificate forces a reload
    assert!(e.caddy.state.lock().unwrap().loads[0].1);
    let last_good: Value = serde_json::from_str(
        &std::fs::read_to_string(e.file(rynasd::web_access::LAST_GOOD)).unwrap(),
    )
    .unwrap();
    assert_eq!(last_good, want);
    let pem = std::fs::read_to_string(e.file(rynasd::web_access::CERT)).unwrap();
    let chain = rynas_tls::inspect(&pem).unwrap();
    for n in [
        "nas.local",
        "nas",
        "immich-nas.local",
        "portainer-nas.local",
    ] {
        assert!(
            chain[0].dns_names.contains(&n.to_string()),
            "{n}: {:?}",
            chain[0].dns_names
        );
    }
    assert!(e.file(rynasd::web_access::KEY).exists());
    // rynas-web learns that the proxy fronts the interface (its plain port follows)
    assert_eq!(
        std::fs::read_to_string(e.file(rynasd::web_access::UI_STATE)).unwrap(),
        "nas.local\nnas\n"
    );
    let hosts = std::fs::read_to_string(e.file("/etc/avahi/hosts")).unwrap();
    assert!(hosts.contains("192.0.2.99 printer.local"), "{hosts}");
    assert!(hosts.contains("192.0.2.10 immich-nas.local"), "{hosts}");
    // the unit was started (reported inactive) and Avahi reloaded
    let calls = e.system.state().calls.clone();
    assert!(
        calls
            .iter()
            .any(|c| c.contains("reload avahi-daemon.service")),
        "{calls:?}"
    );

    // the same value again: the running configuration matches, nothing is loaded
    e.config().drift_overwrite(WEB_ACCESS_KEY, &|_| {}).unwrap();
    assert_eq!(e.caddy.loads(), 1);
    let st = e.svc.engine_state();
    assert!(st.installed && st.running && st.in_sync == Some(true) && st.last_error.is_none());
}

#[test]
fn engine_refusal_fails_the_apply_and_rolls_back() {
    let e = env();
    e.apply(&sample()).unwrap();
    let before = e.applied();
    e.caddy.state.lock().unwrap().refuse = Some("loading new config: http app module: bad".into());
    let mut c = sample();
    c.routes.push(route("app-jellyfin", "jellyfin", 8096));
    let err = e.apply(&c).unwrap_err();
    assert!(err.contains("bad"), "{err}");
    assert_eq!(e.applied(), before);
    // the engine kept the previous configuration: the rollback finds it running and loads
    // nothing
    assert_eq!(e.caddy.state.lock().unwrap().refused, 1);
    assert_eq!(e.caddy.config(), e.svc.render(&before, &facts()).1);
    assert!(e.svc.engine_state().last_error.unwrap().contains("bad"));
    // last-good stays the previous configuration
    let last_good: Value = serde_json::from_str(
        &std::fs::read_to_string(e.file(rynasd::web_access::LAST_GOOD)).unwrap(),
    )
    .unwrap();
    assert!(!last_good.to_string().contains("jellyfin"));
}

#[test]
fn drift_and_host_changes_are_followed() {
    let e = env();
    // never applied: the first sync applies the defaults (the interface behind the proxy)
    e.svc.sync(&|_| {}).unwrap();
    assert_eq!(e.applied(), WebAccessConfig::default());
    assert_eq!(e.caddy.loads(), 1);
    e.apply(&sample()).unwrap();
    let loads = e.caddy.loads();
    // someone changed the running configuration through the socket
    e.caddy.state.lock().unwrap().config = json!({"apps": {}});
    e.svc.sync(&|_| {}).unwrap();
    assert_eq!(e.caddy.loads(), loads + 1);
    assert_eq!(e.caddy.config(), e.svc.render(&e.applied(), &facts()).1);
    // in sync: nothing
    e.svc.sync(&|_| {}).unwrap();
    assert_eq!(e.caddy.loads(), loads + 1);
    // the NAS got another address and name: Avahi lines and certificate follow
    {
        let mut f = e.facts.lock().unwrap();
        f.primary_v4 = Some("192.0.2.20".parse().unwrap());
        f.hostname = "store".into();
    }
    e.svc.sync(&|_| {}).unwrap();
    let hosts = std::fs::read_to_string(e.file("/etc/avahi/hosts")).unwrap();
    assert!(
        hosts.contains("192.0.2.20 immich-store.local") && !hosts.contains("immich-nas"),
        "{hosts}"
    );
    assert!(e.caddy.config().to_string().contains("immich-store.local"));
    assert!(
        e.caddy.state.lock().unwrap().loads.last().unwrap().1,
        "new certificate: forced"
    );
}

#[test]
fn external_mode_releases_the_ports() {
    let e = env();
    e.apply(&sample()).unwrap();
    assert_eq!(
        e.svc.reserved_ports(),
        vec![(80, false), (443, false), (443, true)]
    );
    let mut c = sample();
    c.settings.mode = ProxyMode::External;
    e.apply(&c).unwrap();
    let calls = e.system.state().calls.clone();
    assert!(
        calls
            .iter()
            .any(|c| c == "run systemctl disable --now rynas-caddy.service"),
        "{calls:?}"
    );
    assert!(e.svc.reserved_ports().is_empty());
    assert!(!e.svc.proxy_active());
    // rynas-web opens its plain port to the network again
    assert!(!e.file(rynasd::web_access::UI_STATE).exists());
    let hosts = std::fs::read_to_string(e.file("/etc/avahi/hosts")).unwrap();
    assert!(!hosts.contains("immich-nas"), "{hosts}");
    // and back: enabled and started again
    e.system
        .state()
        .inactive
        .push(rynasd::web_access::UNIT.into());
    e.apply(&sample()).unwrap();
    let calls = e.system.state().calls.clone();
    assert!(
        calls
            .iter()
            .any(|c| c == "run systemctl enable --now rynas-caddy.service"),
        "{calls:?}"
    );
}

#[test]
fn not_installed_keeps_the_settings_only() {
    let e = env_with(Duration::from_secs(120), false);
    e.apply(&sample()).unwrap();
    assert_eq!(e.caddy.loads(), 0);
    assert!(!e.svc.proxy_active());
    assert!(!e.file(rynasd::web_access::UI_STATE).exists());
    // the ports are reserved anyway: the proxy owns them from install
    assert!(!e.svc.reserved_ports().is_empty());
}

// ---------------------------------------------------------------------------------------------
// timed confirmation
// ---------------------------------------------------------------------------------------------

fn wait_until(mut f: impl FnMut() -> bool) {
    for _ in 0..100 {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("condition not reached");
}

#[test]
fn ui_route_change_without_confirmation_comes_back() {
    let e = env_with(Duration::from_millis(300), true);
    e.apply(&sample()).unwrap();
    // a change of a route only: nothing to confirm
    let mut c = sample();
    c.routes[0].upstream.port = 9444;
    e.apply(&c).unwrap();
    assert!(e.svc.state("admin").unwrap().confirm.is_none());
    let before = e.applied();
    // the interface's access changes: confirmation needed
    let mut c2 = before.clone();
    c2.settings.ui.allow = vec!["198.51.100.0/24".into()];
    e.apply(&c2).unwrap();
    let pending = e.svc.state("admin").unwrap().confirm.unwrap();
    assert_eq!(pending.names, vec!["nas.local", "nas"]);
    wait_until(|| e.applied() == before);
    wait_until(|| e.svc.state("admin").unwrap().confirm.is_none());
    assert!(!e.caddy.config().to_string().contains("198.51.100.0/24"));
}

#[test]
fn confirmation_only_through_the_proxy() {
    let e = env_with(Duration::from_millis(1500), true);
    e.apply(&sample()).unwrap();
    let mut c = e.applied();
    c.settings.ui.access = AccessLevel::LanVpn;
    e.apply(&c).unwrap();
    // over :8443 (not through the proxy): refused
    let direct: SocketAddr = "192.0.2.10:8443".parse().unwrap();
    let err = e
        .svc
        .confirm(Some("nas.local:8443"), Some(direct))
        .unwrap_err();
    assert_eq!(err.code(), error_codes::CONFLICT);
    // through the proxy on another name: refused
    let via: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    assert!(e.svc.confirm(Some("other.local"), Some(via)).is_err());
    // through the proxy on a new name: accepted, and it stays
    e.svc.confirm(Some("NAS.local"), Some(via)).unwrap();
    std::thread::sleep(Duration::from_millis(1800));
    assert_eq!(e.applied().settings.ui.access, AccessLevel::LanVpn);
    let err = e.svc.confirm(Some("nas.local"), Some(via)).unwrap_err();
    assert_eq!(err.code(), error_codes::NOTHING_PENDING);
}

// ---------------------------------------------------------------------------------------------
// HTTP routes
// ---------------------------------------------------------------------------------------------

async fn call(
    app: &axum::Router,
    who: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-rynas-user", who)
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
        .unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    (s, to_bytes(r.into_body(), 4 << 20).await.unwrap().to_vec())
}

#[tokio::test]
async fn routes_state_stage_plan_and_test() {
    let e = env();
    let app = e.app();
    let (s, _) = call(&app, "bob", "GET", "/v1/web-access", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = call(
        &app,
        "admin",
        "PUT",
        "/v1/web-access",
        Some(serde_json::to_value(sample()).unwrap()),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let st: WebAccessState = serde_json::from_slice(&b).unwrap();
    assert_eq!(st.pending.as_ref().unwrap().routes.len(), 2);
    assert_eq!(st.host, "nas");
    assert_eq!(st.emergency_port, 8443);
    assert_eq!(st.reserved_ports, vec![80, 443]);
    assert!(st.lan.contains(&"192.0.2.0/24".to_string()));
    // a bad value names its field
    let mut bad = sample();
    bad.routes[0].access = AccessLevel::Public;
    let (s, b) = call(
        &app,
        "admin",
        "PUT",
        "/v1/web-access",
        Some(serde_json::to_value(bad).unwrap()),
    )
    .await;
    let err: ApiError = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        (s, err.field.as_deref()),
        (StatusCode::BAD_REQUEST, Some("routes[0].access"))
    );
    // the plan of the staged value
    let (s, b) = call(&app, "admin", "POST", "/v1/web-access/plan", None).await;
    assert_eq!(s, StatusCode::OK);
    let plan: WebAccessPlan = serde_json::from_slice(&b).unwrap();
    assert!(plan.names.contains(&"immich-nas.local".to_string()));
    assert!(plan.avahi_hosts.contains("immich-nas.local"));
    assert!(plan.engine_config["apps"]["http"].is_object());

    // applied: views and diagnostics
    e.apply(&sample()).unwrap();
    let (s, b) = call(&app, "admin", "GET", "/v1/web-access", None).await;
    assert_eq!(s, StatusCode::OK);
    let st: WebAccessState = serde_json::from_slice(&b).unwrap();
    let immich = st.routes.iter().find(|r| r.id == "app-immich").unwrap();
    assert_eq!(immich.urls, vec!["https://immich-nas.local/"]);
    assert_eq!(immich.upstream, "http://127.0.0.1:2283");
    assert_eq!(st.ui_urls, vec!["https://nas.local/", "https://nas/"]);
    assert!(st.engine.in_sync == Some(true));

    e.probe
        .open
        .lock()
        .unwrap()
        .push("127.0.0.1:443".parse().unwrap());
    let (s, b) = call(
        &app,
        "admin",
        "POST",
        "/v1/web-access/routes/app-immich/test",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let d: RouteDiagnostics = serde_json::from_slice(&b).unwrap();
    let state = |id: &str| d.checks.iter().find(|c| c.id == id).unwrap().state;
    assert_eq!(state("dns"), CheckState::Ok);
    assert_eq!(state("proxy_running"), CheckState::Ok);
    assert_eq!(state("route_loaded"), CheckState::Ok);
    assert_eq!(state("https_listening"), CheckState::Ok);
    assert_eq!(state("certificate"), CheckState::Ok);
    assert_eq!(state("upstream"), CheckState::Fail, "{d:?}");
    // the name points elsewhere
    *e.probe.resolve.lock().unwrap() = Ok(vec!["192.0.2.77".parse().unwrap()]);
    let (_, b) = call(
        &app,
        "admin",
        "POST",
        "/v1/web-access/routes/app-immich/test",
        None,
    )
    .await;
    let d: RouteDiagnostics = serde_json::from_slice(&b).unwrap();
    assert_eq!(d.checks[0].state, CheckState::Fail);
    let (s, _) = call(
        &app,
        "admin",
        "POST",
        "/v1/web-access/routes/nope/test",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // sync answers the engine state; confirm without anything pending
    let (s, b) = call(&app, "admin", "POST", "/v1/web-access/sync", None).await;
    assert_eq!(s, StatusCode::OK);
    let eng: ProxyEngine = serde_json::from_slice(&b).unwrap();
    assert!(eng.running);
    let (s, b) = call(&app, "admin", "POST", "/v1/web-access/confirm", None).await;
    let err: ApiError = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        (s, err.code.as_str()),
        (StatusCode::CONFLICT, error_codes::NOTHING_PENDING)
    );
}
