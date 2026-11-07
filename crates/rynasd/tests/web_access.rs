//! Web access (W1): the Caddy render against a golden file, access ranges, local names and
//! their collisions, validation, applies against a fake admin socket (load, last-good,
//! certificate, refusal and rollback), anti-drift, host facts changes, external mode, the
//! timed confirmation, and the HTTP routes.

mod fake_caddy;

use std::collections::BTreeMap;
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
use rynas_dns::wire::Answer;
use rynas_dns::{Creds, DnsApi, DnsError, RecordSet, Rtype, Zone};
use rynas_notify::MemoryNotifier;
use rynas_notify::engine::secrets::SecretBox;
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
        primary_v6: None,
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
        ..Default::default()
    }
}

struct FakeProbe {
    resolve: Mutex<Result<Vec<IpAddr>, String>>,
    open: Mutex<Vec<SocketAddr>>,
    /// Direct DNS answers by server (W2 checklist).
    answers: Mutex<BTreeMap<SocketAddr, Answer>>,
    lan: Mutex<Vec<SocketAddr>>,
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
    fn query(&self, server: SocketAddr, _name: &str, _t: Rtype) -> Result<Answer, String> {
        self.0
            .answers
            .lock()
            .unwrap()
            .get(&server)
            .cloned()
            .ok_or_else(|| format!("{server}: no answer"))
    }
    fn lan_resolvers(&self) -> Vec<SocketAddr> {
        self.0.lan.lock().unwrap().clone()
    }
}

/// A DNS provider in memory (W2 records), shared with the test.
#[derive(Default)]
struct MemDns {
    sets: Mutex<BTreeMap<(String, Rtype), Vec<String>>>,
    calls: Mutex<Vec<String>>,
    /// Creds each client was built with.
    creds: Mutex<Vec<Creds>>,
    fail: Mutex<bool>,
    /// Each read waits this long (a slow provider).
    slow: Mutex<Option<Duration>>,
}

struct MemDnsRef(Arc<MemDns>);

impl DnsApi for MemDnsRef {
    fn zone(&self, fqdn: &str) -> Result<Zone, DnsError> {
        if fqdn.ends_with("example.com") {
            Ok(Zone {
                id: "z".into(),
                name: "example.com".into(),
                min_ttl: 60,
            })
        } else {
            Err(DnsError::NoZone(fqdn.into()))
        }
    }
    fn get(&self, _z: &Zone, n: &str, t: Rtype) -> Result<Option<RecordSet>, DnsError> {
        let slow = *self.0.slow.lock().unwrap();
        if let Some(d) = slow {
            std::thread::sleep(d);
        }
        Ok(self
            .0
            .sets
            .lock()
            .unwrap()
            .get(&(n.to_string(), t))
            .map(|v| RecordSet {
                values: v.clone(),
                ttl: 300,
            }))
    }
    fn set(&self, _z: &Zone, n: &str, t: Rtype, v: &[String], _ttl: u32) -> Result<(), DnsError> {
        self.0
            .calls
            .lock()
            .unwrap()
            .push(format!("set {n} {t} {}", v.join(",")));
        if *self.0.fail.lock().unwrap() {
            return Err(DnsError::Api("rate limited".into()));
        }
        self.0
            .sets
            .lock()
            .unwrap()
            .insert((n.into(), t), v.to_vec());
        Ok(())
    }
    fn delete(&self, _z: &Zone, n: &str, t: Rtype) -> Result<(), DnsError> {
        self.0.calls.lock().unwrap().push(format!("delete {n} {t}"));
        self.0.sets.lock().unwrap().remove(&(n.to_string(), t));
        Ok(())
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
    dns: Arc<MemDns>,
    notes: Arc<MemoryNotifier>,
    engine_log: Arc<Mutex<String>>,
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
        answers: Mutex::new(BTreeMap::new()),
        lan: Mutex::new(Vec::new()),
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
    // W2: providers in memory, the age key, notifications, the engine's log
    svc.state_dir = dir.path().join("state");
    let dns = Arc::new(MemDns::default());
    let d2 = dns.clone();
    svc.dns = Box::new(move |p, c| {
        d2.creds.lock().unwrap().push(c.clone());
        Ok(rynas_dns::info(p)
            .records
            .then(|| Box::new(MemDnsRef(d2.clone())) as Box<dyn DnsApi>))
    });
    std::fs::create_dir_all(dir.path().join("state/notify")).unwrap();
    let _ = svc.secret_box.set(Arc::new(
        SecretBox::open(&dir.path().join("state/notify")).unwrap(),
    ));
    let notes = Arc::new(MemoryNotifier::default());
    let _ = svc
        .notifier
        .set(notes.clone() as Arc<dyn rynas_notify::Notifier>);
    let engine_log = Arc::new(Mutex::new(String::new()));
    let l2 = engine_log.clone();
    svc.engine_log = Box::new(move || l2.lock().unwrap().clone());
    svc.engine_log_ttl = Duration::ZERO;
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
        dns,
        notes,
        engine_log,
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
    c.routes[0].prefix = Some("/x".into());
    assert_eq!(field(c), "routes[0].prefix");
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
// extra ports and custom routes (§5.6)
// ---------------------------------------------------------------------------------------------

fn custom(id: &str, label: &str, host: &str, port: u16) -> WebRoute {
    WebRoute {
        id: id.into(),
        kind: RouteKind::Custom,
        label: label.into(),
        upstream: WebUpstream {
            host: host.into(),
            port,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// `sample()` + immich also on :8445, a router (HTTPS, self-signed, its own Host) only on
/// `https://nas.local:8444/`, and Grafana on another device below `/grafana` on the same
/// port.
fn sample_ports() -> WebAccessConfig {
    let mut c = sample();
    c.routes[1].ports = vec![8445];
    let mut router = custom("router", "router", "192.0.2.1", 8443);
    router.upstream.scheme = UpstreamScheme::Https;
    router.upstream.skip_verify = true;
    router.upstream.host_header = HostHeader::Upstream;
    router.ports = vec![8444];
    router.main_port = false;
    router.names = RouteNames {
        local: true,
        domain: false,
        nas: true,
    };
    let mut grafana = custom("grafana", "grafana", "Grafana.LAN", 3000);
    grafana.ports = vec![8444];
    grafana.names.nas = true;
    grafana.prefix = Some("/grafana/".into());
    grafana.access = AccessLevel::LanVpn;
    c.routes.push(router);
    c.routes.push(grafana);
    c
}

#[test]
fn extra_ports_and_custom_routes_render() {
    let e = env();
    let cfg = model::validate(sample_ports(), &[8080, 8443]).unwrap();
    // normalised: host lowercased, prefix without the final slash
    let g = cfg.routes.iter().find(|r| r.id == "grafana").unwrap();
    assert_eq!(g.upstream.host, "grafana.lan");
    assert_eq!(g.prefix.as_deref(), Some("/grafana"));
    let (resolved, json) = e.svc.render(&cfg, &facts());
    assert_golden("web_access_caddy_ports.json", &json);
    let servers = &json["apps"]["http"]["servers"];
    // one server per extra port, same TLS policies as the main one
    assert_eq!(servers["rynas_port_8444"]["listen"], json!([":8444"]));
    assert_eq!(servers["rynas_port_8445"]["listen"], json!([":8445"]));
    assert_eq!(
        servers["rynas_port_8444"]["tls_connection_policies"],
        servers["rynas_https"]["tls_connection_policies"]
    );
    let ids = |srv: &Value| -> Vec<String> {
        srv["routes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["@id"].as_str().map(String::from))
            .collect()
    };
    // the longest prefix first; the router is not on the main port
    assert_eq!(
        ids(&servers["rynas_port_8444"]),
        vec!["rynas-route-grafana:8444", "rynas-route-router:8444"]
    );
    assert!(!ids(&servers["rynas_https"]).contains(&"rynas-route-router".to_string()));
    assert!(ids(&servers["rynas_https"]).contains(&"rynas-route-grafana".to_string()));
    assert_eq!(ids(&servers["rynas_port_8445"]), vec!["rynas-route-app-immich:8445"]);
    // access rules on the extra port as on 443
    let site = &servers["rynas_port_8445"]["routes"][0];
    let main = servers["rynas_https"]["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["@id"] == "rynas-route-app-immich")
        .unwrap();
    assert_eq!(site["handle"], main["handle"]);
    assert_eq!(
        site["handle"][0]["routes"][0]["match"][0]["not"][0]["remote_ip"]["ranges"]
            .as_array()
            .unwrap()
            .len(),
        model::access_ranges(AccessLevel::LanVpn, &facts(), &["198.51.100.7".into()]).len()
    );
    let gsite = &servers["rynas_port_8444"]["routes"][0];
    assert_eq!(
        gsite["match"][0]["path"],
        json!(["/grafana", "/grafana/*"])
    );
    assert_eq!(
        gsite["match"][0]["host"],
        json!(["grafana-nas.local", "nas.local", "nas"])
    );
    // the router: insecure TLS, its own Host; Grafana plain HTTP by name
    let proxy_of = |site: &Value| {
        site["handle"][0]["routes"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["handle"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone()
    };
    let rp = proxy_of(&servers["rynas_port_8444"]["routes"][1]);
    assert_eq!(rp["upstreams"][0]["dial"], "192.0.2.1:8443");
    assert_eq!(rp["transport"]["tls"]["insecure_skip_verify"], true);
    assert_eq!(
        rp["headers"]["request"]["set"]["Host"][0],
        "{http.reverse_proxy.upstream.hostport}"
    );
    assert_eq!(proxy_of(gsite)["upstreams"][0]["dial"], "grafana.lan:3000");
    // `:80`: the router's own name goes to its port
    let http = servers["rynas_http"]["routes"].as_array().unwrap();
    assert_eq!(http[0]["match"][0]["host"], json!(["router-nas.local"]));
    assert_eq!(
        http[0]["handle"][0]["headers"]["Location"][0],
        "https://{http.request.host}:8444{http.request.uri}"
    );
    // the local leaf covers the NAS's names on the port even with the interface off
    let mut off = cfg.clone();
    off.settings.ui.enabled = false;
    let r2 = resolve(&off, &facts());
    assert!(r2.local_names().contains(&"nas.local".to_string()));
    assert!(resolved.all_names().contains(&"router-nas.local".to_string()));
    // verified TLS towards a device with a real certificate
    let mut v = cfg.clone();
    v.routes.iter_mut().find(|r| r.id == "router").unwrap().upstream.skip_verify = false;
    let (_, j) = e.svc.render(&v, &facts());
    let rp = proxy_of(&j["apps"]["http"]["servers"]["rynas_port_8444"]["routes"][1]);
    assert_eq!(rp["transport"]["tls"], json!({}));
    // a disabled route leaves its port
    let mut d = cfg;
    d.routes[1].enabled = false;
    let (_, j) = e.svc.render(&d, &facts());
    assert!(j["apps"]["http"]["servers"]["rynas_port_8445"].is_null());
}

#[test]
fn extra_port_and_custom_validation() {
    let ok = |c: WebAccessConfig| model::validate(c, &[8080, 8443]);
    let field = |c: WebAccessConfig| ok(c).unwrap_err().field.unwrap();
    let base = sample_ports;
    for (port, why) in [(443, "main"), (80, "http"), (8443, "emergency"), (22, "system")] {
        let mut c = base();
        c.routes[1].ports = vec![port];
        assert_eq!(field(c), "routes[1].ports", "{why}");
    }
    let mut c = base();
    c.routes[1].ports = (2000..2009).collect();
    assert_eq!(field(c), "routes[1].ports");
    let mut c = base();
    c.routes[1].ports.clear();
    c.routes[1].main_port = false;
    assert_eq!(field(c), "routes[1].main_port");
    let mut c = base();
    c.routes[1].ports.clear();
    c.routes[1].names.nas = true;
    assert_eq!(field(c), "routes[1].names.nas");
    // two routes on the NAS's names on one port need different prefixes
    let mut c = base();
    c.routes[3].prefix = None;
    assert_eq!(field(c), "routes[3].prefix");
    // the proxy itself as upstream
    let mut c = base();
    c.routes[2].upstream.host = "127.0.0.1".into();
    c.routes[2].upstream.port = 8445;
    assert_eq!(field(c), "routes[2].upstream.port");
    for bad in ["0.0.0.0", "224.0.0.1", "bad_host", "-x.lan", ""] {
        let mut c = base();
        c.routes[2].upstream.host = bad.into();
        assert_eq!(field(c), "routes[2].upstream.host", "{bad}");
    }
    let mut c = base();
    c.routes[3].prefix = Some("/a*".into());
    assert_eq!(field(c), "routes[3].prefix");
    // HTTP-01 needs port 80 on the name: not for an address only on another port
    let mut c = base();
    c.domains = vec![WebDomain {
        id: "home".into(),
        name: "nas.example.com".into(),
        challenge: ChallengeKind::Http01,
        lan_record: false,
        ..Default::default()
    }];
    assert_eq!(field(c.clone()), "routes[2].main_port");
    c.routes[2].main_port = true;
    let n = ok(c).unwrap();
    // the domain's own name joins its certificate once a route answers on it
    let d = &n.domains[0];
    assert!(
        rynasd::web_access::domains::cert_subjects(&n, d).contains(&"nas.example.com".to_string())
    );
    // skip_verify is dropped where it means nothing (plain HTTP, loopback)
    let mut c = base();
    c.routes[3].upstream.skip_verify = true;
    let n = ok(c).unwrap();
    assert!(!n.routes[3].upstream.skip_verify);
    // the unchanged sample serialises without the new fields
    let v = serde_json::to_value(model::validate(sample(), &[8080, 8443]).unwrap()).unwrap();
    let text = v.to_string();
    for k in ["ports", "main_port", "prefix", "skip_verify", "host_header", "\"nas\""] {
        assert!(!text.contains(k), "{k} in {text}");
    }
}

#[tokio::test]
async fn extra_ports_busy_state_and_checks() {
    let e = env();
    let app = e.app();
    // another program listens on 8444 (0x20FC)
    std::fs::create_dir_all(e.root().join("proc/net")).unwrap();
    std::fs::write(
        e.root().join("proc/net/tcp"),
        "  sl  local_address rem_address   st\n   0: 00000000:20FC 00000000:0000 0A 0\n   1: 00000000:20FD 00000000:0000 01 0\n",
    )
    .unwrap();
    let (s, b) = call(
        &app,
        "admin",
        "PUT",
        "/v1/web-access",
        Some(serde_json::to_value(sample_ports()).unwrap()),
    )
    .await;
    let err: ApiError = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        (s, err.field.as_deref()),
        (StatusCode::BAD_REQUEST, Some("routes[2].ports"))
    );
    std::fs::remove_file(e.root().join("proc/net/tcp")).unwrap();
    e.apply(&sample_ports()).unwrap();
    // the proxy holds the port now: keeping it is no conflict
    std::fs::write(
        e.root().join("proc/net/tcp"),
        "  sl  local_address rem_address   st\n   0: 00000000:20FC 00000000:0000 0A 0\n",
    )
    .unwrap();
    e.apply(&sample_ports()).unwrap();
    let (s, b) = call(&app, "admin", "GET", "/v1/web-access", None).await;
    assert_eq!(s, StatusCode::OK);
    let st: WebAccessState = serde_json::from_slice(&b).unwrap();
    assert_eq!(st.reserved_ports, vec![80, 443, 8444, 8445]);
    assert!(e.svc.reserved_ports().contains(&(8444, true)));
    let router = st.routes.iter().find(|r| r.id == "router").unwrap();
    assert_eq!(router.kind, RouteKind::Custom);
    assert!(!router.main_port);
    assert_eq!(
        router.urls,
        vec![
            "https://router-nas.local:8444/",
            "https://nas.local:8444/",
            "https://nas:8444/"
        ]
    );
    assert_eq!(router.upstream, "https://192.0.2.1:8443");
    let grafana = st.routes.iter().find(|r| r.id == "grafana").unwrap();
    assert_eq!(grafana.urls[0], "https://grafana-nas.local/");
    assert!(grafana.urls.contains(&"https://nas.local:8444/grafana/".to_string()));
    // checks: the port listens, the upstream (a host name) answers
    e.probe
        .open
        .lock()
        .unwrap()
        .push("127.0.0.1:8444".parse().unwrap());
    *e.probe.resolve.lock().unwrap() = Ok(vec!["192.0.2.10".parse().unwrap()]);
    e.probe
        .open
        .lock()
        .unwrap()
        .push("192.0.2.10:3000".parse().unwrap());
    let test = |id: &'static str| {
        let app = app.clone();
        async move {
            let (s, b) = call(
                &app,
                "admin",
                "POST",
                &format!("/v1/web-access/routes/{id}/test"),
                None,
            )
            .await;
            assert_eq!(s, StatusCode::OK);
            serde_json::from_slice::<RouteDiagnostics>(&b).unwrap()
        }
    };
    let d = test("grafana").await;
    let st = |d: &RouteDiagnostics, id: &str| d.checks.iter().find(|c| c.id == id).unwrap().state;
    assert_eq!(st(&d, "port_listening"), CheckState::Ok);
    assert_eq!(st(&d, "route_loaded"), CheckState::Ok, "{d:?}");
    assert_eq!(st(&d, "upstream"), CheckState::Ok, "{d:?}");
    let d = test("router").await;
    assert_eq!(st(&d, "https_listening"), CheckState::Skipped);
    assert_eq!(st(&d, "route_loaded"), CheckState::Ok, "{d:?}");
    assert_eq!(st(&d, "upstream"), CheckState::Fail);
    let d = test("app-immich").await;
    assert_eq!(st(&d, "port_listening"), CheckState::Fail);
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
    // local names go to the Avahi publisher; the hosts file is left alone, no reload
    let names = e.svc.names.desired();
    assert_eq!(
        names.keys().collect::<Vec<_>>(),
        ["immich-nas.local", "portainer-nas.local"]
    );
    assert!(
        names
            .values()
            .all(|a| *a == "192.0.2.10".parse::<IpAddr>().unwrap())
    );
    let hosts = std::fs::read_to_string(e.file("/etc/avahi/hosts")).unwrap();
    assert_eq!(
        hosts,
        "# the administrator's line\n192.0.2.99 printer.local\n"
    );
    let calls = e.system.state().calls.clone();
    assert!(
        !calls.iter().any(|c| c.contains("avahi-daemon.service")),
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
    let names = e.svc.names.desired();
    assert_eq!(
        names.get("immich-store.local"),
        Some(&"192.0.2.20".parse::<IpAddr>().unwrap())
    );
    assert!(!names.contains_key("immich-nas.local"), "{names:?}");
    assert!(e.caddy.config().to_string().contains("immich-store.local"));
    assert!(
        e.caddy.state.lock().unwrap().loads.last().unwrap().1,
        "new certificate: forced"
    );
}

const OLD_BLOCK: &str = "# BEGIN rynas web-access — managed by rynas, do not edit (sha256:00)\n192.0.2.10 immich-nas.local\n# END rynas web-access\n";

#[test]
fn old_avahi_hosts_block_is_removed_on_apply_and_at_start() {
    let e = env();
    let admin = "# the administrator's line\n192.0.2.99 printer.local\n";
    let hosts = e.file("/etc/avahi/hosts");
    // on apply: removed once, Avahi reloaded once
    std::fs::write(&hosts, format!("{admin}{OLD_BLOCK}")).unwrap();
    e.apply(&sample()).unwrap();
    assert_eq!(std::fs::read_to_string(&hosts).unwrap(), admin);
    let reloads = |e: &Env| {
        e.system
            .state()
            .calls
            .iter()
            .filter(|c| c.contains("avahi-daemon.service"))
            .count()
    };
    assert_eq!(reloads(&e), 1);
    e.apply(&sample()).unwrap();
    assert_eq!(reloads(&e), 1, "nothing left to migrate");
    // at start (a block left by an older rynasd, still in the drift records)
    std::fs::write(&hosts, format!("{admin}{OLD_BLOCK}")).unwrap();
    e.svc.sync(&|_| {}).unwrap();
    assert_eq!(std::fs::read_to_string(&hosts).unwrap(), admin);
    assert_eq!(reloads(&e), 2);
    let drift = e.config().drift().unwrap();
    assert!(
        drift.entries.iter().all(|d| !d.path.contains("avahi")),
        "{:?}",
        drift.entries
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
    assert!(e.svc.names.desired().is_empty());
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
    // no Avahi publisher in tests (as under --dry-system): skipped, first
    assert_eq!(d.checks[0].id, "local_name");
    assert_eq!(state("local_name"), CheckState::Skipped);
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
    let dns = d.checks.iter().find(|c| c.id == "dns").unwrap();
    assert_eq!(dns.state, CheckState::Fail);
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

// ---------------------------------------------------------------------------------------------
// wave W2: own domains, credentials, records, checklist, certificates
// ---------------------------------------------------------------------------------------------

const CF_TOKEN: &str = "cf-test-token-0123456789";

fn cred(id: &str, p: DnsProvider, values: &[(&str, &str)]) -> DnsCredential {
    DnsCredential {
        id: id.into(),
        provider: p,
        label: format!("{id} token"),
        secrets: values
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    }
}

/// `sample()` with an own domain on Cloudflare (DNS-01, wildcard, LAN records), a delegated
/// one on acme-dns, staging and an e-mail.
fn sample_w2() -> WebAccessConfig {
    let mut c = sample();
    c.credentials = vec![
        cred("cf", DnsProvider::Cloudflare, &[("api_token", CF_TOKEN)]),
        cred(
            "adns",
            DnsProvider::Acmedns,
            &[
                ("server_url", "https://auth.acme-dns.example.net"),
                ("username", "user-uuid-1"),
                ("subdomain", "sub-uuid-1"),
                ("password", "adns-password-secret"),
            ],
        ),
    ];
    c.domains = vec![
        WebDomain {
            id: "home".into(),
            name: "NAS.Example.com.".into(),
            provider: Some(DnsProvider::Cloudflare),
            credential: Some("cf".into()),
            ..Default::default()
        },
        WebDomain {
            id: "deleg".into(),
            name: "files.example.org".into(),
            provider: Some(DnsProvider::Acmedns),
            credential: Some("adns".into()),
            challenge: ChallengeKind::CnameDelegation,
            delegate_to: Some("sub-uuid-1.auth.acme-dns.example.net".into()),
            ui: false,
            ..Default::default()
        },
    ];
    c.acme = AcmeSettings {
        email: Some("admin@example.com".into()),
        staging: true,
        profile: Some("tlsserver".into()),
    };
    c
}

/// `sample_w2()` with only the Cloudflare domain, in production.
fn one_domain() -> WebAccessConfig {
    let mut c = sample_w2();
    c.domains.truncate(1);
    c.credentials.truncate(1);
    c.acme.staging = false;
    c
}

#[test]
fn domain_validation() {
    let ok = |c: WebAccessConfig| model::validate(c, &[8080, 8443]);
    let field = |c: WebAccessConfig| ok(c).unwrap_err().field.unwrap_or_default();
    let v = ok(sample_w2()).unwrap();
    assert_eq!(
        v.domains[0].name, "nas.example.com",
        "lowercase, no final dot"
    );
    let mut c = sample_w2();
    c.domains[0].name = "nas.local".into();
    assert_eq!(field(c), "domains[0].name");
    let mut c = sample_w2();
    c.domains[0].name = "*.example.com".into();
    assert_eq!(field(c), "domains[0].name");
    let mut c = sample_w2();
    c.domains[0].credential = Some("adns".into());
    assert_eq!(
        field(c),
        "domains[0].credential",
        "credential of another provider"
    );
    let mut c = sample_w2();
    c.domains[0].credential = None;
    assert_eq!(field(c), "domains[0].credential");
    let mut c = sample_w2();
    c.domains[1].challenge = ChallengeKind::Dns01;
    assert_eq!(field(c), "domains[1].provider", "acme-dns only delegated");
    let mut c = sample_w2();
    c.domains[1].delegate_to = None;
    assert_eq!(field(c), "domains[1].delegate_to");
    let mut c = sample_w2();
    c.domains[0].provider = Some(DnsProvider::Duckdns);
    c.credentials[0].provider = DnsProvider::Duckdns;
    c.credentials[0].secrets.clear();
    assert_eq!(
        field(c),
        "domains[0].name",
        "DuckDNS names are <x>.duckdns.org"
    );
    let mut c = sample_w2();
    c.domains[1].name = "nas.example.com".into();
    assert_eq!(field(c), "domains[1].name", "twice");
    let mut c = sample_w2();
    c.credentials[0].secrets.insert("zone".into(), "x".into());
    assert_eq!(field(c), "credentials[0].secrets.zone");
    let mut c = sample_w2();
    c.acme.email = Some("not an address".into());
    assert_eq!(field(c), "acme.email");
    // HTTP-01: no wildcard
    let mut c = sample_w2();
    c.domains[0].challenge = ChallengeKind::Http01;
    assert!(!ok(c).unwrap().domains[0].wildcard);
}

#[test]
fn domain_render_matches_the_golden_file() {
    let e = env();
    let mut c = sample_w2();
    e.svc.seal_input(&mut c, &[]).unwrap();
    let c = model::validate(c, &[8080, 8443]).unwrap();
    let (resolved, json) = e.svc.render(&c, &facts());
    assert_eq!(resolved.ui, vec!["nas.local", "nas", "nas.example.com"]);
    let immich = resolved
        .routes
        .iter()
        .find(|r| r.route.id == "app-immich")
        .unwrap();
    assert_eq!(
        immich.names,
        [
            "immich-nas.local",
            "immich.nas.example.com",
            "immich.files.example.org"
        ]
    );
    assert_eq!(
        resolved.local_names(),
        [
            "nas.local",
            "nas",
            "immich-nas.local",
            "portainer-nas.local"
        ]
    );
    assert_golden("web_access_caddy_domains.json", &json);
    let text = json.to_string();
    // secrets are placeholders, never values; acme-dns names its account literally
    assert!(!text.contains(CF_TOKEN) && !text.contains("adns-password-secret"));
    assert!(text.contains("{file./var/lib/rynas-caddy/rynas/dns/cf/api_token}"));
    assert!(text.contains("\"username\":\"user-uuid-1\""));
    // the plan shows the same, with any literal secret masked
    let plan = e.svc.plan(&c, &WebAccessConfig::default());
    assert!(!plan.engine_config.to_string().contains(CF_TOKEN));
    // external mode: no engine, no ACME
    let mut x = c.clone();
    x.settings.mode = ProxyMode::External;
    assert!(e.svc.render(&x, &facts()).1.is_null());
}

#[tokio::test]
async fn credentials_are_sealed_and_never_returned() {
    let e = env();
    let app = e.app();
    let (s, b) = call(
        &app,
        "admin",
        "PUT",
        "/v1/web-access",
        Some(serde_json::to_value(sample_w2()).unwrap()),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let text = String::from_utf8_lossy(&b).into_owned();
    assert!(!text.contains(CF_TOKEN), "{text}");
    assert!(!text.contains("\"sealed\""), "{text}");
    let st: WebAccessState = serde_json::from_slice(&b).unwrap();
    let p = st.pending.unwrap();
    assert_eq!(p.credentials[0].fields, ["api_token"]);
    assert!(p.credentials[0].secrets.is_empty());
    // sent back as GET gave it (no values): the stored ones stay
    let (s, b) = call(
        &app,
        "admin",
        "PUT",
        "/v1/web-access",
        Some(serde_json::to_value(&p).unwrap()),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    // a forged envelope is ignored, a new credential without values refused
    let mut forged = p.clone();
    forged.credentials[0].sealed = Some("Zm9yZ2Vk".into());
    forged.credentials.push(DnsCredential {
        id: "new".into(),
        provider: DnsProvider::Desec,
        ..Default::default()
    });
    let (s, b) = call(
        &app,
        "admin",
        "PUT",
        "/v1/web-access",
        Some(serde_json::to_value(&forged).unwrap()),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let err: ApiError = serde_json::from_slice(&b).unwrap();
    assert_eq!(err.field.as_deref(), Some("credentials[2].secrets"));

    // apply: the logs, config.db, the export and the answers never carry the token
    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    let l2 = logs.clone();
    let cfg = e.ops.config.clone();
    tokio::task::spawn_blocking(move || {
        cfg.apply(
            "admin",
            "admin",
            "test",
            &|l| l2.lock().unwrap().push(l.to_string()),
            &|| false,
        )
        .unwrap();
    })
    .await
    .unwrap();
    assert!(!logs.lock().unwrap().is_empty());
    assert!(logs.lock().unwrap().iter().all(|l| !l.contains(CF_TOKEN)));
    let db = std::fs::read(e.dir.path().join("state/config.db")).unwrap();
    assert!(
        !db.windows(CF_TOKEN.len()).any(|w| w == CF_TOKEN.as_bytes()),
        "config.db holds the token in clear"
    );
    let out = e.dir.path().join("x.rynasbak");
    rynasd::backup::export(e.config(), &e.dir.path().join("state"), &out, "nas", None).unwrap();
    let a = rynas_config::archive::read_archive(&out, None, e.dir.path()).unwrap();
    assert!(a.values.contains_key(WEB_ACCESS_KEY));
    assert!(!serde_json::to_string(&a.values).unwrap().contains(CF_TOKEN));
    // the engine reads it from its own file, the JSON names the file
    let f = e.file("/var/lib/rynas-caddy/rynas/dns/cf/api_token");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), CF_TOKEN);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert!(!e.caddy.config().to_string().contains(CF_TOKEN));
    let last_good = std::fs::read_to_string(e.file(rynasd::web_access::LAST_GOOD)).unwrap();
    assert!(!last_good.contains(CF_TOKEN));
    let (_, b) = call(&app, "admin", "GET", "/v1/web-access", None).await;
    assert!(!String::from_utf8_lossy(&b).contains(CF_TOKEN));
    let (_, b) = call(&app, "admin", "POST", "/v1/web-access/plan", None).await;
    assert!(!String::from_utf8_lossy(&b).contains(CF_TOKEN));
    // revisions never carry the envelope either (review 2026-11-07, L8)
    let sealed = serde_json::from_value::<WebAccessConfig>(
        e.config().applied(WEB_ACCESS_KEY).unwrap().unwrap(),
    )
    .unwrap()
    .credentials[0]
        .sealed
        .clone()
        .unwrap();
    let (_, b) = call(&app, "admin", "GET", "/v1/config/revisions", None).await;
    let revs: Vec<rynas_api_types::RevisionSummary> = serde_json::from_slice(&b).unwrap();
    let (s, b) = call(
        &app,
        "admin",
        "GET",
        &format!("/v1/config/revisions/{}", revs[0].id),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let text = String::from_utf8_lossy(&b);
    assert!(text.contains("credentials"), "{text}");
    assert!(!text.contains(&sealed), "{text}");
    // the provider client got the value
    assert!(
        e.dns
            .creds
            .lock()
            .unwrap()
            .iter()
            .any(|c| c["api_token"] == CF_TOKEN)
    );

    let (s, b) = call(&app, "admin", "GET", "/v1/web-access/providers", None).await;
    assert_eq!(s, StatusCode::OK);
    let prov: Vec<DnsProviderInfo> = serde_json::from_slice(&b).unwrap();
    assert_eq!(prov.len(), DnsProvider::ALL.len());
    assert!(
        prov.iter()
            .any(|p| p.provider == DnsProvider::Desec && p.records && p.min_ttl == 3600)
    );

    // a new value, same JSON: the engine is reloaded anyway; the domain removed: file gone
    let e = Arc::new(e);
    let e2 = e.clone();
    tokio::task::spawn_blocking(move || {
        let loads = e2.caddy.loads();
        let mut c = e2.applied();
        c.credentials[0].secrets =
            [("api_token".to_string(), "cf-second-token-987".to_string())].into();
        c.credentials[0].sealed = None;
        e2.apply(&c).unwrap();
        assert_eq!(e2.caddy.loads(), loads + 1);
        let f = e2.file("/var/lib/rynas-caddy/rynas/dns/cf/api_token");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "cf-second-token-987");
        let mut c = e2.applied();
        c.domains.clear();
        e2.apply(&c).unwrap();
        assert!(!f.exists());
    })
    .await
    .unwrap();
}

fn sets(e: &Env) -> Vec<String> {
    e.dns
        .sets
        .lock()
        .unwrap()
        .iter()
        .map(|((n, t), v)| format!("{n} {t} {}", v.join(",")))
        .collect()
}

#[test]
fn records_are_planned_written_followed_and_removed() {
    let e = env();
    let mut c = one_domain();
    // the plan reads, never writes
    e.svc.seal_input(&mut c, &[]).unwrap();
    let c = model::validate(c, &[8080, 8443]).unwrap();
    let plan = e.svc.plan(&c, &WebAccessConfig::default());
    let acts: Vec<String> = plan
        .dns_changes
        .iter()
        .map(|x| {
            format!(
                "{:?} {} {} {}",
                x.action,
                x.name,
                x.rtype,
                x.wanted.join(",")
            )
        })
        .collect();
    assert_eq!(
        acts,
        [
            "Create *.nas.example.com A 192.0.2.10",
            "Create nas.example.com A 192.0.2.10"
        ]
    );
    assert!(e.dns.calls.lock().unwrap().is_empty());
    // the apply writes them and remembers them
    e.apply(&c).unwrap();
    assert_eq!(
        sets(&e),
        [
            "*.nas.example.com A 192.0.2.10",
            "nas.example.com A 192.0.2.10"
        ]
    );
    let owned =
        std::fs::read_to_string(e.dir.path().join("state/web-access/dns-records.json")).unwrap();
    assert!(owned.contains("*.nas.example.com"));
    let st = e.svc.state("admin").unwrap();
    let d = &st.domains[0];
    assert!(d.records.as_ref().unwrap().ok);
    assert_eq!(
        d.certificate_names,
        ["*.nas.example.com", "nas.example.com"]
    );
    assert!(d.problems.is_empty(), "{:?}", d.problems);
    assert_eq!(d.ui_urls, ["https://nas.example.com/"]);
    assert!(st.ui_names.contains(&"nas.example.com".to_string()));
    let immich = st.routes.iter().find(|r| r.id == "app-immich").unwrap();
    assert!(
        immich
            .urls
            .contains(&"https://immich.nas.example.com/".to_string())
    );
    // the app's Open button prefers the trusted name
    assert_eq!(
        e.svc.stack_url("immich").as_deref(),
        Some("https://immich.nas.example.com/")
    );

    // the LAN address changes: the next check writes the new one
    e.svc.sync(&|_| {}).unwrap();
    e.dns.calls.lock().unwrap().clear();
    {
        let mut f = e.facts.lock().unwrap();
        f.primary_v4 = Some("192.0.2.20".parse().unwrap());
        f.addrs = ["192.0.2.20".parse().unwrap()].into();
    }
    e.svc.sync(&|_| {}).unwrap();
    assert_eq!(
        sets(&e),
        [
            "*.nas.example.com A 192.0.2.20",
            "nas.example.com A 192.0.2.20"
        ]
    );

    // IPv6 on: AAAA too
    e.facts.lock().unwrap().primary_v6 = Some("fd00::20".parse().unwrap());
    let mut c = e.applied();
    c.domains[0].lan_ipv6 = true;
    e.apply(&c).unwrap();
    assert_eq!(sets(&e).len(), 4);

    // a record set rynas did not write is never deleted
    e.dns.sets.lock().unwrap().insert(
        ("mail.example.com".into(), Rtype::A),
        vec!["198.51.100.1".into()],
    );
    // the domain removed: exactly the sets rynas wrote go
    let mut c = e.applied();
    c.domains.clear();
    let plan = e.svc.plan(&c, &e.applied());
    assert_eq!(plan.dns_changes.len(), 4, "{:?}", plan.dns_changes);
    assert!(
        plan.dns_changes
            .iter()
            .all(|x| x.action == DnsAction::Delete)
    );
    e.apply(&c).unwrap();
    assert_eq!(sets(&e), ["mail.example.com A 198.51.100.1"]);
}

/// Review 2026-11-07 H2: a record set rynas did not write is a `conflict` (never
/// overwritten, adopted or deleted) until it is taken over; one edited after rynas wrote it
/// is not deleted either.
#[test]
fn foreign_records_are_never_overwritten_or_deleted() {
    let e = env();
    // the administrator's DDNS record on the name the domain wants
    e.dns.sets.lock().unwrap().insert(
        ("nas.example.com".into(), Rtype::A),
        vec!["203.0.113.5".into()],
    );
    let mut c = one_domain();
    e.svc.seal_input(&mut c, &[]).unwrap();
    let c = model::validate(c, &[8080, 8443]).unwrap();
    let plan = e.svc.plan(&c, &WebAccessConfig::default());
    let conflict = plan
        .dns_changes
        .iter()
        .find(|x| x.name == "nas.example.com")
        .unwrap();
    assert_eq!(conflict.action, DnsAction::Conflict);
    assert_eq!(conflict.current, ["203.0.113.5"]);
    e.apply(&c).unwrap();
    assert_eq!(
        sets(&e),
        [
            "*.nas.example.com A 192.0.2.10",
            "nas.example.com A 203.0.113.5"
        ],
        "the conflict is left alone"
    );
    let st = e.svc.state("admin").unwrap();
    assert!(
        st.domains[0]
            .problems
            .contains(&"records_conflict".to_string()),
        "{:?}",
        st.domains[0].problems
    );
    // the wildcard rynas wrote is then edited by hand
    e.dns.sets.lock().unwrap().insert(
        ("*.nas.example.com".into(), Rtype::A),
        vec!["198.51.100.9".into()],
    );
    // the domain removed: nothing rynas did not write (or that changed since) is deleted
    let mut gone = e.applied();
    gone.domains.clear();
    e.apply(&gone).unwrap();
    assert!(e.dns.calls.lock().unwrap().iter().all(|c| !c.starts_with("delete")));
    assert_eq!(
        sets(&e),
        [
            "*.nas.example.com A 198.51.100.9",
            "nas.example.com A 203.0.113.5"
        ]
    );
    let owned =
        std::fs::read_to_string(e.dir.path().join("state/web-access/dns-records.json")).unwrap();
    assert!(!owned.contains("nas.example.com"), "ownership dropped: {owned}");

    // consent: the name taken over is replaced, then owned (and deleted with the domain)
    let mut c = one_domain();
    c.domains[0].take_over = vec!["NAS.example.com".into()];
    e.apply(&c).unwrap();
    assert!(sets(&e).contains(&"nas.example.com A 192.0.2.10".to_string()));
    let mut gone = e.applied();
    assert_eq!(gone.domains[0].take_over, ["nas.example.com"]);
    gone.domains.clear();
    e.apply(&gone).unwrap();
    assert!(!sets(&e).iter().any(|s| s.starts_with("nas.example.com")));
    // a take-over outside the domain is refused
    let mut bad = one_domain();
    bad.domains[0].take_over = vec!["mail.example.com".into()];
    assert_eq!(
        model::validate(bad, &[8080, 8443])
            .unwrap_err()
            .field
            .as_deref(),
        Some("domains[0].take_over")
    );
}

/// Review 2026-11-07 H1: a `rynas/` planted by the engine's user (here a symlink out of its
/// folder) is moved aside and replaced; nothing is written through it.
#[test]
fn planted_rynas_folder_is_moved_aside() {
    let e = env();
    let victim = e.dir.path().join("victim");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("keep"), "x").unwrap();
    let state = e.file("/var/lib/rynas-caddy");
    std::fs::create_dir_all(&state).unwrap();
    std::os::unix::fs::symlink(&victim, state.join("rynas")).unwrap();
    e.apply(&one_domain()).unwrap();
    let meta = std::fs::symlink_metadata(state.join("rynas")).unwrap();
    assert!(meta.is_dir() && !meta.is_symlink());
    assert_eq!(
        std::fs::read_to_string(state.join("rynas/dns/cf/api_token")).unwrap(),
        CF_TOKEN
    );
    let names: Vec<String> = std::fs::read_dir(&victim)
        .unwrap()
        .map(|d| d.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["keep"], "nothing written through the link");
    assert!(
        std::fs::read_dir(&state)
            .unwrap()
            .any(|d| d.unwrap().file_name().to_string_lossy().starts_with("rynas.untrusted-"))
    );
}

/// Review 2026-11-07 M1: the pages answer while a record sync waits on a slow provider.
#[tokio::test]
async fn pages_answer_during_a_slow_record_sync() {
    let e = Arc::new(env());
    let app = e.app();
    let e2 = e.clone();
    tokio::task::spawn_blocking(move || e2.apply(&one_domain()).unwrap())
        .await
        .unwrap();
    *e.dns.slow.lock().unwrap() = Some(Duration::from_secs(3));
    let e3 = e.clone();
    let sync = tokio::task::spawn_blocking(move || {
        let c = e3.applied();
        e3.svc
            .sync_records(&c, &c, &facts(), Some("home"), &|_| {})
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = std::time::Instant::now();
    let (s, _) = call(&app, "admin", "GET", "/v1/web-access", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "GET waited {:?}",
        t.elapsed()
    );
    *e.dns.slow.lock().unwrap() = None;
    assert!(sync.await.unwrap()[0].ok);
}

#[test]
fn record_failures_never_fail_the_apply() {
    let e = env();
    *e.dns.fail.lock().unwrap() = true;
    e.apply(&one_domain()).unwrap();
    let st = e.svc.state("admin").unwrap();
    assert!(
        st.domains[0]
            .problems
            .contains(&"records_failed".to_string())
    );
    let r = st.domains[0].records.as_ref().unwrap();
    assert!(!r.ok);
    assert!(
        r.changes[0].detail.contains("rate limited"),
        "{:?}",
        r.changes
    );
    // fixed: "Write records" now
    *e.dns.fail.lock().unwrap() = false;
    let r = e
        .svc
        .sync_records(&e.applied(), &e.applied(), &facts(), Some("home"), &|_| {});
    assert!(r[0].ok);
    assert!(e.svc.state("admin").unwrap().domains[0].problems.is_empty());
    // a delegated domain: the records are listed for the administrator, nothing written
    let mut c = sample_w2();
    c.domains.remove(0);
    e.svc.seal_input(&mut c, &[]).unwrap();
    let c = model::validate(c, &[8080, 8443]).unwrap();
    e.dns.calls.lock().unwrap().clear();
    let plan = e.svc.plan(&c, &e.applied());
    let manual: Vec<String> = plan
        .dns_changes
        .iter()
        .filter(|x| x.action == DnsAction::Manual)
        .map(|x| format!("{} {} {}", x.name, x.rtype, x.wanted.join(",")))
        .collect();
    assert_eq!(
        manual,
        [
            "*.files.example.org A 192.0.2.10",
            "files.example.org A 192.0.2.10",
            "_acme-challenge.files.example.org CNAME sub-uuid-1.auth.acme-dns.example.net"
        ]
    );
    assert!(e.dns.calls.lock().unwrap().is_empty());
}

#[test]
fn domain_checklist_tells_rebind_protection_apart() {
    let e = env();
    let lan_ip: IpAddr = "192.168.1.10".parse().unwrap();
    {
        let mut f = e.facts.lock().unwrap();
        f.primary_v4 = Some("192.168.1.10".parse().unwrap());
        f.addrs = [lan_ip].into();
        f.lan = [Cidr::parse("192.168.1.0/24").unwrap()].into();
    }
    *e.probe.resolve.lock().unwrap() = Err("NXDOMAIN".into());
    let router: SocketAddr = "192.168.1.1:53".parse().unwrap();
    *e.probe.lan.lock().unwrap() = vec![router];
    {
        let mut a = e.probe.answers.lock().unwrap();
        a.insert("1.1.1.1:53".parse().unwrap(), Answer::Addrs(vec![lan_ip]));
        a.insert(router, Answer::NxDomain);
    }
    e.apply(&one_domain()).unwrap();
    let get =
        |d: &RouteDiagnostics, id: &str| d.checks.iter().find(|c| c.id == id).cloned().unwrap();
    let d = e.svc.diagnose_domain("home").unwrap();
    let ids: Vec<&str> = d.checks.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "dns_record",
            "dns_nas",
            "dns_public",
            "rebind",
            "certificate",
            "renewal",
            "hairpin"
        ]
    );
    assert_eq!(get(&d, "dns_record").state, CheckState::Ok);
    assert_eq!(get(&d, "dns_public").state, CheckState::Ok);
    assert_eq!(get(&d, "dns_nas").state, CheckState::Fail);
    let rb = get(&d, "rebind");
    assert_eq!(rb.state, CheckState::Fail);
    assert!(
        rb.detail.contains("rebind") && rb.detail.contains("nas.example.com"),
        "{}",
        rb.detail
    );
    assert_eq!(
        get(&d, "certificate").state,
        CheckState::Warn,
        "being obtained"
    );
    assert_eq!(get(&d, "hairpin").state, CheckState::Ok);
    // the router allows the domain: green
    e.probe
        .answers
        .lock()
        .unwrap()
        .insert(router, Answer::Addrs(vec![lan_ip]));
    *e.probe.resolve.lock().unwrap() = Ok(vec![lan_ip]);
    let d = e.svc.diagnose_domain("home").unwrap();
    assert_eq!(get(&d, "rebind").state, CheckState::Ok);
    assert_eq!(get(&d, "dns_nas").state, CheckState::Ok);
    // public DNS does not have it yet
    e.probe
        .answers
        .lock()
        .unwrap()
        .insert("1.1.1.1:53".parse().unwrap(), Answer::NxDomain);
    let d = e.svc.diagnose_domain("home").unwrap();
    assert_eq!(get(&d, "dns_public").state, CheckState::Fail);
    assert_eq!(get(&d, "rebind").state, CheckState::Skipped);
    assert!(e.svc.diagnose_domain("nope").is_err());
    // the route test adds the domain name's checks
    let r = e.svc.diagnose("app-immich").unwrap();
    assert!(
        r.checks
            .iter()
            .any(|c| c.id == "domain_dns" && c.state == CheckState::Ok)
    );
    assert!(
        r.checks
            .iter()
            .any(|c| c.id == "domain_certificate" && c.state == CheckState::Warn)
    );
}

fn write_acme(e: &Env, subject: &str, now: i64) {
    let ca = rynas_tls::generate_ca("le", now).unwrap();
    let leaf = rynas_tls::issue_leaf(
        &ca,
        &rynas_tls::Names {
            dns: [subject.to_string()].into(),
            ips: Default::default(),
        },
        now,
    )
    .unwrap();
    let p = e.file(&rynasd::web_access::domains::cert_path(
        "/var/lib/rynas-caddy/data",
        rynasd::web_access::domains::ACME_PRODUCTION,
        subject,
    ));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, leaf.chain).unwrap();
}

#[test]
fn certificate_inventory_and_notifications() {
    let e = env();
    e.apply(&one_domain()).unwrap();
    // an expired wildcard (issued long ago), the apex not obtained yet with an engine error
    write_acme(
        &e,
        "*.nas.example.com",
        rynas_tls::now() - 4000 * 86_400_000,
    );
    *e.engine_log.lock().unwrap() = r#"{"level":"error","logger":"tls.obtain","msg":"could not get certificate from issuer","identifier":"nas.example.com","error":"cloudflare: invalid token"}"#.into();
    let inv = e.svc.certificates();
    let by = |id: &str| {
        inv.certificates
            .iter()
            .find(|x| x.id == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id}: {:?}", inv.certificates))
    };
    assert_eq!(by("proxy-local").kind, CertKind::ProxyLocal);
    let w = by("acme:*.nas.example.com");
    assert_eq!((w.kind, w.status), (CertKind::Acme, CertStatus::Expired));
    assert_eq!(w.domain.as_deref(), Some("nas.example.com"));
    assert!(w.renew_after_ms.is_some());
    let apex = by("acme:nas.example.com");
    assert_eq!(apex.status, CertStatus::Pending);
    assert_eq!(
        apex.last_error.as_deref(),
        Some("could not get certificate from issuer: cloudflare: invalid token")
    );
    // notifications: firing once, nothing on the next check, resolved when renewed
    let ev = e.svc.check_certificates(true);
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].code, "web_access.cert_expired");
    assert_eq!(e.notes.take().len(), 1);
    assert!(e.svc.check_certificates(true).is_empty());
    assert!(e.svc.check_certificates(false).is_empty(), "rate limited");
    write_acme(&e, "*.nas.example.com", rynas_tls::now());
    let ev = e.svc.check_certificates(true);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].code, "web_access.cert_ok");
    assert_eq!(ev[0].state, rynas_api_types::notify::EventState::Resolved);
    // the HTTP routes
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let app = e.app();
    let (s, b) = rt.block_on(call(
        &app,
        "admin",
        "GET",
        "/v1/web-access/certificates",
        None,
    ));
    assert_eq!(s, StatusCode::OK);
    let inv: CertificateInventory = serde_json::from_slice(&b).unwrap();
    assert!(
        inv.certificates
            .iter()
            .any(|x| x.id == "acme:*.nas.example.com" && x.status != CertStatus::Expired)
    );
    let (s, _) = rt.block_on(call(
        &app,
        "bob",
        "GET",
        "/v1/web-access/certificates",
        None,
    ));
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = rt.block_on(call(
        &app,
        "admin",
        "POST",
        "/v1/web-access/domains/home/records",
        None,
    ));
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let r: DnsSyncReport = serde_json::from_slice(&b).unwrap();
    assert!(r.ok && r.changes.iter().all(|x| x.action == DnsAction::Keep));
    let (s, _) = rt.block_on(call(
        &app,
        "admin",
        "POST",
        "/v1/web-access/domains/nope/records",
        None,
    ));
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, b) = rt.block_on(call(
        &app,
        "admin",
        "POST",
        "/v1/web-access/domains/home/test",
        None,
    ));
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
}

// ---------------------------------------------------------------------------------------------
// imported certificates (§5.5)
// ---------------------------------------------------------------------------------------------

/// A leaf for `names` from a throw-away CA ("made elsewhere"), PEM chain and key.
fn foreign_pair(names: &[&str], now: i64) -> rynas_tls::PemPair {
    let ca = rynas_tls::generate_ca("corp", now).unwrap();
    rynas_tls::issue_leaf(
        &ca,
        &rynas_tls::Names {
            dns: names.iter().map(|s| s.to_string()).collect(),
            ips: Default::default(),
        },
        now,
    )
    .unwrap()
}

fn upload(id: &str, p: &rynas_tls::PemPair) -> CertImportRequest {
    CertImportRequest {
        id: id.into(),
        label: "router wildcard".into(),
        source: ImportSource::Upload,
        certificate: Some(p.chain.clone()),
        private_key: Some(p.key.clone()),
        names: Vec::new(),
    }
}

fn https_tls(json: &Value) -> (Value, Value) {
    (
        json["apps"]["http"]["servers"]["rynas_https"]["tls_connection_policies"].clone(),
        json["apps"]["tls"].clone(),
    )
}

fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

#[test]
fn imported_certificate_takes_over_the_names_it_covers() {
    let e = env();
    e.apply(&one_domain()).unwrap();
    let loads = e.caddy.loads();
    let now = rynas_tls::now();
    let pair = foreign_pair(&["*.nas.example.com", "nas.example.com"], now);
    let v = e.svc.save_import(&upload("wild", &pair)).unwrap();
    assert_eq!(v.status, CertStatus::Ok);
    let mut covers = v.covers.clone();
    covers.sort();
    assert_eq!(
        covers,
        [
            "immich.nas.example.com",
            "nas.example.com",
            "portainer.nas.example.com"
        ]
    );
    assert_eq!(v.replaces_acme, ["*.nas.example.com", "nas.example.com"]);
    assert!(v.shadowed.is_empty());
    // loaded again, re-reading the files
    assert_eq!(e.caddy.loads(), loads + 1);
    assert!(e.caddy.state.lock().unwrap().loads.last().unwrap().1);
    let running = e.caddy.config();
    let (policies, tls) = https_tls(&running);
    assert_eq!(policies.as_array().unwrap().len(), 2, "{policies}");
    let mut sni: Vec<String> =
        serde_json::from_value(policies[0]["match"]["sni"].clone()).unwrap();
    sni.sort();
    assert_eq!(sni, covers);
    assert_eq!(
        policies[0]["certificate_selection"],
        json!({"any_tag": ["rynas-import-wild"]})
    );
    assert_eq!(policies[1], json!({}));
    let files = tls["certificates"]["load_files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "{files:?}");
    assert_eq!(
        files[1],
        json!({"certificate": "/var/lib/rynas-caddy/rynas/import-wild.crt",
               "key": "/var/lib/rynas-caddy/rynas/import-wild.key",
               "tags": ["rynas-import-wild"]})
    );
    // the ACME subjects it replaces are not ordered
    assert!(tls["certificates"].get("automate").is_none(), "{tls}");
    assert!(tls.get("automation").is_none(), "{tls}");
    // the engine's copies: 0640, the key as uploaded; the private store 0600
    let crt = e.file("/var/lib/rynas-caddy/rynas/import-wild.crt");
    let key = e.file("/var/lib/rynas-caddy/rynas/import-wild.key");
    assert_eq!(
        std::fs::read_to_string(&key).unwrap().trim(),
        pair.key.trim()
    );
    assert_eq!((mode_of(&crt), mode_of(&key)), (0o640, 0o640));
    let store = e.dir.path().join("state/web-access/imported");
    assert_eq!(mode_of(&store), 0o700);
    assert_eq!(mode_of(&store.join("wild.key")), 0o600);
    // the dashboard: the import, no ACME entries for the replaced subjects
    let inv = e.svc.certificates();
    let imp = inv
        .certificates
        .iter()
        .find(|c| c.id == "imported:wild")
        .unwrap();
    assert_eq!((imp.kind, imp.status), (CertKind::Imported, CertStatus::Ok));
    assert_eq!(imp.source.as_deref(), Some("upload"));
    assert!(
        !inv.certificates.iter().any(|c| c.kind == CertKind::Acme),
        "{:?}",
        inv.certificates
    );
    // the same pair again: nothing changes, nothing reloads
    e.svc.save_import(&upload("wild", &pair)).unwrap();
    assert_eq!(e.caddy.loads(), loads + 1);
    // removed: ACME again, copies gone
    e.svc.delete_import("wild").unwrap();
    let (policies, tls) = https_tls(&e.caddy.config());
    assert_eq!(policies, json!([{}]));
    assert_eq!(
        tls["certificates"]["automate"],
        json!(["*.nas.example.com", "nas.example.com"])
    );
    assert!(!crt.exists() && !key.exists());
    assert!(!store.join("wild.key").exists());
    assert!(e.svc.imports().is_empty());
}

#[test]
fn exact_import_beats_a_wildcard_and_local_names_stay_local() {
    let e = env();
    e.apply(&one_domain()).unwrap();
    let now = rynas_tls::now();
    e.svc
        .save_import(&upload(
            "wild",
            &foreign_pair(&["*.nas.example.com", "nas.example.com"], now),
        ))
        .unwrap();
    e.svc
        .save_import(&upload(
            "immich",
            &foreign_pair(&["immich.nas.example.com"], now),
        ))
        .unwrap();
    let all = e.svc.imports();
    let wild = all.iter().find(|v| v.id == "wild").unwrap();
    assert_eq!(wild.shadowed, ["immich.nas.example.com"]);
    let (policies, tls) = https_tls(&e.caddy.config());
    assert_eq!(
        policies[0],
        json!({"match": {"sni": ["immich.nas.example.com"]},
               "certificate_selection": {"any_tag": ["rynas-import-immich"]}})
    );
    assert!(
        !policies[1]["match"]["sni"]
            .to_string()
            .contains("immich.nas.example.com")
    );
    // the local names keep the local leaf (no policy, served by the catch-all)
    assert_eq!(
        tls["certificates"]["load_files"][0]["tags"],
        json!(["rynas-local"])
    );
    assert!(!policies.to_string().contains(".local"));
    // a certificate that covers no served name is stored but not loaded
    e.svc
        .save_import(&upload("other", &foreign_pair(&["files.example.org"], now)))
        .unwrap();
    let (_, tls) = https_tls(&e.caddy.config());
    assert!(!tls.to_string().contains("import-other"));
    let other = e
        .svc
        .imports()
        .into_iter()
        .find(|v| v.id == "other")
        .unwrap();
    assert!(other.covers.is_empty() && other.status == CertStatus::Ok);
}

#[test]
fn expired_import_is_not_served_and_notifies() {
    let e = env();
    e.apply(&one_domain()).unwrap();
    // an import that expired since (written to the store as it would have been)
    let old = foreign_pair(
        &["*.nas.example.com", "nas.example.com"],
        rynas_tls::now() - 4000 * 86_400_000,
    );
    let store = e.dir.path().join("state/web-access/imported");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("old.crt"), &old.chain).unwrap();
    std::fs::write(store.join("old.key"), &old.key).unwrap();
    std::fs::write(
        store.join("index.json"),
        r#"[{"id":"old","label":"bought wildcard","source":"upload"}]"#,
    )
    .unwrap();
    let (_, json) = e.svc.render(&e.applied(), &facts());
    let (policies, tls) = https_tls(&json);
    assert_eq!(policies, json!([{}]));
    assert_eq!(
        tls["certificates"]["automate"],
        json!(["*.nas.example.com", "nas.example.com"])
    );
    let v = e.svc.imports().pop().unwrap();
    assert_eq!(v.status, CertStatus::Expired);
    assert!(v.covers.is_empty());
    let ev = e.svc.check_certificates(true);
    let mine: Vec<_> = ev
        .iter()
        .filter(|x| x.code == "web_access.cert_expired" && x.title.contains("imported"))
        .collect();
    assert_eq!(mine.len(), 1, "{ev:?}");
}

#[tokio::test]
async fn import_routes_validate_and_never_return_the_key() {
    let e = env();
    e.apply(&one_domain()).unwrap();
    let app = e.app();
    let now = rynas_tls::now();
    let pair = foreign_pair(&["nas.example.com"], now);
    let other = foreign_pair(&["nas.example.com"], now);
    let post = |b: &CertImportRequest| Some(serde_json::to_value(b).unwrap());
    // a key that does not match the certificate
    let mut bad = upload("apex", &pair);
    bad.private_key = Some(other.key.clone());
    let (s, b) = call(&app, "admin", "POST", "/v1/web-access/imports", post(&bad)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let err: ApiError = serde_json::from_slice(&b).unwrap();
    assert_eq!(err.field.as_deref(), Some("private_key"), "{err:?}");
    // a bad id; a missing key for an upload
    let mut bad = upload("Bad_Id", &pair);
    let (s, b) = call(&app, "admin", "POST", "/v1/web-access/imports", post(&bad)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_slice::<ApiError>(&b)
            .unwrap()
            .field
            .as_deref(),
        Some("id")
    );
    bad.id = "apex".into();
    bad.private_key = None;
    let (s, _) = call(&app, "admin", "POST", "/v1/web-access/imports", post(&bad)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // inspect: coverage, nothing stored
    let good = upload("apex", &pair);
    let (s, b) = call(
        &app,
        "admin",
        "POST",
        "/v1/web-access/imports/inspect",
        post(&good),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let v: ImportedCert = serde_json::from_slice(&b).unwrap();
    assert_eq!(v.covers, ["nas.example.com"]);
    assert_eq!(v.replaces_acme, ["nas.example.com"]);
    assert!(e.svc.imports().is_empty());
    // the real import; answers never carry key material
    let (s, b) = call(&app, "admin", "POST", "/v1/web-access/imports", post(&good)).await;
    assert_eq!(s, StatusCode::OK);
    let key_line = pair
        .key
        .lines()
        .find(|l| !l.starts_with("-----"))
        .unwrap()
        .to_string();
    let text = String::from_utf8_lossy(&b).into_owned();
    assert!(!text.contains(&key_line) && !text.contains("PRIVATE KEY"));
    let (s, b) = call(&app, "admin", "GET", "/v1/web-access/imports", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!String::from_utf8_lossy(&b).contains("PRIVATE KEY"));
    let list: Vec<ImportedCert> = serde_json::from_slice(&b).unwrap();
    assert_eq!(list.len(), 1);
    // the wildcard ACME subject stays, the apex is replaced
    let (_, tls) = https_tls(&e.caddy.config());
    assert_eq!(
        tls["certificates"]["automate"],
        json!(["*.nas.example.com"])
    );
    // administrators only, reads included
    let (s, _) = call(&app, "bob", "GET", "/v1/web-access/imports", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // check needs a watched folder; unknown ids are 404
    let (s, _) = call(
        &app,
        "admin",
        "POST",
        "/v1/web-access/imports/apex/check",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(&app, "admin", "DELETE", "/v1/web-access/imports/nope", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&app, "admin", "DELETE", "/v1/web-access/imports/apex", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

#[test]
fn watched_folder_settles_validates_and_keeps_the_last_good_pair() {
    use rynasd::web_access::imported::{DROP_DIR, FULLCHAIN, PRIVKEY, SETTLE};
    use std::time::Instant;
    let e = env();
    e.apply(&one_domain()).unwrap();
    let v = e
        .svc
        .save_import(&CertImportRequest {
            id: "router".into(),
            label: "router acme.sh".into(),
            source: ImportSource::Folder,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(v.status, CertStatus::Pending);
    assert_eq!(v.folder.as_deref(), Some("/var/lib/rynas-cert-drop/router"));
    let base = e.file(DROP_DIR);
    let dir = base.join("router");
    assert_eq!((mode_of(&base), mode_of(&dir)), (0o750, 0o2770));
    let inv = e.svc.certificates();
    let entry = inv
        .certificates
        .iter()
        .find(|c| c.id == "imported:router")
        .unwrap();
    assert_eq!(
        (entry.status, entry.source.as_deref()),
        (CertStatus::Pending, Some("folder"))
    );

    let loads = e.caddy.loads();
    let now = rynas_tls::now();
    let good = foreign_pair(&["*.nas.example.com", "nas.example.com"], now);
    // the tool writes the key, then the chain (acme.sh's ssh deploy: in place, one by one)
    std::fs::write(dir.join(PRIVKEY), &good.key).unwrap();
    let t0 = Instant::now();
    assert!(e.svc.scan_drops_at(t0).is_empty(), "first sight");
    std::fs::write(dir.join(FULLCHAIN), &good.chain).unwrap();
    assert!(e.svc.scan_drops_at(t0 + SETTLE).is_empty(), "changed again");
    let t1 = t0 + SETTLE + SETTLE / 2;
    assert!(e.svc.scan_drops_at(t1).is_empty(), "not settled yet");
    assert_eq!(e.svc.scan_drops_at(t1 + SETTLE), ["router"]);
    assert_eq!(e.caddy.loads(), loads + 1);
    let (policies, _) = https_tls(&e.caddy.config());
    assert_eq!(
        policies[0]["certificate_selection"]["any_tag"],
        json!(["rynas-import-router"])
    );
    assert!(e.svc.scan_drops_at(t1 + SETTLE * 2).is_empty(), "done once");
    assert!(e.notes.take().is_empty());

    // a key of another certificate: refused, the last good pair stays, one notification
    let other = foreign_pair(&["nas.example.com"], now);
    std::fs::write(dir.join(PRIVKEY), &other.key).unwrap();
    let t2 = t1 + SETTLE * 10;
    e.svc.scan_drops_at(t2);
    // a key of another chain may be a writer between the two files: confirmed a window later
    assert!(e.svc.scan_drops_at(t2 + SETTLE).is_empty());
    assert!(e.notes.take().is_empty(), "not refused on first sight");
    assert!(e.svc.imports().pop().unwrap().last_error.is_none());
    assert!(e.svc.scan_drops_at(t2 + SETTLE * 2).is_empty());
    assert_eq!(e.caddy.loads(), loads + 1);
    let v = e.svc.imports().pop().unwrap();
    assert!(
        v.last_error.as_deref().unwrap().starts_with("privkey.pem: "),
        "{:?}",
        v.last_error
    );
    assert_eq!(v.status, CertStatus::Ok);
    assert_eq!(
        v.certificate.unwrap().dns_names,
        ["*.nas.example.com", "nas.example.com"]
    );
    let ev = e.notes.take();
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].code, "web_access.cert_import_failed");
    assert!(!format!("{ev:?}").contains("PRIVATE KEY"));
    // a symlink planted instead of the chain is never followed
    std::fs::remove_file(dir.join(FULLCHAIN)).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", dir.join(FULLCHAIN)).unwrap();
    std::fs::write(dir.join(PRIVKEY), &good.key).unwrap();
    let t3 = t2 + SETTLE * 10;
    e.svc.scan_drops_at(t3);
    assert!(e.svc.scan_drops_at(t3 + SETTLE).is_empty());
    let v = e.svc.imports().pop().unwrap();
    assert!(
        v.last_error.as_deref().unwrap().starts_with("fullchain.pem"),
        "{:?}",
        v.last_error
    );
    assert_eq!(e.notes.take().len(), 1);
    // good files again: taken (same pair as stored: no reload), the trouble resolves
    std::fs::remove_file(dir.join(FULLCHAIN)).unwrap();
    std::fs::write(dir.join(FULLCHAIN), &good.chain).unwrap();
    let v = e.svc.check_import("router").unwrap();
    assert!(v.last_error.is_none());
    let ev = e.notes.take();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].code, "web_access.cert_import_ok");
    assert_eq!(e.caddy.loads(), loads + 1);
    // a renewed pair through "Check now"
    let renewed = foreign_pair(&["*.nas.example.com", "nas.example.com"], now);
    std::fs::write(dir.join(PRIVKEY), &renewed.key).unwrap();
    std::fs::write(dir.join(FULLCHAIN), &renewed.chain).unwrap();
    e.svc.check_import("router").unwrap();
    assert_eq!(e.caddy.loads(), loads + 2);
    // removing the import removes its folder
    e.svc.delete_import("router").unwrap();
    assert!(!dir.exists() && base.exists());
}

// review 2026-11-07 of the imports (M1-M3, L1-L7)

fn folder_import(id: &str, names: &[&str]) -> CertImportRequest {
    CertImportRequest {
        id: id.into(),
        label: "router acme.sh".into(),
        source: ImportSource::Folder,
        names: names.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

/// Writes a pair into a watched folder (the key first, like acme.sh).
fn push_pair(dir: &Path, p: &rynas_tls::PemPair) {
    use rynasd::web_access::imported::{FULLCHAIN, PRIVKEY};
    for (n, data) in [(PRIVKEY, &p.key), (FULLCHAIN, &p.chain)] {
        let _ = std::fs::remove_file(dir.join(n));
        std::fs::write(dir.join(n), data).unwrap();
    }
}

fn wildcard_pair(now: i64) -> rynas_tls::PemPair {
    foreign_pair(&["*.nas.example.com", "nas.example.com"], now)
}

#[test]
fn watched_folder_refuses_fifos_oversize_files_and_hard_links() {
    use rynasd::web_access::imported::{DROP_DIR, FULLCHAIN, PRIVKEY};
    let e = env();
    e.apply(&one_domain()).unwrap();
    e.svc.save_import(&folder_import("router", &[])).unwrap();
    let dir = e.file(DROP_DIR).join("router");
    let good = wildcard_pair(rynas_tls::now());
    std::fs::write(dir.join(PRIVKEY), &good.key).unwrap();
    // a FIFO: refused at once (opened non-blocking, never read)
    let st = std::process::Command::new("mkfifo")
        .arg(dir.join(FULLCHAIN))
        .status()
        .unwrap();
    assert!(st.success());
    let v = e.svc.check_import("router").unwrap();
    assert_eq!(v.problem, Some(ImportProblem::Refused));
    assert_eq!(
        v.last_error.as_deref(),
        Some("fullchain.pem is not a regular file")
    );
    // over 64 KiB
    std::fs::remove_file(dir.join(FULLCHAIN)).unwrap();
    std::fs::write(dir.join(FULLCHAIN), "x".repeat(70 * 1024)).unwrap();
    let v = e.svc.check_import("router").unwrap();
    assert!(
        v.last_error
            .as_deref()
            .unwrap()
            .contains("larger than 64 KiB"),
        "{:?}",
        v.last_error
    );
    // a hard link (a group member handing over a file it may not read)
    std::fs::remove_file(dir.join(FULLCHAIN)).unwrap();
    let elsewhere = e.dir.path().join("chain-elsewhere.pem");
    std::fs::write(&elsewhere, &good.chain).unwrap();
    std::fs::hard_link(&elsewhere, dir.join(FULLCHAIN)).unwrap();
    let v = e.svc.check_import("router").unwrap();
    assert!(
        v.last_error.as_deref().unwrap().contains("hard links"),
        "{:?}",
        v.last_error
    );
    assert_eq!(v.status, CertStatus::Pending, "nothing stored");
    let ev = e.notes.take();
    assert_eq!(ev.len(), 3, "one per distinct refusal: {ev:?}");
    // a file of its own: taken, the trouble resolves
    std::fs::remove_file(dir.join(FULLCHAIN)).unwrap();
    std::fs::write(dir.join(FULLCHAIN), &good.chain).unwrap();
    let v = e.svc.check_import("router").unwrap();
    assert_eq!((v.status, v.problem), (CertStatus::Ok, None));
    let ev = e.notes.take();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].code, "web_access.cert_import_ok");
}

#[test]
fn watched_folder_retries_after_a_store_error() {
    use rynasd::web_access::imported::{DROP_DIR, SETTLE};
    use std::time::Instant;
    let e = env();
    e.apply(&one_domain()).unwrap();
    e.svc.save_import(&folder_import("router", &[])).unwrap();
    let loads = e.caddy.loads();
    let dir = e.file(DROP_DIR).join("router");
    // the store cannot take the key (a folder in its place)
    let blocker = e.dir.path().join("state/web-access/imported/router.key");
    std::fs::create_dir_all(blocker.join("x")).unwrap();
    push_pair(&dir, &wildcard_pair(rynas_tls::now()));
    let t0 = Instant::now();
    assert!(e.svc.scan_drops_at(t0).is_empty());
    assert!(e.svc.scan_drops_at(t0 + SETTLE).is_empty(), "store error");
    assert_eq!(e.caddy.loads(), loads);
    assert!(e.notes.take().is_empty(), "an I/O error is not a refusal");
    // fixed: the same files are read again after another window (not marked done)
    std::fs::remove_dir_all(&blocker).unwrap();
    assert!(e.svc.scan_drops_at(t0 + SETTLE + SETTLE / 2).is_empty());
    assert_eq!(e.svc.scan_drops_at(t0 + SETTLE * 2), ["router"]);
    assert_eq!(e.caddy.loads(), loads + 1);
}

#[test]
fn a_pair_not_valid_yet_waits_and_never_replaces_a_serving_one() {
    use rynasd::web_access::imported::DROP_DIR;
    const DAY: i64 = 86_400_000;
    let e = env();
    e.apply(&one_domain()).unwrap();
    let now = rynas_tls::now();
    // issued "tomorrow": starts in an hour (validation allows up to a day ahead)
    let later = foreign_pair(
        &["*.nas.example.com", "nas.example.com"],
        now + DAY + 3_600_000,
    );
    // a watched folder with nothing stored yet takes it, but it is not served before it starts
    e.svc.save_import(&folder_import("router", &[])).unwrap();
    let dir = e.file(DROP_DIR).join("router");
    push_pair(&dir, &later);
    let v = e.svc.check_import("router").unwrap();
    assert!(v.certificate.is_some() && v.covers.is_empty(), "{v:?}");
    let (policies, tls) = https_tls(&e.caddy.config());
    assert_eq!(policies, json!([{}]));
    assert!(!tls.to_string().contains("import-router"));
    // a serving pair is kept while the next one waits in the folder
    let current = wildcard_pair(now);
    push_pair(&dir, &current);
    e.svc.check_import("router").unwrap();
    let loads = e.caddy.loads();
    push_pair(&dir, &later);
    let v = e.svc.check_import("router").unwrap();
    assert_eq!(v.problem, Some(ImportProblem::NotYetValid));
    let from = v.pending_from_ms.unwrap();
    assert!(from > now + 3_000_000 && from <= now + 3_700_000, "{from}");
    assert_eq!(v.covers.len(), 3, "the current pair still serves");
    assert_eq!(
        v.certificate.as_ref().unwrap().fingerprint_sha256,
        rynas_tls::inspect(&current.chain).unwrap()[0].fingerprint_sha256
    );
    assert_eq!(e.caddy.loads(), loads);
    assert!(e.notes.take().is_empty(), "waiting is not trouble");
    // an upload that starts later is refused while the stored pair serves
    let mut up = upload("router", &later);
    up.source = ImportSource::Folder;
    let err = e.svc.save_import(&up).unwrap_err();
    assert_eq!(err.code(), error_codes::INVALID_VALUE);
}

#[tokio::test]
async fn pushed_pair_for_other_names_is_held_until_accepted() {
    use rynasd::web_access::imported::DROP_DIR;
    let e = env();
    e.apply(&one_domain()).unwrap();
    let app = e.app();
    let now = rynas_tls::now();
    e.svc.save_import(&folder_import("router", &[])).unwrap();
    let dir = e.file(DROP_DIR).join("router");
    // the first good pair pins its names
    push_pair(&dir, &wildcard_pair(now));
    let v = e.svc.check_import("router").unwrap();
    assert_eq!(v.names, ["*.nas.example.com", "nas.example.com"]);
    let loads = e.caddy.loads();
    // a renewal with the same names (another order, another case) applies by itself
    push_pair(
        &dir,
        &foreign_pair(&["NAS.example.com", "*.nas.example.com"], now),
    );
    let v = e.svc.check_import("router").unwrap();
    assert_eq!(v.problem, None);
    assert_eq!(e.caddy.loads(), loads + 1);
    // a push that takes over other names is held: the last good pair stays
    push_pair(
        &dir,
        &foreign_pair(
            &["*.nas.example.com", "nas.example.com", "files.example.org"],
            now,
        ),
    );
    let v = e.svc.check_import("router").unwrap();
    assert_eq!(v.problem, Some(ImportProblem::NamesChanged));
    assert_eq!(
        v.pending_names,
        ["*.nas.example.com", "files.example.org", "nas.example.com"]
    );
    assert_eq!(v.names, ["*.nas.example.com", "nas.example.com"]);
    assert_eq!(v.certificate.unwrap().dns_names.len(), 2);
    assert_eq!(e.caddy.loads(), loads + 1);
    let ev = e.notes.take();
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].code, "web_access.cert_import_names_changed");
    // seen again: no second notification
    e.svc.check_import("router").unwrap();
    assert!(e.notes.take().is_empty());
    // accept: administrators only
    let accept = "/v1/web-access/imports/router/accept";
    let (s, _) = call(&app, "bob", "POST", accept, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = call(&app, "admin", "POST", accept, None).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let v: ImportedCert = serde_json::from_slice(&b).unwrap();
    assert_eq!(v.problem, None);
    assert!(v.pending_names.is_empty());
    assert_eq!(
        v.names,
        ["*.nas.example.com", "files.example.org", "nas.example.com"]
    );
    assert_eq!(e.caddy.loads(), loads + 2);
    let ev = e.notes.take();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].code, "web_access.cert_import_ok");
    // nothing held any more: 409; unknown: 404
    let (s, _) = call(&app, "admin", "POST", accept, None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(
        &app,
        "admin",
        "POST",
        "/v1/web-access/imports/nope/accept",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // held again, then removed: the notification is resolved (L3)
    push_pair(&dir, &foreign_pair(&["nas.example.com"], now));
    e.svc.check_import("router").unwrap();
    assert_eq!(e.notes.take().len(), 1);
    e.svc.delete_import("router").unwrap();
    let ev = e.notes.take();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].code, "web_access.cert_import_ok");

    // names given at creation: even the first push must carry them
    e.svc
        .save_import(&folder_import("photos", &["Photos.nas.example.com"]))
        .unwrap();
    let dir = e.file(DROP_DIR).join("photos");
    push_pair(&dir, &wildcard_pair(now));
    let v = e.svc.check_import("photos").unwrap();
    assert_eq!(v.names, ["photos.nas.example.com"]);
    assert_eq!(v.problem, Some(ImportProblem::NamesChanged));
    assert_eq!(v.status, CertStatus::Pending, "nothing served");
    // bad names; names that are not the uploaded pair's
    let err = e
        .svc
        .save_import(&folder_import("x", &["not a name"]))
        .unwrap_err();
    assert_eq!(err.code(), error_codes::INVALID_VALUE);
    let mut up = upload("y", &wildcard_pair(now));
    up.names = vec!["nas.example.com".into()];
    assert!(e.svc.save_import(&up).is_err());
    // the request's Debug never shows the key
    assert!(!format!("{up:?}").contains("PRIVATE KEY"));
}

#[test]
fn acme_comes_back_after_expiry_and_starts_before_it() {
    const DAY: i64 = 86_400_000;
    let e = env();
    e.apply(&one_domain()).unwrap();
    let now = rynas_tls::now();
    // expires in 10 days: still served (pinned), but ACME is ordered ahead of the expiry (L2)
    let soon = foreign_pair(
        &["*.nas.example.com", "nas.example.com"],
        now - (rynas_tls::LEAF_DAYS - 10) * DAY,
    );
    let v = e.svc.save_import(&upload("wild", &soon)).unwrap();
    assert_eq!(v.status, CertStatus::ExpiresSoon);
    assert_eq!(v.covers.len(), 3);
    assert!(v.replaces_acme.is_empty());
    let (policies, tls) = https_tls(&e.caddy.config());
    assert_eq!(
        policies[0]["certificate_selection"],
        json!({"any_tag": ["rynas-import-wild"]})
    );
    assert_eq!(
        tls["certificates"]["automate"],
        json!(["*.nas.example.com", "nas.example.com"])
    );
    // a valid import replaces ACME; once it has expired, the periodic sync renders ACME again
    e.svc
        .save_import(&upload("wild", &wildcard_pair(now)))
        .unwrap();
    let (_, tls) = https_tls(&e.caddy.config());
    assert!(tls["certificates"].get("automate").is_none(), "{tls}");
    let old = foreign_pair(&["*.nas.example.com", "nas.example.com"], now - 4000 * DAY);
    let store = e.dir.path().join("state/web-access/imported");
    std::fs::write(store.join("wild.crt"), &old.chain).unwrap();
    std::fs::write(store.join("wild.key"), &old.key).unwrap();
    e.svc.sync(&|_| {}).unwrap();
    let (policies, tls) = https_tls(&e.caddy.config());
    assert_eq!(policies, json!([{}]));
    assert_eq!(
        tls["certificates"]["automate"],
        json!(["*.nas.example.com", "nas.example.com"])
    );
    assert!(!e.file("/var/lib/rynas-caddy/rynas/import-wild.crt").exists());
}

#[test]
fn watched_folder_needs_its_group() {
    use rynasd::web_access::imported::DROP_DIR;
    let e = env();
    e.apply(&one_domain()).unwrap();
    e.svc.drop_group_check.set(true).unwrap();
    let err = e
        .svc
        .save_import(&folder_import("router", &[]))
        .unwrap_err();
    assert_eq!(err.code(), error_codes::CONFLICT);
    assert!(e.svc.imports().is_empty(), "nothing stored");
    assert!(!e.file(DROP_DIR).join("router").exists());
    // uploads do not need it
    e.svc
        .save_import(&upload("wild", &wildcard_pair(rynas_tls::now())))
        .unwrap();
}
