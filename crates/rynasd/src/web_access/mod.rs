//! Web access (wave W1, `docs/phase2/docker-apps.md` §5.3, `docs/research/web-access.md`):
//! the configuration key `web_access`, its render into the engine's configuration (Caddy
//! JSON, [`caddy`]), the load over the engine's admin socket ([`admin`]), `last-good.json`
//! for a boot without rynasd, anti-drift, the timed confirmation of changes to the
//! interface's own route, local names through Avahi's D-Bus API ([`avahi`]), and route
//! diagnostics ([`diag`]).
//!
//! - **The key is the only source.** An apply validates it ([`model::validate`]), hands the
//!   routes' local names to the Avahi publisher ([`avahi::LocalNames`]; no file is written:
//!   static `/etc/avahi/hosts` entries collide with the host's own reverse record) and, in
//!   [`Applier::reconcile`], renders the engine configuration from the key and the host
//!   facts (host name, connected subnets) and loads it with `POST /load`. The engine
//!   validates it and keeps the running configuration on error: the error fails the apply,
//!   which rolls back and loads the previous value. After a load rynasd writes
//!   `/var/lib/rynas-caddy/last-good.json`, which the unit starts from.
//! - **Reload only on apply** (and when the host facts change, or the running configuration
//!   drifted): never on container events, since a reload closes WebSockets until Caddy 2.12.
//!   An identical configuration is not reloaded by the engine.
//! - **Certificates.** rynasd issues one leaf from the rynas local CA (the one of `:8443`,
//!   [`crate::tls`]) for every served name and writes it where the engine loads it from
//!   (`/var/lib/rynas-caddy/rynas/local.{crt,key}`, 0640 root:rynas-caddy). No ACME in W1.
//! - **External proxy mode** stops and disables `rynas-caddy` (80/443 free for another
//!   proxy app); the built-in mode enables and starts it.
//! - **Timed confirmation.** An apply that changes the interface's own route (names, ports,
//!   access) waits [`CONFIRM_TIMEOUT`] for `POST /web-access/confirm` arriving *through the
//!   proxy* on one of the new names; otherwise the previous value is applied again. The
//!   emergency port `:8443` never goes through the proxy, so nobody is locked out.
//! - **rynas-web's plain port follows.** After every load (and on each periodic check)
//!   rynasd writes the names the proxy serves the interface on to [`UI_STATE`]; the file is
//!   removed when the proxy does not front the interface (external mode, engine not
//!   installed, no interface names). rynas-web started with `--listen auto` reads it and
//!   moves its plain HTTP port from the network to `127.0.0.1` while it is there
//!   (D-PROXY; `docs/phase1/plan.md`, "Behind the built-in reverse proxy").

pub mod admin;
pub mod avahi;
pub mod caddy;
pub mod certs;
pub mod diag;
pub mod domains;
pub mod imported;
pub mod model;

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::apps::WebAuth;
use rynas_api_types::error_codes;
use rynas_api_types::web_access::*;
use rynas_config::files::under_root;
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile, System};
use rynas_dns::Creds;
use rynas_notify::Notifier;
use rynas_notify::engine::secrets::SecretBox;
use rynas_tls::{Names, PemPair};
use serde_json::Value;

use self::admin::AdminClient;
use self::caddy::{Caddy, Renderer};
use self::diag::{NetProbe, check};
use self::model::HostFacts;
use crate::ops::{Error, Ops, Res, user};

/// The configuration key.
pub const WEB_ACCESS_KEY: &str = "web_access";
/// The engine's admin socket (0600, owned by the engine's user; root connects).
pub const ADMIN_SOCKET: &str = "/run/rynas-caddy/admin.sock";
/// The engine's unit.
pub const UNIT: &str = "rynas-caddy.service";
/// The engine's state folder (its storage, the configuration it boots from, the
/// certificate rynasd issues for it).
pub const STATE_DIR: &str = "/var/lib/rynas-caddy";
pub const LAST_GOOD: &str = "/var/lib/rynas-caddy/last-good.json";
pub const CERT: &str = "/var/lib/rynas-caddy/rynas/local.crt";
pub const KEY: &str = "/var/lib/rynas-caddy/rynas/local.key";
/// DNS provider secrets the engine reads through `{file.*}` placeholders (wave W2).
pub const DNS_SECRETS: &str = "/var/lib/rynas-caddy/rynas/dns";
/// How often the certificate inventory is checked for notifications.
pub const CERT_CHECK_INTERVAL: Duration = Duration::from_secs(600);
/// Group of the engine's user: reads the configuration and the key rynasd writes.
pub const ENGINE_GROUP: &str = "rynas-caddy";
pub const AVAHI_HOSTS: &str = "/etc/avahi/hosts";
pub const AVAHI_UNIT: &str = "avahi-daemon.service";
/// The names the proxy serves the interface on, one per line (0644; absent when the proxy
/// does not front the interface). rynas-web reads it (`--proxy-state`).
pub const UI_STATE: &str = "/run/rynas/web-access.ui";
/// How long a change of the interface's route waits for its confirmation.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);
/// How often the host facts and the running configuration are checked.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(30);
/// Where the engine's unit file may be (the package installs it in the first).
const UNIT_FILES: &[&str] = &[
    "/usr/lib/systemd/system/rynas-caddy.service",
    "/lib/systemd/system/rynas-caddy.service",
    "/etc/systemd/system/rynas-caddy.service",
];

fn audit(who: &str, action: &str, target: &str) {
    tracing::info!(target: "rynas::audit", "web-access {action} {target} by {who}");
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// render input and resolved names
// ---------------------------------------------------------------------------------------------

/// Paths as the engine sees them (absolute, never below a dry root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnginePaths {
    pub admin_socket: String,
    pub storage: String,
    pub cert: String,
    pub key: String,
    /// Folder of the DNS secret files (`<dir>/<credential>/<field>`).
    pub dns_secrets: String,
}

impl Default for EnginePaths {
    fn default() -> Self {
        Self {
            admin_socket: ADMIN_SOCKET.into(),
            storage: format!("{STATE_DIR}/data"),
            cert: CERT.into(),
            key: KEY.into(),
            dns_secrets: DNS_SECRETS.into(),
        }
    }
}

/// Decrypted credentials by id: provider and values.
pub type CredValues = BTreeMap<String, (DnsProvider, Creds)>;

/// Everything a render depends on.
pub struct RenderInput<'a> {
    pub cfg: &'a WebAccessConfig,
    pub facts: &'a HostFacts,
    pub paths: &'a EnginePaths,
    /// rynas-web's plain HTTP port on loopback (the interface's upstream).
    pub ui_port: u16,
    /// rynas-web's own HTTPS port.
    pub emergency_port: u16,
    /// Decrypted credentials of the domains (only the fields written as values, never as
    /// placeholders, are rendered from it).
    pub creds: &'a CredValues,
    /// The valid imported certificates (§5.5).
    pub imports: &'a [imported::Active],
}

/// A route with the names it gets on this host.
#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub route: WebRoute,
    /// Every served name: the local one first, then the own-domain ones.
    pub names: Vec<String>,
    /// `<label>-<host>.local`, when it has one.
    pub local: Option<String>,
    /// `<label>.<domain>` for each enabled domain.
    pub domain_names: Vec<String>,
    /// The NAS's own names the route answers on its extra ports (`names.nas`, §5.6):
    /// `<host>.local`, `<host>`, each enabled domain's name.
    pub nas: Vec<String>,
    pub problems: Vec<String>,
}

impl ResolvedRoute {
    /// The names served on the route's extra ports: its own, then the NAS's.
    pub fn port_names(&self) -> Vec<String> {
        let mut v = self.names.clone();
        v.extend(self.nas.iter().filter(|n| !self.names.contains(n)).cloned());
        v
    }
}

/// Served names: the interface's own and each route's.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub host: String,
    /// The interface's names: local ones, then the domains'.
    pub ui: Vec<String>,
    /// The interface's local names (`<host>.local`, `<host>`).
    pub ui_local: Vec<String>,
    pub routes: Vec<ResolvedRoute>,
}

impl Resolved {
    /// Every served name.
    pub fn all_names(&self) -> Vec<String> {
        let mut v = self.ui.clone();
        for r in &self.routes {
            v.extend(r.names.iter().cloned());
        }
        for r in &self.routes {
            for n in &r.nas {
                if !v.contains(n) {
                    v.push(n.clone());
                }
            }
        }
        v
    }

    /// The names the local leaf covers (no own-domain names: those get ACME certificates).
    pub fn local_names(&self) -> Vec<String> {
        let mut v = self.ui_local.clone();
        v.extend(self.routes.iter().filter_map(|r| r.local.clone()));
        // the NAS's local names on extra ports, when the interface itself is off
        for r in &self.routes {
            for n in r.nas.iter().filter(|n| n.ends_with(".local") || !n.contains('.')) {
                if !v.contains(n) {
                    v.push(n.clone());
                }
            }
        }
        v
    }
}

/// The names each route gets: `<label>-<host>.local` when it is enabled, local names are
/// on and the name fits; `<label>.<domain>` for each enabled own domain; routes sorted by
/// id.
pub fn resolve(cfg: &WebAccessConfig, facts: &HostFacts) -> Resolved {
    let host = facts.host();
    let builtin = cfg.settings.mode == ProxyMode::Builtin;
    let (ui_local, mut ui) = if builtin && cfg.settings.ui.enabled {
        let l = model::ui_names(&host);
        (l.clone(), l)
    } else {
        (Vec::new(), Vec::new())
    };
    if builtin && cfg.settings.ui.enabled {
        ui.extend(
            domains::active(cfg)
                .filter(|d| d.ui)
                .map(|d| d.name.clone()),
        );
    }
    let mut routes: Vec<ResolvedRoute> = cfg
        .routes
        .iter()
        .map(|r| {
            let mut problems = Vec::new();
            let mut local = None;
            let mut domain_names = Vec::new();
            if !r.enabled {
                problems.push("disabled".to_string());
            } else {
                if !cfg.settings.local_names || !r.names.local {
                    problems.push("local_names_off".to_string());
                } else {
                    match model::local_name(&r.label, &host) {
                        Some(n) => local = Some(n),
                        None => problems.push("name_too_long".to_string()),
                    }
                }
                if r.names.domain {
                    for d in domains::active(cfg) {
                        match domains::route_name(&r.label, &d.name) {
                            Some(n) => domain_names.push(n),
                            None => problems.push("domain_name_too_long".to_string()),
                        }
                    }
                }
            }
            if !builtin {
                local = None;
                domain_names.clear();
            }
            let names = local
                .iter()
                .cloned()
                .chain(domain_names.iter().cloned())
                .collect();
            let mut nas = Vec::new();
            if builtin && r.enabled && r.names.nas && !r.ports.is_empty() {
                nas.extend(model::ui_names(&host));
                nas.extend(domains::active(cfg).map(|d| d.name.clone()));
            }
            ResolvedRoute {
                route: r.clone(),
                names,
                local,
                domain_names,
                nas,
                problems,
            }
        })
        .collect();
    routes.sort_by(|a, b| a.route.id.cmp(&b.route.id));
    Resolved {
        host,
        ui,
        ui_local,
        routes,
    }
}

/// The local names published through Avahi, as `<address> <name>` lines (the interface's
/// own `<host>.local` is Avahi's host name already). Empty in external mode, with local
/// names off, or without an IPv4 address.
pub fn avahi_block(cfg: &WebAccessConfig, facts: &HostFacts) -> String {
    avahi::lines(&avahi::records(cfg, facts))
}

/// `https://<name>[:port]<path>`.
pub fn url_of(name: &str, https_port: u16, path: &str) -> String {
    if https_port == 443 {
        format!("https://{name}{path}")
    } else {
        format!("https://{name}:{https_port}{path}")
    }
}

/// What the interface's own route depends on: a change waits for a confirmation.
fn ui_fingerprint(c: &WebAccessConfig) -> String {
    let s = &c.settings;
    format!(
        "{:?}|{}|{}|{}|{:?}|{:?}",
        s.mode, s.http_port, s.https_port, s.ui.enabled, s.ui.access, s.ui.allow
    )
}

fn config_of(v: Option<&Value>) -> WebAccessConfig {
    v.and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// The gid of `group` in `<root>/etc/group`, applied only as root.
/// `text` with every secret credential value masked.
fn redact_creds(text: &str, creds: &CredValues) -> String {
    creds.values().fold(text.to_string(), |t, (p, c)| {
        let secret: Creds = c
            .iter()
            .filter(|(k, _)| rynas_dns::fields(*p).iter().any(|f| f.name == *k && f.secret))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        rynas_dns::redact(&t, &secret)
    })
}

/// rynasd's own user (root in production; the test user in tests).
fn euid() -> u32 {
    nix::unistd::Uid::effective().as_raw()
}

fn group_gid(root: &Path, group: &str) -> Option<u32> {
    if !nix::unistd::Uid::effective().is_root() {
        return None;
    }
    let text = std::fs::read_to_string(under_root(root, Path::new("/etc/group"))).ok()?;
    text.lines().find_map(|l| {
        let mut f = l.split(':');
        (f.next() == Some(group))
            .then(|| f.nth(1).and_then(|g| g.parse().ok()))
            .flatten()
    })
}

// ---------------------------------------------------------------------------------------------
// the service
// ---------------------------------------------------------------------------------------------

pub type FactsProbe = Box<dyn Fn() -> HostFacts + Send + Sync>;
pub type CaProvider = Box<dyn Fn() -> io::Result<PemPair> + Send + Sync>;

#[derive(Debug, Default)]
struct EngineMem {
    last_error: Option<String>,
    last_load_ms: Option<i64>,
    in_sync: Option<bool>,
    facts: Option<String>,
}

struct Confirm {
    generation: u64,
    previous: Value,
    names: Vec<String>,
    deadline: Instant,
    deadline_ms: i64,
}

/// What a catalogue app's route is made from (its `[web]` block and host port).
#[derive(Debug, Clone, Default)]
pub struct AppRouteSpec {
    pub stack: String,
    pub app: String,
    pub port: u16,
    pub scheme: UpstreamScheme,
    pub auth: WebAuth,
    pub path: String,
    pub websocket: bool,
    pub subpath: bool,
    pub body: Option<String>,
    pub timeout: Option<String>,
    pub buffering: bool,
}

pub struct WebAccessSvc {
    /// Files are written below this directory ("/" in production, the dry root otherwise).
    pub root: PathBuf,
    pub admin: AdminClient,
    pub ui_port: u16,
    pub emergency_port: u16,
    pub paths: EnginePaths,
    pub confirm_timeout: Duration,
    /// Tests: whether the engine counts as installed (else its unit file or socket).
    pub installed_override: Option<bool>,
    facts: FactsProbe,
    ca: CaProvider,
    probe: Box<dyn NetProbe>,
    /// Set once the configuration store exists (it holds this service's domain).
    pub ops: OnceLock<Arc<Ops>>,
    engine: Mutex<EngineMem>,
    confirm: Mutex<Option<Confirm>>,
    confirm_gen: AtomicU64,
    /// One engine push at a time (apply, watch, sync).
    push_lock: Mutex<()>,
    /// The routes' local names, published through Avahi's D-Bus API.
    pub names: avahi::LocalNames,
    /// rynasd's state folder (`/var/lib/rynas`): the record sets written for the domains.
    pub state_dir: PathBuf,
    /// The record clients of the DNS providers ([`domains::real_factory`]; tests inject
    /// fakes, `--dry-system` [`domains::offline_factory`]).
    pub dns: domains::DnsFactory,
    /// The age key that seals the DNS credentials (the notification engine's).
    pub secret_box: OnceLock<Arc<SecretBox>>,
    /// The interface's certificate service (certificate dashboard).
    pub tls: OnceLock<Arc<crate::tls::TlsSvc>>,
    /// Certificate notifications.
    pub notifier: OnceLock<Arc<dyn Notifier>>,
    /// Recent JSON log lines of the engine (certificate errors); `journalctl` by default.
    pub engine_log: Box<dyn Fn() -> String + Send + Sync>,
    /// How long the errors read from [`Self::engine_log`] are reused (review 2026-11-07,
    /// L3: `journalctl` on every inventory and domain test); tests set zero.
    pub engine_log_ttl: Duration,
    dns_mem: Mutex<domains::DnsMem>,
    /// One record sync at a time; held across the provider calls, unlike `dns_mem`
    /// (review 2026-11-07, M1: the pages never wait on a slow provider).
    dns_sync: Mutex<()>,
    /// Provider reads shared by plans (review 2026-11-07, M6).
    dns_cache: rynas_dns::cache::Cache,
    cert_mem: Mutex<CertMem>,
    /// Imported certificates: watched folder state; also serialises changes to and reads of
    /// the store (review 2026-11-07, M3).
    imports: Mutex<imported::ImportMem>,
    /// Whether the group of the watched folders must exist (default: when running as root,
    /// where the folders get it; tests set `true` to see the missing-group path).
    pub drop_group_check: OnceLock<bool>,
}

/// What the certificate checks remember.
#[derive(Default)]
struct CertMem {
    /// When each expected ACME subject was first seen missing (ms).
    expected_since: BTreeMap<String, i64>,
    notified: certs::Notified,
    last_check: Option<Instant>,
    /// The engine's certificate errors and when they were read.
    log_errors: Option<(Instant, BTreeMap<String, String>)>,
}

/// The engine's recent log lines: `journalctl -u rynas-caddy -o cat` of the last day.
fn journal_lines() -> String {
    std::process::Command::new("journalctl")
        .args([
            "-u",
            UNIT,
            "-o",
            "cat",
            "--no-pager",
            "-n",
            "2000",
            "--since",
            "-24h",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

pub type WaState = Option<Arc<WebAccessSvc>>;

impl WebAccessSvc {
    pub fn new(
        root: PathBuf,
        admin_socket: PathBuf,
        ui_port: u16,
        emergency_port: u16,
        facts: FactsProbe,
        ca: CaProvider,
        probe: Box<dyn NetProbe>,
    ) -> Self {
        Self {
            admin: AdminClient::new(admin_socket),
            ui_port,
            emergency_port,
            paths: EnginePaths::default(),
            confirm_timeout: CONFIRM_TIMEOUT,
            installed_override: None,
            facts,
            ca,
            probe,
            ops: OnceLock::new(),
            engine: Mutex::new(EngineMem::default()),
            confirm: Mutex::new(None),
            confirm_gen: AtomicU64::new(0),
            push_lock: Mutex::new(()),
            names: avahi::LocalNames::new(),
            state_dir: under_root(&root, Path::new("/var/lib/rynas")),
            root,
            dns: domains::real_factory(),
            secret_box: OnceLock::new(),
            tls: OnceLock::new(),
            notifier: OnceLock::new(),
            engine_log: Box::new(journal_lines),
            engine_log_ttl: Duration::from_secs(60),
            dns_mem: Mutex::new(domains::DnsMem::default()),
            dns_sync: Mutex::new(()),
            dns_cache: rynas_dns::cache::Cache::new(),
            cert_mem: Mutex::new(CertMem::default()),
            imports: Mutex::new(imported::ImportMem::default()),
            drop_group_check: OnceLock::new(),
        }
    }

    fn sb(&self) -> Option<&SecretBox> {
        self.secret_box.get().map(|b| b.as_ref())
    }

    /// The decrypted credentials the domains of `cfg` use; unreadable ones are left out.
    pub fn cred_values(&self, cfg: &WebAccessConfig) -> CredValues {
        let used: BTreeSet<&str> = domains::active(cfg)
            .filter_map(|d| d.credential.as_deref())
            .collect();
        cfg.credentials
            .iter()
            .filter(|c| used.contains(c.id.as_str()))
            .filter_map(|c| {
                domains::open_creds(c, self.sb())
                    .map_err(|e| tracing::warn!("web access: {e}"))
                    .ok()
                    .map(|v| (c.id.clone(), (c.provider, v)))
            })
            .collect()
    }

    /// Seals given credential values and carries the stored ones of `previous` forward
    /// (input from the API: a client-sent envelope is dropped first).
    pub fn seal_input(
        &self,
        c: &mut WebAccessConfig,
        previous: &[&WebAccessConfig],
    ) -> Result<(), Invalid> {
        domains::strip_input(c);
        domains::seal_credentials(c, previous, self.sb())
    }

    /// Hands the local names of `cfg` on this host to the Avahi publisher.
    pub fn publish_names(&self, cfg: &WebAccessConfig, facts: &HostFacts) {
        self.names.set(avahi::records(cfg, facts));
    }

    /// Migration: removes rynas's old block from `/etc/avahi/hosts` (written before the names
    /// moved to Avahi's D-Bus API) and reloads Avahi once when it runs. Returns whether a
    /// block was removed.
    pub fn migrate_avahi_hosts(&self, system: &dyn System, log: &dyn Fn(&str)) -> bool {
        let p = self.path(AVAHI_HOSTS);
        let Ok(text) = std::fs::read_to_string(&p) else {
            return false;
        };
        let Some(clean) = avahi::strip_hosts_block(&text) else {
            return false;
        };
        let mode = std::fs::metadata(&p)
            .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o7777)
            .unwrap_or(0o644);
        if let Err(e) = crate::tls::write_atomic(&p, clean.as_bytes(), mode, None) {
            log(&format!("web access: {}: {e}", p.display()));
            return false;
        }
        log(&format!(
            "web access: removed rynas's old block from {AVAHI_HOSTS} (local names are published through Avahi's D-Bus API)"
        ));
        if system.unit_active(AVAHI_UNIT).unwrap_or(false)
            && let Err(e) = system.reload_or_restart_unit(AVAHI_UNIT)
        {
            log(&format!("web access: reloading Avahi: {e}"));
        }
        true
    }

    /// The running system's facts: kernel host name, `ip -j addr show`,
    /// `ip -j route show default`.
    pub fn system_facts() -> FactsProbe {
        Box::new(|| {
            let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
            let ip = |args: &[&str]| {
                std::process::Command::new("ip")
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                    .unwrap_or_default()
            };
            model::parse_facts(
                &host,
                &ip(&["-j", "addr", "show"]),
                &ip(&["-j", "route", "show", "default"]),
            )
        })
    }

    pub fn facts(&self) -> HostFacts {
        (self.facts)()
    }

    /// Host ports with a listening TCP socket (`/proc/net/tcp{,6}` below the root):
    /// services, and containers published through docker-proxy.
    pub fn listening_ports(&self) -> BTreeSet<u16> {
        let mut v = BTreeSet::new();
        for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
            if let Ok(t) = std::fs::read_to_string(self.path(f)) {
                v.extend(model::listening_tcp(&t));
            }
        }
        v
    }

    fn path(&self, abs: &str) -> PathBuf {
        under_root(&self.root, Path::new(abs))
    }

    /// The engine is installed: its unit file below the root, or its admin socket.
    pub fn installed(&self) -> bool {
        self.installed_override.unwrap_or_else(|| {
            self.admin.present() || UNIT_FILES.iter().any(|u| self.path(u).exists())
        })
    }

    /// The applied key (defaults when never applied).
    pub fn applied(&self) -> WebAccessConfig {
        self.ops
            .get()
            .and_then(|o| o.config.applied(WEB_ACCESS_KEY).ok().flatten())
            .map(|v| config_of(Some(&v)))
            .unwrap_or_default()
    }

    /// Web access serves apps: built-in mode and the engine installed.
    pub fn proxy_active(&self) -> bool {
        self.applied().settings.mode == ProxyMode::Builtin && self.installed()
    }

    /// Host ports apps may not publish: the proxy's, unless external mode. `(port, udp)`.
    pub fn reserved_ports(&self) -> Vec<(u16, bool)> {
        let s = self.applied().settings;
        if s.mode == ProxyMode::External {
            return Vec::new();
        }
        let mut v = vec![(s.http_port, false), (s.https_port, false)];
        if s.h3 {
            v.push((s.https_port, true));
        }
        // the routes' extra ports (§5.6)
        for p in model::extra_ports(&self.applied()) {
            v.push((p, false));
            if s.h3 {
                v.push((p, true));
            }
        }
        v
    }

    fn input<'a>(
        &'a self,
        cfg: &'a WebAccessConfig,
        facts: &'a HostFacts,
        creds: &'a CredValues,
        imports: &'a [imported::Active],
    ) -> RenderInput<'a> {
        RenderInput {
            cfg,
            facts,
            paths: &self.paths,
            ui_port: self.ui_port,
            emergency_port: self.emergency_port,
            creds,
            imports,
        }
    }

    /// The resolved names and the engine configuration of `cfg` on this host.
    pub fn render(&self, cfg: &WebAccessConfig, facts: &HostFacts) -> (Resolved, Value) {
        let creds = self.cred_values(cfg);
        let active = self.active_imports(rynas_tls::now());
        self.render_with(cfg, facts, &creds, &active)
    }

    fn render_with(
        &self,
        cfg: &WebAccessConfig,
        facts: &HostFacts,
        creds: &CredValues,
        active: &[imported::Active],
    ) -> (Resolved, Value) {
        let resolved = resolve(cfg, facts);
        let json = Caddy.render(&self.input(cfg, facts, creds, active), &resolved);
        (resolved, json)
    }

    /// The render with every credential value replaced by `***` (what a plan shows).
    pub fn render_redacted(&self, cfg: &WebAccessConfig, facts: &HostFacts) -> (Resolved, Value) {
        let creds: CredValues = self
            .cred_values(cfg)
            .into_iter()
            .map(|(id, (p, v))| {
                let v = v
                    .into_iter()
                    .map(|(k, val)| {
                        let secret = rynas_dns::fields(p).iter().any(|f| f.name == k && f.secret);
                        (k, if secret { "***".to_string() } else { val })
                    })
                    .collect();
                (id, (p, v))
            })
            .collect();
        let active = self.active_imports(rynas_tls::now());
        self.render_with(cfg, facts, &creds, &active)
    }

    /// Makes sure the engine's certificate covers `names` and is issued by the current
    /// local CA; returns whether new files were written.
    pub fn ensure_cert(&self, names: &[String]) -> io::Result<bool> {
        let want = Names {
            dns: names.iter().cloned().collect(),
            ips: BTreeSet::new(),
        };
        let ca = (self.ca)()?;
        let now = rynas_tls::now();
        let (crt, key) = (self.path(CERT), self.path(KEY));
        let ca_fp = rynas_tls::inspect(&ca.chain)
            .ok()
            .and_then(|c| c.first().map(|c| c.fingerprint_sha256.clone()));
        let current_ok = std::fs::read_to_string(&crt)
            .ok()
            .filter(|_| key.exists())
            .and_then(|pem| rynas_tls::inspect(&pem).ok())
            .is_some_and(|chain| {
                chain
                    .get(1)
                    .map(|c| Some(c.fingerprint_sha256.clone()) == ca_fp)
                    == Some(true)
                    && !rynas_tls::needs_reissue(&chain[0], &want, now)
            });
        if current_ok {
            return Ok(false);
        }
        let leaf = rynas_tls::issue_leaf(&ca, &want, now).map_err(io::Error::other)?;
        let gid = group_gid(&self.root, ENGINE_GROUP);
        let dir = self.rynas_dir(gid)?;
        let owner = gid.map(|g| (0, g));
        // the key first: a reload between the two renames would find the old chain with
        // the old key still in place only if the chain came first
        let name = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
        rynas_docker::safefs::write_at(&dir, &name(KEY), leaf.key.as_bytes(), 0o640, owner)?;
        rynas_docker::safefs::write_at(&dir, &name(CERT), leaf.chain.as_bytes(), 0o640, owner)?;
        tracing::info!("web access: new certificate for {}", names.join(", "));
        Ok(true)
    }

    /// Writes the DNS secret files below `rynas/dns`, all through descriptors from the
    /// checked `rynas/` folder ([`domains::write_engine_secrets`]). Returns whether anything
    /// changed.
    fn write_dns_secrets(&self, values: &CredValues) -> io::Result<bool> {
        let gid = group_gid(&self.root, ENGINE_GROUP);
        let dir = if values.is_empty() {
            // nothing to write: only clean up an existing, trusted `rynas/`
            match self.existing_rynas_dir()? {
                Some(d) => d,
                None => return Ok(false),
            }
        } else {
            self.rynas_dir(gid)?
        };
        domains::write_engine_secrets(&dir, gid, values)
    }

    /// `rynas/` when it exists and is rynasd's own (owned by rynasd's user, a real folder);
    /// `None` when missing or not trusted (the next [`Self::rynas_dir`] replaces it).
    fn existing_rynas_dir(&self) -> io::Result<Option<std::fs::File>> {
        let p = self.path(LAST_GOOD);
        let parent = p.parent().expect("has a parent");
        if !parent.exists() {
            return Ok(None);
        }
        let state = rynas_docker::safefs::open_start(parent)?;
        match rynas_docker::safefs::open_dir_at(&state, "rynas") {
            Ok(d) if rynas_docker::safefs::stat_of(&d)?.1 == euid() => Ok(Some(d)),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// The engine's state folder (`/var/lib/rynas-caddy`, writable by the engine's user):
    /// made when missing (its parents are root's), opened as a descriptor.
    fn engine_state_dir(&self) -> io::Result<std::fs::File> {
        let p = self.path(LAST_GOOD);
        let dir = p.parent().expect("has a parent");
        std::fs::create_dir_all(dir)?;
        rynas_docker::safefs::open_start(dir)
    }

    /// `rynas/` in the engine's state folder: rynasd's own files (certificate and key),
    /// `0750 root:rynas-caddy` so that the engine can read but never replace them.
    ///
    /// The engine's user can rename `rynas/` inside its own folder and plant one of its own
    /// (review 2026-11-07, H1): an existing `rynas` that is not a real folder owned by
    /// rynasd's user is moved aside (`rynas.untrusted-<n>`) and a fresh one is made, so
    /// nothing planted inside is ever trusted. An existing trusted folder gets the owner and
    /// mode back.
    fn rynas_dir(&self, gid: Option<u32>) -> io::Result<std::fs::File> {
        use rynas_docker::safefs;
        let state = self.engine_state_dir()?;
        let owner = gid.map(|g| (0, g));
        let trusted = match safefs::open_dir_at(&state, "rynas") {
            Ok(d) => safefs::stat_of(&d)?.1 == euid(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
                false
            }
            Err(e) => return Err(e),
        };
        if !trusted {
            let aside = format!(
                "rynas.untrusted-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            );
            tracing::warn!(
                "web access: {STATE_DIR}/rynas is not rynasd's own folder: moved aside as {aside}"
            );
            safefs::rename_at(&state, "rynas", &aside)?;
        }
        let (d, made) = safefs::ensure_dir_at(&state, "rynas", 0o750, owner)?;
        if safefs::stat_of(&d)?.1 != euid() {
            // raced by another plant between the rename and the mkdir: give up this round
            return Err(io::Error::other(format!(
                "{STATE_DIR}/rynas is not rynasd's own folder"
            )));
        }
        if !made && owner.is_some() {
            safefs::set_owner_mode(&d, 0o750, owner)?;
        }
        Ok(d)
    }

    fn write_last_good(&self, json: &Value) -> io::Result<()> {
        let mut text = serde_json::to_vec_pretty(json).map_err(io::Error::other)?;
        text.push(b'\n');
        let dir = self.engine_state_dir()?;
        let name = LAST_GOOD.rsplit('/').next().unwrap_or(LAST_GOOD);
        if rynas_docker::safefs::read_at(&dir, name, 16 << 20)
            .is_ok_and(|old| old.as_bytes() == text)
        {
            return Ok(());
        }
        // the engine's folder is writable by its user: descriptor-based write, root-owned
        // file it can read (group) but not change in place
        let owner = group_gid(&self.root, ENGINE_GROUP).map(|g| (0, g));
        rynas_docker::safefs::write_at(&dir, name, &text, 0o640, owner)
    }

    /// Publishes whether (and on which names) the proxy fronts the interface: [`UI_STATE`],
    /// rewritten only when it changes. Best effort: a failure is logged, never an apply
    /// error (rynas-web then keeps its plain port as it is).
    pub fn publish_ui(&self, names: &[String]) {
        let p = self.path(UI_STATE);
        let r = if names.is_empty() {
            match std::fs::remove_file(&p) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            let mut text = names.join("\n");
            text.push('\n');
            if std::fs::read(&p).is_ok_and(|old| old == text.as_bytes()) {
                return;
            }
            std::fs::create_dir_all(p.parent().expect("has a parent"))
                .and_then(|()| crate::tls::write_atomic(&p, text.as_bytes(), 0o644, None))
        };
        if let Err(e) = r {
            tracing::warn!("web access: {}: {e}", p.display());
        }
    }

    fn note(&self, f: impl FnOnce(&mut EngineMem)) {
        f(&mut self.engine.lock().unwrap_or_else(|e| e.into_inner()));
    }

    /// Brings the running engine to the render of `cfg`: certificate, comparison with the
    /// running configuration, `/load` when they differ, `last-good.json`. Returns whether a
    /// load happened. Needs the engine running (built-in mode, installed).
    pub fn push(
        &self,
        cfg: &WebAccessConfig,
        facts: &HostFacts,
        log: &dyn Fn(&str),
    ) -> Result<bool, String> {
        let _g = self.push_lock.lock().unwrap_or_else(|e| e.into_inner());
        let creds = self.cred_values(cfg);
        let active = self.active_imports(rynas_tls::now());
        let (resolved, json) = self.render_with(cfg, facts, &creds, &active);
        if json.is_null() {
            self.publish_ui(&[]);
            return Ok(false);
        }
        let names = resolved.all_names();
        let local = resolved.local_names();
        let mut cert_changed = if local.is_empty() {
            false
        } else {
            self.ensure_cert(&local)
                .map_err(|e| format!("the certificate for the local names: {e}"))?
        };
        if cert_changed {
            log("web access: new certificate for the local names");
        }
        // the DNS secrets the engine reads through placeholders: a changed value with an
        // unchanged configuration still needs a reload
        let engine_creds = domains::engine_credentials(cfg);
        let needed: CredValues = creds
            .iter()
            .filter(|(id, _)| engine_creds.contains(*id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let secrets_changed = self
            .write_dns_secrets(&needed)
            .map_err(|e| format!("{DNS_SECRETS}: {e}"))?;
        if secrets_changed {
            log("web access: DNS provider credentials written for the proxy");
            cert_changed = true;
        }
        // imported certificates: the engine's copies of the valid pairs (§5.5)
        if self
            .write_import_files(&active)
            .map_err(|e| format!("the imported certificates for the proxy: {e}"))?
        {
            log("web access: imported certificates written for the proxy");
            cert_changed = true;
        }
        if !self.admin.wait_ready(Duration::from_secs(10)) {
            let m = format!(
                "rynas-caddy does not answer on {}",
                self.admin.socket.display()
            );
            self.note(|e| {
                e.in_sync = None;
                e.last_error = Some(m.clone());
            });
            return Err(m);
        }
        if !cert_changed && self.admin.config().is_ok_and(|cur| cur == json) {
            self.note(|e| e.in_sync = Some(true));
            self.write_last_good(&json)
                .map_err(|e| format!("{LAST_GOOD}: {e}"))?;
            self.publish_ui(&resolved.ui);
            return Ok(false);
        }
        log(&format!(
            "web access: loading the proxy configuration ({} names)",
            names.len()
        ));
        if let Err(m) = self.admin.load(&json, cert_changed) {
            // the engine may echo the configuration, and two providers carry a literal
            // secret in it (review 2026-11-07, M4): never store or log a value
            let m = redact_creds(&m, &creds);
            self.note(|e| e.last_error = Some(m.clone()));
            return Err(m);
        }
        self.write_last_good(&json)
            .map_err(|e| format!("{LAST_GOOD}: {e}"))?;
        self.publish_ui(&resolved.ui);
        self.note(|e| {
            e.in_sync = Some(true);
            e.last_error = None;
            e.last_load_ms = Some(now_ms());
            e.facts = Some(facts.fingerprint());
        });
        Ok(true)
    }

    /// The engine side of an apply (and of its rollback): units, load, confirmation.
    pub fn engine_apply(
        self: &Arc<Self>,
        from: Option<&Value>,
        to: Option<&Value>,
        system: &dyn System,
        log: &dyn Fn(&str),
    ) -> Result<(), String> {
        let new = config_of(to);
        let facts = self.facts();
        if !self.installed() {
            log(
                "web access: rynas-caddy is not installed; the settings are kept and loaded once it is",
            );
            self.publish_ui(&[]);
            return Ok(());
        }
        if new.settings.mode == ProxyMode::External {
            log(
                "web access: external proxy mode, stopping rynas-caddy (ports 80 and 443 are free)",
            );
            self.publish_ui(&[]);
            system.set_unit_enabled(UNIT, false)?;
            self.note(|e| e.in_sync = None);
            *self.confirm.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return Ok(());
        }
        if !system.unit_active(UNIT).unwrap_or(false) {
            log("web access: starting rynas-caddy");
            system.set_unit_enabled(UNIT, true)?;
        }
        self.push(&new, &facts, log)?;
        self.after_load(from, to, &new, &facts, log);
        Ok(())
    }

    /// Arms or clears the timed confirmation after a successful load.
    fn after_load(
        self: &Arc<Self>,
        from: Option<&Value>,
        to: Option<&Value>,
        new: &WebAccessConfig,
        facts: &HostFacts,
        log: &dyn Fn(&str),
    ) {
        let mut pending = self.confirm.lock().unwrap_or_else(|e| e.into_inner());
        // back to the value a confirmation would restore: a rollback, nothing to confirm
        if let (Some(p), Some(t)) = (pending.as_ref(), to)
            && &p.previous == t
        {
            *pending = None;
            return;
        }
        let (Some(from), Some(_)) = (from, to) else {
            return;
        };
        let old = config_of(Some(from));
        let ui = resolve(new, facts).ui;
        if ui_fingerprint(&old) == ui_fingerprint(new) || ui.is_empty() {
            return;
        }
        let generation = self.confirm_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let deadline_ms = now_ms() + self.confirm_timeout.as_millis() as i64;
        log(&format!(
            "web access: the interface's own route changed; confirm it within {} s through https://{}/ or the previous settings come back",
            self.confirm_timeout.as_secs(),
            ui[0]
        ));
        // a confirmation already pending keeps its original previous value
        let previous = pending
            .as_ref()
            .map(|p| p.previous.clone())
            .unwrap_or_else(|| from.clone());
        *pending = Some(Confirm {
            generation,
            previous,
            names: ui,
            deadline: Instant::now() + self.confirm_timeout,
            deadline_ms,
        });
        drop(pending);
        let me = self.clone();
        std::thread::spawn(move || me.confirm_timer(generation));
    }

    fn confirm_timer(self: Arc<Self>, generation: u64) {
        loop {
            let previous = {
                let p = self.confirm.lock().unwrap_or_else(|e| e.into_inner());
                match p.as_ref() {
                    Some(c) if c.generation == generation => {
                        if Instant::now() < c.deadline {
                            None
                        } else {
                            Some(c.previous.clone())
                        }
                    }
                    // confirmed, replaced or rolled back
                    _ => return,
                }
            };
            let Some(previous) = previous else {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            };
            tracing::warn!(
                "web access: the interface's new route was not confirmed in time; restoring the previous settings"
            );
            let Some(ops) = self.ops.get() else { return };
            if let Err(e) = ops.config.apply_value(
                "rynas",
                WEB_ACCESS_KEY,
                Some(previous),
                "web access: the interface was not confirmed through its new address in time, previous settings restored",
                &|l| tracing::info!("{l}"),
            ) {
                tracing::error!("web access: restoring the previous settings failed: {e}");
                *self.confirm.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            return;
        }
    }

    /// A confirmation: accepted only through the proxy (rynas-web's loopback port as the
    /// local address) on one of the new names.
    pub fn confirm(&self, host: Option<&str>, local: Option<SocketAddr>) -> Res<()> {
        let mut p = self.confirm.lock().unwrap_or_else(|e| e.into_inner());
        let Some(c) = p.as_ref() else {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::NOTHING_PENDING,
                "no change of the interface's route waits for a confirmation".into(),
            ));
        };
        let via_proxy = local.is_some_and(|a| a.ip().is_loopback() && a.port() == self.ui_port);
        let host = host
            .map(|h| {
                let h = h.trim().to_ascii_lowercase();
                match h.rsplit_once(':') {
                    Some((n, port))
                        if !n.ends_with(']') && port.bytes().all(|b| b.is_ascii_digit()) =>
                    {
                        n.to_string()
                    }
                    _ => h,
                }
            })
            .unwrap_or_default();
        if via_proxy && c.names.contains(&host) {
            *p = None;
            return Ok(());
        }
        Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            format!(
                "confirm through the new address https://{}/ (this request did not come through the proxy on that name)",
                c.names[0]
            ),
        ))
    }

    fn pending_confirm(&self) -> Option<PendingConfirm> {
        self.confirm
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|c| PendingConfirm {
                deadline_ms: c.deadline_ms,
                names: c.names.clone(),
            })
    }

    /// The engine's state; reads the running configuration when it answers.
    pub fn engine_state(&self) -> ProxyEngine {
        let installed = self.installed();
        let running = installed && self.admin.config().is_ok();
        let m = self.engine.lock().unwrap_or_else(|e| e.into_inner());
        ProxyEngine {
            installed,
            running,
            in_sync: if running { m.in_sync } else { None },
            last_load_ms: m.last_load_ms,
            last_error: m.last_error.clone(),
        }
    }

    /// Anti-drift and host facts: called at start, periodically and by `POST /sync`.
    /// - never applied and the engine installed: the default value is applied (the proxy
    ///   serves the interface from install);
    /// - the host facts changed since the last load: the applied value is written again
    ///   (local names, access ranges, certificate names);
    /// - otherwise the running configuration is compared with the render and loaded again
    ///   when it differs (the engine restarted from an older `last-good.json`, or someone
    ///   changed it through the socket).
    pub fn sync(self: &Arc<Self>, log: &dyn Fn(&str)) -> Result<(), String> {
        let Some(ops) = self.ops.get() else {
            return Ok(());
        };
        // local names follow the applied value and the host facts (address, host name)
        self.publish_names(&self.applied(), &self.facts());
        if self.migrate_avahi_hosts(ops.config.system(), log)
            && ops
                .config
                .applied(WEB_ACCESS_KEY)
                .map_err(|e| e.to_string())?
                .is_some()
        {
            // forget the old block in the drift records
            ops.config
                .drift_overwrite(WEB_ACCESS_KEY, log)
                .map_err(|e| e.to_string())?;
        }
        if !self.installed() {
            self.publish_ui(&[]);
            return Ok(());
        }
        let applied = ops
            .config
            .applied(WEB_ACCESS_KEY)
            .map_err(|e| e.to_string())?;
        let Some(v) = applied else {
            let v = serde_json::to_value(WebAccessConfig::default()).map_err(|e| e.to_string())?;
            ops.config
                .apply_value(
                    "rynas",
                    WEB_ACCESS_KEY,
                    Some(v),
                    "web access on by default",
                    log,
                )
                .map_err(|e| e.to_string())?;
            return Ok(());
        };
        let cfg = config_of(Some(&v));
        if cfg.settings.mode == ProxyMode::External {
            self.publish_ui(&[]);
            return Ok(());
        }
        let facts = self.facts();
        let fp = facts.fingerprint();
        let known = self
            .engine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .facts
            .clone();
        if known.is_some_and(|k| k != fp) {
            log(
                "web access: the host name or the networks changed, writing the configuration again",
            );
            ops.config
                .drift_overwrite(WEB_ACCESS_KEY, log)
                .map_err(|e| e.to_string())?;
            return Ok(());
        }
        if self.push(&cfg, &facts, log)? {
            log("web access: the running proxy configuration differed and was loaded again");
        }
        self.note(|e| e.facts = Some(fp));
        self.retry_records(&cfg, &facts);
        self.check_certificates(false);
        Ok(())
    }

    /// `GET /web-access`.
    pub fn state(&self, owner: &str) -> Res<WebAccessState> {
        let ops = self.ops.get().ok_or_else(unavailable)?;
        let (eff, cur, _): (WebAccessConfig, WebAccessConfig, Option<WebAccessConfig>) =
            crate::services::docs(&ops.config, owner, WEB_ACCESS_KEY)?;
        let facts = self.facts();
        let r = resolve(&cur, &facts);
        let port = cur.settings.https_port;
        let names = self.names.report();
        let domains_view = self.domain_views(&cur, &facts);
        let routes = r
            .routes
            .iter()
            .map(|x| RouteView {
                id: x.route.id.clone(),
                kind: x.route.kind,
                stack: x.route.stack.clone(),
                app: x.route.app.clone(),
                urls: {
                    let mut u: Vec<String> = if x.route.main_port {
                        x.names
                            .iter()
                            .map(|n| url_of(n, port, &x.route.path))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let path = x.route.prefix.as_deref().map_or_else(
                        || x.route.path.clone(),
                        |p| format!("{p}/"),
                    );
                    if x.route.enabled {
                        for p in &x.route.ports {
                            u.extend(x.port_names().iter().map(|n| url_of(n, *p, &path)));
                        }
                    }
                    u
                },
                ports: x.route.ports.clone(),
                main_port: x.route.main_port,
                names: x.names.clone(),
                upstream: format!(
                    "{}://{}",
                    match x.route.upstream.scheme {
                        UpstreamScheme::Http => "http",
                        UpstreamScheme::Https => "https",
                    },
                    caddy_dial(&x.route.upstream.host, x.route.upstream.port)
                ),
                access: x.route.access,
                enabled: x.route.enabled,
                problems: {
                    let mut p = x.problems.clone();
                    p.extend(
                        x.names
                            .iter()
                            .filter_map(|n| names.problem(n))
                            .map(String::from),
                    );
                    p.dedup();
                    p
                },
            })
            .collect();
        let reserved = {
            let mut v: Vec<u16> = if cur.settings.mode == ProxyMode::External {
                Vec::new()
            } else {
                vec![cur.settings.http_port, cur.settings.https_port]
            };
            v.extend(model::extra_ports(&cur));
            v.dedup();
            v
        };
        Ok(WebAccessState {
            pending: (eff != cur).then(|| domains::for_answer(eff)),
            domains: domains_view,
            engine: self.engine_state(),
            host: r.host.clone(),
            ui_urls: r.ui.iter().map(|n| url_of(n, port, "/")).collect(),
            ui_names: r.ui.clone(),
            routes,
            lan: model::access_ranges(AccessLevel::Lan, &facts, &[]),
            vpn: model::vpn_ranges(&facts),
            confirm: self.pending_confirm(),
            emergency_port: self.emergency_port,
            reserved_ports: reserved,
            applied: domains::for_answer(cur),
        })
    }

    /// `POST /web-access/plan`.
    pub fn plan(&self, cfg: &WebAccessConfig, previous: &WebAccessConfig) -> WebAccessPlan {
        let facts = self.facts();
        let (resolved, json) = self.render_redacted(cfg, &facts);
        WebAccessPlan {
            engine_config: json,
            avahi_hosts: avahi_block(cfg, &facts),
            names: resolved.all_names(),
            ui_route_changes: ui_fingerprint(cfg) != ui_fingerprint(previous)
                && !resolved.ui.is_empty(),
            dns_changes: self.plan_records(cfg, previous, &facts),
        }
    }

    /// `POST /web-access/routes/{id}/test`.
    pub fn diagnose(&self, id: &str) -> Res<RouteDiagnostics> {
        let cfg = self.applied();
        let facts = self.facts();
        let resolved = resolve(&cfg, &facts);
        let r = resolved
            .routes
            .iter()
            .find(|r| r.route.id == id)
            .ok_or_else(|| Error::not_found(format!("no route {id}")))?;
        let mut checks = Vec::new();
        let name = r.local.clone();
        // 0. the name is published through Avahi
        checks.push(match &name {
            None => check(
                "local_name",
                CheckState::Skipped,
                format!("the route has no name ({})", r.problems.join(", ")),
            ),
            Some(n) => local_name_check(n, &self.names.report()),
        });
        let name = name
            .or_else(|| r.names.first().cloned())
            .or_else(|| r.nas.first().cloned());
        // 1. the name resolves to this NAS
        checks.push(match &name {
            None => check(
                "dns",
                CheckState::Skipped,
                format!("the route has no name ({})", r.problems.join(", ")),
            ),
            Some(n) => match self.probe.resolve(n) {
                Ok(addrs) if addrs.iter().any(|a| facts.addrs.contains(a)) => check(
                    "dns",
                    CheckState::Ok,
                    format!("{n} → {}", join(&addrs)),
                ),
                Ok(addrs) if addrs.iter().any(IpAddr::is_loopback) || addrs.is_empty() => check(
                    "dns",
                    CheckState::Warn,
                    format!("{n} → {} (only this NAS sees that)", join(&addrs)),
                ),
                Ok(addrs) => check(
                    "dns",
                    CheckState::Fail,
                    format!("{n} → {}, not an address of this NAS (another device uses the name?)", join(&addrs)),
                ),
                Err(e) => check(
                    "dns",
                    CheckState::Fail,
                    format!("{n} does not resolve on the NAS: {e} (Avahi running? nss-mdns on the clients?)"),
                ),
            },
        });
        // 2. the engine runs
        let running = if cfg.settings.mode == ProxyMode::External {
            checks.push(check(
                "proxy_running",
                CheckState::Fail,
                "external proxy mode: rynas-caddy is stopped",
            ));
            None
        } else if !self.installed() {
            checks.push(check(
                "proxy_running",
                CheckState::Fail,
                "rynas-caddy is not installed",
            ));
            None
        } else {
            match self.admin.config() {
                Ok(v) => {
                    checks.push(check(
                        "proxy_running",
                        CheckState::Ok,
                        "rynas-caddy answers on its admin socket",
                    ));
                    Some(v)
                }
                Err(e) => {
                    checks.push(check("proxy_running", CheckState::Fail, e));
                    None
                }
            }
        };
        // 3. the route is loaded (on the main port, and on each extra port)
        let mut want: Vec<(String, String)> = Vec::new();
        if r.route.main_port {
            want.push(("rynas_https".into(), format!("rynas-route-{id}")));
        }
        for p in &r.route.ports {
            want.push((format!("rynas_port_{p}"), format!("rynas-route-{id}:{p}")));
        }
        checks.push(match &running {
            None => check("route_loaded", CheckState::Skipped, ""),
            Some(v) => {
                let found = want.iter().all(|(server, want_id)| {
                    v["apps"]["http"]["servers"][server]["routes"]
                        .as_array()
                        .is_some_and(|rs| rs.iter().any(|x| x["@id"] == want_id.as_str()))
                });
                if found {
                    check(
                        "route_loaded",
                        CheckState::Ok,
                        "the route is in the running configuration",
                    )
                } else {
                    check(
                        "route_loaded",
                        CheckState::Fail,
                        "the route is not in the running configuration (apply again, or Sync)",
                    )
                }
            }
        });
        // 4. the HTTPS port listens
        let https = SocketAddr::from(([127, 0, 0, 1], cfg.settings.https_port));
        checks.push(if running.is_none() {
            check("https_listening", CheckState::Skipped, "")
        } else if !r.route.main_port {
            check(
                "https_listening",
                CheckState::Skipped,
                "the address is only on its extra ports",
            )
        } else {
            match self.probe.connect(https) {
                Ok(()) => check(
                    "https_listening",
                    CheckState::Ok,
                    format!("port {} answers", cfg.settings.https_port),
                ),
                Err(e) => check(
                    "https_listening",
                    CheckState::Fail,
                    format!("port {}: {e}", cfg.settings.https_port),
                ),
            }
        });
        // 4b. each extra port listens
        for p in &r.route.ports {
            let a = SocketAddr::from(([127, 0, 0, 1], *p));
            checks.push(if running.is_none() {
                check("port_listening", CheckState::Skipped, format!("port {p}"))
            } else {
                match self.probe.connect(a) {
                    Ok(()) => check(
                        "port_listening",
                        CheckState::Ok,
                        format!("port {p} answers (the router may forward it to this NAS)"),
                    ),
                    Err(e) => check("port_listening", CheckState::Fail, format!("port {p}: {e}")),
                }
            });
        }
        // 5. the certificate covers the name
        checks.push(match &name {
            None => check("certificate", CheckState::Skipped, ""),
            Some(n) => match std::fs::read_to_string(self.path(CERT))
                .ok()
                .and_then(|p| rynas_tls::inspect(&p).ok())
            {
                None => check("certificate", CheckState::Fail, "no certificate yet (apply the settings)"),
                Some(chain) => {
                    let want = Names {
                        dns: [n.clone()].into(),
                        ips: BTreeSet::new(),
                    };
                    let now = rynas_tls::now();
                    if chain[0].not_after_ms <= now {
                        check("certificate", CheckState::Fail, "the certificate has expired")
                    } else if !rynas_tls::missing_names(&chain[0], &want).is_empty() {
                        check("certificate", CheckState::Fail, format!("the certificate does not cover {n}"))
                    } else {
                        check(
                            "certificate",
                            CheckState::Ok,
                            format!("issued by the rynas local CA for {n} (browsers need the CA trusted: Trust this NAS)"),
                        )
                    }
                }
            },
        });
        // 6. the upstream answers (a host name of a custom route is resolved first)
        let uh = &r.route.upstream.host;
        let up: Result<SocketAddr, String> = match uh.parse::<IpAddr>() {
            Ok(ip) => Ok(SocketAddr::new(ip, r.route.upstream.port)),
            Err(_) => match self.probe.resolve(uh) {
                Ok(a) if !a.is_empty() => Ok(SocketAddr::new(a[0], r.route.upstream.port)),
                Ok(_) => Err(format!("{uh} has no address")),
                Err(e) => Err(format!("{uh} does not resolve on the NAS: {e}")),
            },
        };
        let hint = if r.route.kind == RouteKind::Custom {
            "is the device on, and is the port right?"
        } else {
            "is the app running?"
        };
        checks.push(match up {
            Err(e) => check("upstream", CheckState::Fail, e),
            Ok(a) => match self.probe.connect(a) {
                Ok(()) => check("upstream", CheckState::Ok, format!("{a} answers")),
                Err(e) => check("upstream", CheckState::Fail, format!("{a}: {e} ({hint})")),
            },
        });
        // 7. own-domain names: they resolve to this NAS, and their ACME certificate
        if let Some(n) = r.domain_names.first() {
            checks.push(match self.probe.resolve(n) {
                Ok(a) if a.iter().any(|x| facts.addrs.contains(x)) => {
                    check("domain_dns", CheckState::Ok, format!("{n} → {}", join(&a)))
                }
                Ok(a) => check(
                    "domain_dns",
                    CheckState::Fail,
                    format!(
                        "{n} → {}, not an address of this NAS (run the domain's checklist)",
                        join(&a)
                    ),
                ),
                Err(e) => check(
                    "domain_dns",
                    CheckState::Fail,
                    format!("{n} does not resolve from the NAS: {e} (run the domain's checklist)"),
                ),
            });
            let inv = self.certificates();
            let want = Names {
                dns: [n.clone()].into(),
                ips: BTreeSet::new(),
            };
            let covering = inv.certificates.iter().find(|e| {
                e.kind == CertKind::Acme
                    && e.certificate
                        .as_ref()
                        .is_some_and(|c| rynas_tls::missing_names(c, &want).is_empty())
            });
            checks.push(match covering {
                Some(e) if e.status == CertStatus::Expired => check(
                    "domain_certificate",
                    CheckState::Fail,
                    "the certificate has expired",
                ),
                Some(e) if e.staging => check(
                    "domain_certificate",
                    CheckState::Warn,
                    format!("{n}: Let's Encrypt staging certificate (not trusted)"),
                ),
                Some(_) => check(
                    "domain_certificate",
                    CheckState::Ok,
                    format!("{n}: a publicly trusted certificate"),
                ),
                None => check(
                    "domain_certificate",
                    CheckState::Warn,
                    format!("no certificate covers {n} yet (see the certificate dashboard)"),
                ),
            });
        }
        Ok(RouteDiagnostics {
            route: id.into(),
            checks,
            at_ms: now_ms(),
        })
    }

    /// The URL of a stack's route in the applied key.
    pub fn stack_url(&self, stack: &str) -> Option<String> {
        let cfg = self.applied();
        let facts = self.facts();
        let r = resolve(&cfg, &facts);
        r.routes
            .iter()
            .find(|x| x.route.stack.as_deref() == Some(stack))
            .and_then(|x| {
                // a trusted own-domain name before the local one
                x.domain_names
                    .first()
                    .or(x.names.first())
                    .map(|n| url_of(n, cfg.settings.https_port, &x.route.path))
            })
    }

    /// The URL a new route for `stack` would get.
    pub fn preview_url(&self, stack: &str, path: &str) -> Option<String> {
        let cfg = self.applied();
        if let Some(u) = self.stack_url(stack) {
            return Some(u);
        }
        if cfg.settings.mode == ProxyMode::External || !cfg.settings.local_names {
            return None;
        }
        let taken: BTreeSet<String> = cfg.routes.iter().map(|r| r.label.clone()).collect();
        let label = model::suggest_label(stack, &taken);
        let host = self.facts().host();
        model::local_name(&label, &host).map(|n| url_of(&n, cfg.settings.https_port, path))
    }

    /// Adds (or updates the upstream of) the route of an app stack and applies the key.
    /// Blocking. Returns the route's URL when it has a name.
    pub fn add_app_route(
        &self,
        author: &str,
        spec: &AppRouteSpec,
        log: &dyn Fn(&str),
    ) -> Result<Option<String>, String> {
        let ops = self
            .ops
            .get()
            .ok_or("web access is not enabled in this rynasd")?;
        ops.config
            .apply_update(
                author,
                WEB_ACCESS_KEY,
                &format!("web access route for {}", spec.stack),
                log,
                |applied| {
                    let mut c = config_of(applied.as_ref());
                    if let Some(r) = c
                        .routes
                        .iter_mut()
                        .find(|r| r.stack.as_deref() == Some(spec.stack.as_str()))
                    {
                        if r.upstream.port == spec.port && r.enabled {
                            return Ok(None);
                        }
                        r.upstream.port = spec.port;
                        r.enabled = true;
                    } else {
                        let labels: BTreeSet<String> =
                            c.routes.iter().map(|r| r.label.clone()).collect();
                        let ids: BTreeSet<String> = c.routes.iter().map(|r| r.id.clone()).collect();
                        c.routes.push(WebRoute {
                            id: model::suggest_id(&spec.stack, &ids),
                            kind: RouteKind::App,
                            stack: Some(spec.stack.clone()),
                            app: Some(spec.app.clone()),
                            label: model::suggest_label(&spec.stack, &labels),
                            upstream: WebUpstream {
                                scheme: spec.scheme,
                                host: "127.0.0.1".into(),
                                port: spec.port,
                                ..Default::default()
                            },
                            auth: spec.auth,
                            limits: RouteLimits {
                                body: spec.body.clone(),
                                timeout: spec.timeout.clone(),
                                buffering: spec.buffering,
                            },
                            websocket: spec.websocket,
                            subpath: spec.subpath,
                            path: if spec.path.is_empty() {
                                "/".into()
                            } else {
                                spec.path.clone()
                            },
                            ..Default::default()
                        });
                    }
                    serde_json::to_value(&c)
                        .map(Some)
                        .map_err(|e| e.to_string())
                },
            )
            .map_err(|e| e.to_string())?;
        Ok(self.stack_url(&spec.stack))
    }

    /// Removes the routes of a stack (uninstall). Blocking.
    pub fn remove_stack_routes(
        &self,
        author: &str,
        stack: &str,
        log: &dyn Fn(&str),
    ) -> Result<bool, String> {
        let ops = self
            .ops
            .get()
            .ok_or("web access is not enabled in this rynasd")?;
        ops.config
            .apply_update(
                author,
                WEB_ACCESS_KEY,
                &format!("web access route of {stack} removed"),
                log,
                |applied| {
                    let mut c = config_of(applied.as_ref());
                    let before = c.routes.len();
                    c.routes.retain(|r| r.stack.as_deref() != Some(stack));
                    if c.routes.len() == before {
                        return Ok(None);
                    }
                    serde_json::to_value(&c)
                        .map(Some)
                        .map_err(|e| e.to_string())
                },
            )
            .map(|r| r.is_some())
            .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// wave W2: records, domain checklist, certificate dashboard
// ---------------------------------------------------------------------------------------------

impl WebAccessSvc {
    fn dns_lock(&self) -> std::sync::MutexGuard<'_, domains::DnsMem> {
        let mut m = self.dns_mem.lock().unwrap_or_else(|e| e.into_inner());
        if !m.loaded {
            m.owned = domains::load_owned(&self.state_dir);
            m.loaded = true;
        }
        m
    }

    /// The record client of a domain behind the read cache, or why there is none. `write`:
    /// a sync that writes, whose record reads skip the cache.
    fn dns_client(
        &self,
        cfg: &WebAccessConfig,
        d: &WebDomain,
        write: bool,
    ) -> Result<Box<dyn rynas_dns::DnsApi>, String> {
        use std::hash::{Hash, Hasher};
        let p = d.provider.ok_or("the domain has no DNS provider")?;
        let cr = domains::credential_of(cfg, d).ok_or("the domain has no credential")?;
        let creds = domains::open_creds(cr, self.sb())?;
        let client = (self.dns)(p, &creds);
        domains::wipe(creds);
        // the envelope changes with the values: a new token never sees the old one's reads
        let mut h = std::collections::hash_map::DefaultHasher::new();
        cr.sealed.hash(&mut h);
        let key = format!("{}#{}#{:x}", p.as_str(), cr.id, h.finish());
        match client {
            Ok(Some(c)) => Ok(self.dns_cache.wrap(key, c, write)),
            Ok(None) => Err(format!("rynas has no record client for {}", p.as_str())),
            Err(e) => Err(e.to_string()),
        }
    }

    /// What a record sync of `cfg` would change (reads the providers, never writes):
    /// the LAN records of each domain, the manual ones, and the deletes of record sets rynas
    /// wrote for domains that no longer want them (`previous` holds their credentials).
    pub fn plan_records(
        &self,
        cfg: &WebAccessConfig,
        previous: &WebAccessConfig,
        facts: &HostFacts,
    ) -> Vec<DnsChange> {
        let owned = self.dns_lock().owned.clone();
        let mut out = Vec::new();
        for d in &cfg.domains {
            out.extend(self.domain_changes(cfg, d, facts, &owned, false).changes);
        }
        for (id, o) in &owned {
            if cfg.domains.iter().any(|d| &d.id == id) {
                continue;
            }
            out.extend(self.removal_changes(cfg, previous, id, o, false).changes);
        }
        out
    }

    /// One domain: plan (and with `write`, apply) its records.
    fn domain_changes(
        &self,
        cfg: &WebAccessConfig,
        d: &WebDomain,
        facts: &HostFacts,
        owned: &BTreeMap<String, domains::Owned>,
        write: bool,
    ) -> DnsSyncReport {
        let mut report = DnsSyncReport {
            domain: d.name.clone(),
            changes: Vec::new(),
            ok: true,
            at_ms: now_ms(),
        };
        let mine = owned.get(&d.id);
        let wanted = match domains::record_mode(d) {
            domains::RecordMode::Off => Vec::new(),
            _ => domains::wanted_records(cfg, d, facts),
        };
        if let domains::RecordMode::Manual(why) = domains::record_mode(d) {
            report.changes = rynas_dns::plan::manual(&d.name, &wanted, &why);
            report.changes.extend(domains::delegation_record(d));
            return report;
        }
        if d.challenge == ChallengeKind::CnameDelegation {
            report.changes.extend(domains::delegation_record(d));
        }
        // a provider change: the old provider's sets are removed through the old credential
        let same_provider = mine.is_none_or(|m| m.provider == d.provider);
        let stale: Vec<rynas_dns::plan::Wanted> = mine
            .filter(|_| same_provider)
            .map(|m| m.sets())
            .unwrap_or_default();
        if wanted.is_empty() && stale.is_empty() {
            if domains::record_mode(d) != domains::RecordMode::Off && facts.primary_v4.is_none() {
                report.ok = false;
                report.changes.push(DnsChange {
                    domain: d.name.clone(),
                    name: format!("*.{}", d.name),
                    rtype: "A".into(),
                    action: DnsAction::Unknown,
                    detail: "this NAS has no IPv4 address on the LAN".into(),
                    ..Default::default()
                });
            }
            return report;
        }
        let api = match self.dns_client(cfg, d, write) {
            Ok(a) => a,
            Err(e) => {
                report.ok = false;
                report
                    .changes
                    .extend(rynas_dns::plan::unknown(&d.name, &wanted, &stale, &e));
                return report;
            }
        };
        match rynas_dns::plan::plan(&d.name, api.as_ref(), &wanted, &stale, &d.take_over) {
            Ok((zone, mut changes)) => {
                if write {
                    report.ok = rynas_dns::plan::apply(api.as_ref(), &zone, &mut changes);
                } else {
                    report.ok = !changes.iter().any(|c| c.action == DnsAction::Unknown);
                }
                report.changes.extend(changes);
            }
            Err(e) => {
                report.ok = false;
                report.changes.extend(rynas_dns::plan::unknown(
                    &d.name,
                    &wanted,
                    &stale,
                    &e.to_string(),
                ));
            }
        }
        report
    }

    /// Deletes (or plans the deletes of) every record set rynas wrote for a domain id that is
    /// gone or changed provider, through the credential it was written with.
    fn removal_changes(
        &self,
        cfg: &WebAccessConfig,
        previous: &WebAccessConfig,
        id: &str,
        o: &domains::Owned,
        write: bool,
    ) -> DnsSyncReport {
        let mut report = DnsSyncReport {
            domain: o.name.clone(),
            changes: Vec::new(),
            ok: true,
            at_ms: now_ms(),
        };
        let stale = o.sets();
        if stale.is_empty() {
            return report;
        }
        // the domain as it was written: provider and credential (from either value)
        let src = WebDomain {
            id: id.into(),
            name: o.name.clone(),
            provider: o.provider,
            credential: o.credential.clone(),
            ..Default::default()
        };
        let with_cred = [cfg, previous].into_iter().find(|c| {
            domains::credential_of(c, &src).is_some_and(|cr| Some(cr.provider) == o.provider)
        });
        let api = match with_cred {
            Some(c) => self.dns_client(c, &src, write),
            None => Err(format!(
                "the credential {} is gone: delete these records by hand",
                o.credential.as_deref().unwrap_or("?")
            )),
        };
        let api = match api {
            Ok(a) => a,
            Err(e) => {
                report.ok = false;
                report.changes = rynas_dns::plan::unknown(&o.name, &[], &stale, &e);
                return report;
            }
        };
        match rynas_dns::plan::plan(&o.name, api.as_ref(), &[], &stale, &[]) {
            Ok((zone, mut changes)) => {
                report.ok = if write {
                    rynas_dns::plan::apply(api.as_ref(), &zone, &mut changes)
                } else {
                    true
                };
                report.changes = changes;
            }
            Err(e) => {
                report.ok = false;
                report.changes = rynas_dns::plan::unknown(&o.name, &[], &stale, &e.to_string());
            }
        }
        report
    }

    /// Writes the LAN records of `to` (only the domain `only`, when given), deletes the sets
    /// no longer wanted (credentials of removed domains come from `from`), remembers what
    /// was written. Best effort: failures are logged and reported in the domain's view, and
    /// tried again after [`domains::RECORDS_RETRY`] or on the next apply.
    pub fn sync_records(
        &self,
        from: &WebAccessConfig,
        to: &WebAccessConfig,
        facts: &HostFacts,
        only: Option<&str>,
        log: &dyn Fn(&str),
    ) -> Vec<DnsSyncReport> {
        // one sync at a time; the state lock is taken only to copy and to merge, never
        // across a provider call (review 2026-11-07, M1). Only syncs change `owned`.
        let _sync = self.dns_sync.lock().unwrap_or_else(|e| e.into_inner());
        let mut owned = self.dns_lock().owned.clone();
        let before = owned.clone();
        let mut reports = Vec::new();
        let mut kept: Vec<(String, DnsSyncReport)> = Vec::new();
        let mut forget: Vec<String> = Vec::new();
        for d in to.domains.iter().filter(|d| only.is_none_or(|o| o == d.id)) {
            // a provider change: the old sets go first
            if let Some(o) = owned.get(&d.id).cloned()
                && o.provider != d.provider
                && domains::record_mode(d) != domains::RecordMode::Off
            {
                let r = self.removal_changes(to, from, &d.id, &o, true);
                if r.ok {
                    owned.remove(&d.id);
                }
            }
            let r = self.domain_changes(to, d, facts, &owned, true);
            for c in &r.changes {
                let verb = match c.action {
                    DnsAction::Create => "create",
                    DnsAction::Update => "update",
                    DnsAction::Delete => "delete",
                    DnsAction::Conflict => "conflict (left as it is)",
                    _ => continue,
                };
                let what = if c.detail.starts_with("failed") {
                    format!(" {}", c.detail)
                } else {
                    String::new()
                };
                log(&format!(
                    "web access: DNS {verb} {} {} {}{what}",
                    c.name,
                    c.rtype,
                    c.wanted.join(" ")
                ));
            }
            if !r.ok {
                log(&format!(
                    "web access: the DNS records of {} could not all be written (tried again later)",
                    d.name
                ));
            }
            let rec = domains::owned_after(owned.get(&d.id), &r.changes);
            if rec.is_empty() {
                owned.remove(&d.id);
            } else {
                owned.insert(
                    d.id.clone(),
                    domains::Owned {
                        name: d.name.clone(),
                        provider: d.provider,
                        credential: d.credential.clone(),
                        records: rec,
                    },
                );
            }
            kept.push((d.id.clone(), r.clone()));
            reports.push(r);
        }
        if only.is_none() {
            let gone: Vec<(String, domains::Owned)> = owned
                .iter()
                .filter(|(id, _)| !to.domains.iter().any(|d| &d.id == *id))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (id, o) in gone {
                let r = self.removal_changes(to, from, &id, &o, true);
                for c in r.changes.iter() {
                    match c.action {
                        DnsAction::Delete => {
                            log(&format!("web access: DNS delete {} {}", c.name, c.rtype))
                        }
                        DnsAction::Conflict => log(&format!(
                            "web access: DNS {} {} left in place: {}",
                            c.name, c.rtype, c.detail
                        )),
                        _ => {}
                    }
                }
                let rest = domains::owned_after(Some(&o), &r.changes);
                if r.ok || rest.is_empty() {
                    owned.remove(&id);
                    forget.push(id);
                } else {
                    owned.insert(
                        id,
                        domains::Owned {
                            records: rest,
                            ..o.clone()
                        },
                    );
                    log(&format!(
                        "web access: the old DNS records of {} could not be deleted",
                        o.name
                    ));
                }
                reports.push(r);
            }
        }
        let save = {
            let mut m = self.dns_lock();
            let now = Instant::now();
            for (id, r) in kept {
                m.tried.insert(id.clone(), now);
                m.reports.insert(id, r);
            }
            for id in forget {
                m.reports.remove(&id);
            }
            m.owned = owned;
            (m.owned != before).then(|| m.owned.clone())
        };
        if let Some(owned) = save
            && let Err(e) = domains::save_owned(&self.state_dir, &owned)
        {
            tracing::warn!(
                "web access: {}: {e}",
                domains::owned_file(&self.state_dir).display()
            );
        }
        reports
    }

    /// Domains whose last record sync failed, tried again after [`domains::RECORDS_RETRY`].
    fn retry_records(&self, cfg: &WebAccessConfig, facts: &HostFacts) {
        let due: Vec<String> = {
            let m = self.dns_lock();
            m.reports
                .iter()
                .filter(|(id, r)| {
                    !r.ok
                        && m.tried
                            .get(*id)
                            .is_none_or(|t| t.elapsed() >= domains::RECORDS_RETRY)
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in due {
            self.sync_records(cfg, cfg, facts, Some(&id), &|l| tracing::info!("{l}"));
        }
    }

    /// The domains as the pages show them.
    pub fn domain_views(&self, cfg: &WebAccessConfig, facts: &HostFacts) -> Vec<DomainView> {
        let reports = self.dns_lock().reports.clone();
        let port = cfg.settings.https_port;
        cfg.domains
            .iter()
            .map(|d| {
                let mut problems = Vec::new();
                let cred = domains::credential_of(cfg, d);
                if d.provider.is_some()
                    && cred.is_none_or(|c| !domains::creds_open(c, self.sb()))
                {
                    problems.push("credential_missing".to_string());
                }
                let rec = reports.get(&d.id).cloned();
                match domains::record_mode(d) {
                    domains::RecordMode::Manual(_) => problems.push("records_manual".into()),
                    domains::RecordMode::Managed(_) if rec.as_ref().is_some_and(|r| !r.ok) => {
                        problems.push("records_failed".into())
                    }
                    _ => {}
                }
                if rec.as_ref().is_some_and(|r| {
                    r.changes
                        .iter()
                        .any(|c| c.action == DnsAction::Conflict && !c.wanted.is_empty())
                }) {
                    problems.push("records_conflict".into());
                }
                if d.lan_record && facts.primary_v4.is_none() {
                    problems.push("no_lan_address".into());
                }
                if cfg.acme.staging {
                    problems.push("staging".into());
                }
                let ui_on = d.enabled
                    && d.ui
                    && cfg.settings.ui.enabled
                    && cfg.settings.mode == ProxyMode::Builtin;
                DomainView {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    enabled: d.enabled,
                    certificate_names: domains::cert_subjects(cfg, d),
                    ui_urls: if ui_on {
                        vec![url_of(&d.name, port, "/")]
                    } else {
                        Vec::new()
                    },
                    records: rec,
                    problems,
                }
            })
            .collect()
    }

    /// The certificate of `subject` in the engine's storage, if any.
    fn acme_cert(
        &self,
        cfg: &WebAccessConfig,
        subject: &str,
    ) -> Option<rynas_api_types::tls::TlsCertificate> {
        let p = domains::cert_path(&self.paths.storage, domains::acme_ca(cfg), subject);
        let pem = std::fs::read_to_string(self.path(&p)).ok()?;
        rynas_tls::inspect(&pem).ok()?.into_iter().next()
    }

    /// `GET /web-access/certificates`.
    pub fn certificates(&self) -> CertificateInventory {
        let now = rynas_tls::now();
        let mut out = Vec::new();
        if let Some(tls) = self.tls.get()
            && let Ok(st) = tls.status()
        {
            if let Some(ca) = st.local_ca.clone() {
                out.push(certs::entry("local-ca", CertKind::LocalCa, ca, false, now));
            }
            let mut e = certs::entry("interface", CertKind::Interface, st.certificate, false, now);
            e.source = Some(
                match st.source {
                    rynas_api_types::tls::TlsSource::Local => "local",
                    rynas_api_types::tls::TlsSource::Imported => "imported",
                }
                .into(),
            );
            out.push(e);
        }
        if let Some(c) = std::fs::read_to_string(self.path(CERT))
            .ok()
            .and_then(|p| rynas_tls::inspect(&p).ok())
            .and_then(|v| v.into_iter().next())
        {
            out.push(certs::entry(
                "proxy-local",
                CertKind::ProxyLocal,
                c,
                false,
                now,
            ));
        }
        let cfg = self.applied();
        let cached = {
            let cm = self.cert_mem.lock().unwrap_or_else(|e| e.into_inner());
            cm.log_errors
                .as_ref()
                .filter(|(at, _)| at.elapsed() < self.engine_log_ttl)
                .map(|(_, e)| e.clone())
        };
        let errors = match cached {
            Some(e) => e,
            None => {
                let e = certs::engine_errors(&(self.engine_log)());
                let mut cm = self.cert_mem.lock().unwrap_or_else(|e| e.into_inner());
                cm.log_errors = Some((Instant::now(), e.clone()));
                e
            }
        };
        // imported certificates (§5.5): the last good pair of each; a watched folder still
        // waiting for its files is `pending`
        let active = self.active_imports(now);
        for m in self.import_index() {
            let id = format!("imported:{}", m.id);
            let mut e = match self.stored_leaf(&m.id).and_then(|v| v.into_iter().next()) {
                Some(c) => certs::entry(&id, CertKind::Imported, c, false, now),
                None => CertEntry {
                    id,
                    kind: CertKind::Imported,
                    status: CertStatus::Pending,
                    ..Default::default()
                },
            };
            e.source = Some(
                match m.source {
                    ImportSource::Upload => "upload",
                    ImportSource::Folder => "folder",
                }
                .into(),
            );
            e.last_error = m.last_error.clone();
            out.push(e);
        }
        let mut cm = self.cert_mem.lock().unwrap_or_else(|e| e.into_inner());
        let mut expected = BTreeSet::new();
        if cfg.settings.mode == ProxyMode::Builtin {
            for d in domains::active(&cfg) {
                // subjects an imported certificate replaces are not ordered
                for s in imported::acme_subjects(&cfg, d, &active) {
                    expected.insert(s.clone());
                    let id = format!("acme:{s}");
                    let mut e = match self.acme_cert(&cfg, &s) {
                        Some(c) => {
                            cm.expected_since.remove(&s);
                            certs::entry(&id, CertKind::Acme, c, true, now)
                        }
                        None => {
                            let since = *cm.expected_since.entry(s.clone()).or_insert(now);
                            CertEntry {
                                id,
                                kind: CertKind::Acme,
                                status: certs::missing_status(since, now),
                                names: vec![s.clone()],
                                ..Default::default()
                            }
                        }
                    };
                    e.domain = Some(d.name.clone());
                    e.staging = e.staging || cfg.acme.staging;
                    e.last_error = errors.get(&s).cloned();
                    out.push(e);
                }
            }
        }
        cm.expected_since.retain(|k, _| expected.contains(k));
        CertificateInventory {
            certificates: out,
            at_ms: now,
        }
    }

    /// The certificate check of the periodic loop: notifications on changes (at most every
    /// [`CERT_CHECK_INTERVAL`] unless `force`).
    pub fn check_certificates(&self, force: bool) -> Vec<rynas_api_types::notify::NewEvent> {
        {
            let mut cm = self.cert_mem.lock().unwrap_or_else(|e| e.into_inner());
            if !force
                && cm
                    .last_check
                    .is_some_and(|t| t.elapsed() < CERT_CHECK_INTERVAL)
            {
                return Vec::new();
            }
            cm.last_check = Some(Instant::now());
        }
        let inv = self.certificates();
        let events = self
            .cert_mem
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .notified
            .events(&inv.certificates);
        if let Some(n) = self.notifier.get() {
            for e in &events {
                n.emit(e.clone());
            }
        }
        events
    }

    /// `POST /web-access/domains/{id}/test`: the domain's checklist.
    pub fn diagnose_domain(&self, id: &str) -> Res<RouteDiagnostics> {
        let cfg = self.applied();
        let facts = self.facts();
        let d = cfg
            .domains
            .iter()
            .find(|d| d.id == id)
            .cloned()
            .ok_or_else(|| Error::not_found(format!("no domain {id}")))?;
        let expected = domains::expected_addrs(&d, &facts);
        let exp_text = join(&expected);
        // the name a client would ask: a route's, or a probe under the wildcard
        let probe_name = resolve(&cfg, &facts)
            .routes
            .iter()
            .find_map(|r| {
                r.domain_names
                    .iter()
                    .find(|n| n.ends_with(&d.name))
                    .cloned()
            })
            .unwrap_or_else(|| {
                if d.wildcard && d.provider != Some(DnsProvider::Duckdns) {
                    format!("rynas-check.{}", d.name)
                } else {
                    d.name.clone()
                }
            });
        let mut checks = Vec::new();
        // 1. the records at the provider
        let rec = self.dns_lock().reports.get(&d.id).cloned();
        checks.push(match (domains::record_mode(&d), rec) {
            (domains::RecordMode::Off, _) => check(
                "dns_record",
                CheckState::Skipped,
                "rynas does not write the records of this domain (LAN record off)",
            ),
            (domains::RecordMode::Manual(why), _) => {
                let mut list: Vec<String> = domains::wanted_records(&cfg, &d, &facts)
                    .iter()
                    .map(|w| format!("{} {} {}", w.name, w.rtype, w.values.join(" ")))
                    .collect();
                if let Some(c) = domains::delegation_record(&d) {
                    list.push(format!("{} CNAME {}", c.name, c.wanted.join(" ")));
                }
                check(
                    "dns_record",
                    CheckState::Warn,
                    format!("{why}: {}", list.join("; ")),
                )
            }
            (_, None) => check(
                "dns_record",
                CheckState::Warn,
                "the records have not been written yet (apply the settings, or Write records)",
            ),
            (_, Some(r)) if r.ok => check(
                "dns_record",
                CheckState::Ok,
                format!(
                    "{} at the provider",
                    r.changes
                        .iter()
                        .filter(|c| c.action != DnsAction::Delete && c.rtype != "CNAME")
                        .map(|c| format!("{} {} {}", c.name, c.rtype, c.wanted.join(" ")))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            ),
            (_, Some(r)) => check(
                "dns_record",
                CheckState::Fail,
                r.changes
                    .iter()
                    .filter(|c| !c.detail.is_empty())
                    .map(|c| format!("{} {}: {}", c.name, c.rtype, c.detail))
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
        });
        // 2. the NAS's own resolver
        checks.push(match self.probe.resolve(&probe_name) {
            Ok(a) if a.iter().any(|x| expected.contains(x)) => check(
                "dns_nas",
                CheckState::Ok,
                format!("{probe_name} → {} from the NAS", join(&a)),
            ),
            Ok(a) => check(
                "dns_nas",
                CheckState::Fail,
                format!(
                    "{probe_name} → {} from the NAS, expected {exp_text}",
                    join(&a)
                ),
            ),
            Err(e) => check(
                "dns_nas",
                CheckState::Fail,
                format!("{probe_name} does not resolve from the NAS: {e}"),
            ),
        });
        // 3. a public resolver
        let mut public: Result<rynas_dns::wire::Answer, String> = Err("no public resolver".into());
        for r in domains::PUBLIC_RESOLVERS {
            let Ok(addr) = r.parse::<SocketAddr>() else {
                continue;
            };
            public = self.probe.query(addr, &probe_name, rynas_dns::Rtype::A);
            if public.is_ok() {
                break;
            }
        }
        checks.push(match &public {
            Ok(a) if a.addrs().iter().any(|x| expected.contains(x)) => check(
                "dns_public",
                CheckState::Ok,
                format!("{probe_name} → {} from public DNS", a.describe()),
            ),
            Ok(a) => check(
                "dns_public",
                CheckState::Fail,
                format!(
                    "{probe_name} → {} from public DNS, expected {exp_text} (the record is missing or not propagated yet)",
                    a.describe()
                ),
            ),
            Err(e) => check(
                "dns_public",
                CheckState::Warn,
                format!("public DNS could not be asked: {e} (outbound DNS blocked?)"),
            ),
        });
        // 4. rebind protection: the LAN resolver against the public one
        let lan_servers = self.probe.lan_resolvers();
        let lan = lan_servers
            .iter()
            .map(|s| self.probe.query(*s, &probe_name, rynas_dns::Rtype::A))
            .find(|r| r.is_ok())
            .unwrap_or_else(|| Err("no LAN resolver answered".into()));
        use rynas_dns::rebind::Verdict;
        checks.push(match rynas_dns::rebind::verdict(&expected, &public, &lan) {
            Verdict::Ok => check(
                "rebind",
                CheckState::Ok,
                format!("the LAN resolver answers {exp_text} too"),
            ),
            Verdict::RebindBlocked => check(
                "rebind",
                CheckState::Fail,
                format!(
                    "public DNS answers {exp_text}, the LAN resolver ({}) drops it: {}",
                    lan.as_ref().map(|a| a.describe()).unwrap_or_default(),
                    rynas_dns::rebind::REBIND_FIX.replace("the domain", &d.name)
                ),
            ),
            Verdict::LanOverride(a) => check(
                "rebind",
                CheckState::Warn,
                format!(
                    "the LAN resolver answers {} instead of {exp_text}: a local DNS entry overrides the domain",
                    join(&a)
                ),
            ),
            Verdict::NotPublished | Verdict::Wrong(_) => check(
                "rebind",
                CheckState::Skipped,
                "the public record is not right yet",
            ),
            Verdict::PublicUnreachable(lan_ok) => check(
                "rebind",
                if lan_ok { CheckState::Ok } else { CheckState::Warn },
                if lan_ok {
                    format!("the LAN resolver answers {exp_text}")
                } else {
                    "public DNS could not be asked; the LAN resolver does not answer the LAN address".into()
                },
            ),
            Verdict::LanUnreachable => check(
                "rebind",
                CheckState::Warn,
                "the LAN resolver could not be asked or failed (SERVFAIL: check its upstream and DNSSEC)",
            ),
        });
        // 5. the certificate, 6. its renewal
        let inv = self.certificates();
        let mine: Vec<&CertEntry> = inv
            .certificates
            .iter()
            .filter(|e| e.kind == CertKind::Acme && e.domain.as_deref() == Some(&d.name))
            .collect();
        let worst = mine.iter().map(|e| e.status).max_by_key(|s| match s {
            CertStatus::Ok => 0,
            CertStatus::Pending => 1,
            CertStatus::ExpiresSoon => 2,
            CertStatus::RenewalOverdue => 3,
            CertStatus::Failed => 4,
            CertStatus::Expired => 5,
        });
        let errs: Vec<String> = mine.iter().filter_map(|e| e.last_error.clone()).collect();
        checks.push(if !d.enabled || cfg.settings.mode != ProxyMode::Builtin {
            check(
                "certificate",
                CheckState::Skipped,
                "the domain is not served",
            )
        } else {
            match worst {
                None => check("certificate", CheckState::Skipped, "no certificate names"),
                Some(CertStatus::Pending) => check(
                    "certificate",
                    CheckState::Warn,
                    "being obtained (DNS-01 takes a minute or two)",
                ),
                Some(CertStatus::Failed) => check(
                    "certificate",
                    CheckState::Fail,
                    format!("not obtained: {}", errs.join("; ")),
                ),
                Some(CertStatus::Expired) => check("certificate", CheckState::Fail, "expired"),
                Some(_) if cfg.acme.staging => check(
                    "certificate",
                    CheckState::Warn,
                    "issued by Let's Encrypt staging: browsers do not trust it",
                ),
                Some(_) => check(
                    "certificate",
                    CheckState::Ok,
                    format!(
                        "valid for {}",
                        mine.iter()
                            .flat_map(|e| e.names.iter().cloned())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
            }
        });
        checks.push(match worst {
            None | Some(CertStatus::Pending) | Some(CertStatus::Failed) => {
                check("renewal", CheckState::Skipped, "")
            }
            Some(CertStatus::RenewalOverdue) | Some(CertStatus::Expired) => check(
                "renewal",
                CheckState::Fail,
                format!("renewal is failing: {}", errs.join("; ")),
            ),
            Some(CertStatus::ExpiresSoon) => check(
                "renewal",
                CheckState::Warn,
                "expires within 14 days and has not been renewed yet",
            ),
            Some(CertStatus::Ok) => check(
                "renewal",
                CheckState::Ok,
                format!(
                    "renewed automatically from {}",
                    mine.iter()
                        .filter_map(|e| e.renew_after_ms)
                        .min()
                        .map(fmt_day)
                        .unwrap_or_default()
                ),
            ),
        });
        // 7. hairpin: a LAN address needs no NAT loopback
        checks.push(if expected.is_empty() {
            check("hairpin", CheckState::Skipped, "")
        } else if expected.iter().all(rynas_dns::rebind::is_private) {
            check(
                "hairpin",
                CheckState::Ok,
                "the name points to the LAN address: devices at home reach the NAS directly, no NAT loopback needed (away from home use Tailscale)",
            )
        } else {
            check(
                "hairpin",
                CheckState::Warn,
                "the name points to a public address: devices at home need NAT loopback (hairpin) on the router",
            )
        });
        Ok(RouteDiagnostics {
            route: id.into(),
            checks,
            at_ms: now_ms(),
        })
    }
}

/// `YYYY-MM-DD` (UTC) of a time in milliseconds.
fn fmt_day(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    // civil from days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

/// The `local_name` check of `name` from the publisher's report.
fn local_name_check(name: &str, report: &avahi::NamesReport) -> RouteCheck {
    use avahi::{Availability, NameState};
    match (&report.avahi, report.names.get(name)) {
        (Availability::Unknown, _) => check(
            "local_name",
            CheckState::Skipped,
            "this rynasd does not publish local names (development mode)",
        ),
        (Availability::Down(why), _) => check(
            "local_name",
            CheckState::Fail,
            format!(
                "Avahi is not reachable on the system bus ({why}): local names are not published; install and start avahi-daemon"
            ),
        ),
        (Availability::Starting, _) => check(
            "local_name",
            CheckState::Warn,
            "Avahi is registering its own host name; the name follows",
        ),
        (Availability::Up, None) => check(
            "local_name",
            CheckState::Warn,
            format!("{name} is being handed to Avahi"),
        ),
        (Availability::Up, Some((addr, st))) => match st {
            NameState::Established => check(
                "local_name",
                CheckState::Ok,
                format!("{name} → {addr}, published through Avahi"),
            ),
            NameState::Registering => check(
                "local_name",
                CheckState::Warn,
                format!("{name} → {addr}: Avahi is probing the name"),
            ),
            NameState::Collision(why) => check(
                "local_name",
                CheckState::Fail,
                format!(
                    "{name} is already used on the network ({why}); change the route's label, or Sync to try again"
                ),
            ),
            NameState::Failed(why) => check(
                "local_name",
                CheckState::Fail,
                format!("Avahi refused {name}: {why}"),
            ),
        },
    }
}

fn caddy_dial(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn join(a: &[IpAddr]) -> String {
    if a.is_empty() {
        return "no address".into();
    }
    a.iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------------------------
// the configuration domain
// ---------------------------------------------------------------------------------------------

/// The `web_access` key.
pub struct WebAccessDomain {
    pub svc: Arc<WebAccessSvc>,
}

impl Applier for WebAccessDomain {
    fn key(&self) -> &str {
        WEB_ACCESS_KEY
    }

    /// After the services and Docker keys.
    fn order(&self) -> i32 {
        60
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let mut c: WebAccessConfig = serde_json::from_value(value)
            .map_err(|e| Invalid::from(format!("invalid web_access document: {e}")))?;
        // values given here are sealed; an envelope already in the document is kept (rynasd's
        // own updates, a restore); a credential with neither takes the applied one
        let applied = self.svc.applied();
        domains::seal_credentials(&mut c, &[&applied], self.svc.sb())?;
        let c = model::validate(c, &[self.svc.ui_port, self.svc.emergency_port])?;
        // extra ports another program listens on (the proxy's own ones excepted)
        let held: BTreeSet<u16> = if self.svc.installed() {
            model::extra_ports(&applied)
        } else {
            BTreeSet::new()
        };
        model::check_busy_ports(&c, &self.svc.listening_ports(), &held)?;
        serde_json::to_value(c).map_err(|e| Invalid::from(e.to_string()))
    }

    /// No files: local names go through Avahi's D-Bus API, the engine through its socket.
    fn render(&self, _value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        Ok(Vec::new())
    }

    fn reconcile(
        &self,
        from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        self.svc.migrate_avahi_hosts(ctx.system, ctx.log);
        let facts = self.svc.facts();
        self.svc.publish_names(&config_of(to), &facts);
        self.svc.engine_apply(from, to, ctx.system, ctx.log)?;
        // the LAN records (best effort: a provider error never rolls the proxy back)
        let (old, new) = (config_of(from), config_of(to));
        if !new.domains.is_empty() || !old.domains.is_empty() {
            self.svc.sync_records(&old, &new, &facts, None, ctx.log);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// periodic check
// ---------------------------------------------------------------------------------------------

/// At start (after a few seconds) and every [`WATCH_INTERVAL`]: [`WebAccessSvc::sync`].
pub fn spawn_watch(svc: Arc<WebAccessSvc>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        loop {
            let s = svc.clone();
            match tokio::task::spawn_blocking(move || s.sync(&|l| tracing::info!("{l}"))).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("web access: {e}"),
                Err(e) => tracing::error!("web access: {e}"),
            }
            tokio::time::sleep(WATCH_INTERVAL).await;
        }
    });
}

// ---------------------------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------------------------

fn unavailable() -> Error {
    Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        error_codes::BACKEND_UNAVAILABLE,
        "web access is not enabled in this rynasd".into(),
    )
}

fn wsvc(st: &WaState) -> Res<(Arc<WebAccessSvc>, Arc<Ops>)> {
    let s = st.clone().ok_or_else(unavailable)?;
    let ops = s.ops.get().cloned().ok_or_else(unavailable)?;
    Ok((s, ops))
}

fn admin_user(ops: &Ops, hd: &HeaderMap, what: &str) -> Res<String> {
    let who = user(hd)?;
    ops.admins.require_admin(&who, what)?;
    Ok(who)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Res<T> + Send + 'static) -> Res<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(Error::internal)?
}

async fn state_get(State(st): State<WaState>, hd: HeaderMap) -> Res<Json<WebAccessState>> {
    let (s, ops) = wsvc(&st)?;
    let who = admin_user(&ops, &hd, "web access")?;
    blocking(move || s.state(&who)).await.map(Json)
}

fn invalid(e: Invalid) -> Error {
    match e.field {
        Some(f) => Error::invalid(f, e.message),
        None => Error::invalid("settings", e.message),
    }
}

async fn state_put(
    State(st): State<WaState>,
    hd: HeaderMap,
    body: Result<Json<WebAccessConfig>, JsonRejection>,
) -> Res<Json<WebAccessState>> {
    let (s, ops) = wsvc(&st)?;
    let who = admin_user(&ops, &hd, "web access settings")?;
    let Json(mut c) = body?;
    let changed_creds: Vec<String> = c
        .credentials
        .iter()
        .filter(|x| !x.secrets.is_empty())
        .map(|x| x.id.clone())
        .collect();
    audit(&who, "settings", "stage");
    for id in &changed_creds {
        // the id only, never a value
        audit(&who, "credential", id);
    }
    blocking(move || {
        let (eff, cur, _): (WebAccessConfig, WebAccessConfig, Option<WebAccessConfig>) =
            crate::services::docs(&ops.config, &who, WEB_ACCESS_KEY)?;
        s.seal_input(&mut c, &[&eff, &cur]).map_err(invalid)?;
        let c = model::validate(c, &[s.ui_port, s.emergency_port]).map_err(invalid)?;
        crate::services::stage(&ops.config, &who, WEB_ACCESS_KEY, &c)?;
        s.state(&who)
    })
    .await
    .map(Json)
}

async fn plan_post(
    State(st): State<WaState>,
    hd: HeaderMap,
    body: axum::body::Bytes,
) -> Res<Json<WebAccessPlan>> {
    let (s, ops) = wsvc(&st)?;
    let who = admin_user(&ops, &hd, "web access")?;
    let given: Option<WebAccessConfig> = if body.iter().all(|b| b.is_ascii_whitespace()) {
        None
    } else {
        Some(serde_json::from_slice(&body).map_err(|e| Error::invalid("settings", e.to_string()))?)
    };
    blocking(move || {
        let (eff, cur, _): (WebAccessConfig, WebAccessConfig, Option<WebAccessConfig>) =
            crate::services::docs(&ops.config, &who, WEB_ACCESS_KEY)?;
        let mut c = match given {
            Some(mut g) => {
                s.seal_input(&mut g, &[&eff, &cur]).map_err(invalid)?;
                g
            }
            None => eff,
        };
        c = model::validate(c, &[s.ui_port, s.emergency_port]).map_err(invalid)?;
        Ok(s.plan(&c, &cur))
    })
    .await
    .map(Json)
}

async fn sync_post(State(st): State<WaState>, hd: HeaderMap) -> Res<Json<ProxyEngine>> {
    let (s, ops) = wsvc(&st)?;
    let who = admin_user(&ops, &hd, "web access")?;
    audit(&who, "sync", "engine");
    // collided or refused local names are tried again
    s.names.retry();
    blocking(move || {
        if let Err(e) = s.sync(&|l| tracing::info!("{l}")) {
            tracing::warn!("web access: {e}");
        }
        Ok(s.engine_state())
    })
    .await
    .map(Json)
}

async fn confirm_post(State(st): State<WaState>, hd: HeaderMap) -> Res<StatusCode> {
    let (s, ops) = wsvc(&st)?;
    let who = admin_user(&ops, &hd, "web access")?;
    let host = hd
        .get(rynas_auth::ipc::HOST_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let local: Option<SocketAddr> = hd
        .get(rynas_auth::ipc::LOCAL_ADDR_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    s.confirm(host.as_deref(), local)?;
    audit(&who, "confirm", "ui route");
    Ok(StatusCode::NO_CONTENT)
}

async fn test_post(
    State(st): State<WaState>,
    hd: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Res<Json<RouteDiagnostics>> {
    let (s, ops) = wsvc(&st)?;
    admin_user(&ops, &hd, "web access")?;
    blocking(move || s.diagnose(&id)).await.map(Json)
}

async fn domain_test_post(
    State(st): State<WaState>,
    hd: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Res<Json<RouteDiagnostics>> {
    let (s, ops) = wsvc(&st)?;
    admin_user(&ops, &hd, "web access")?;
    blocking(move || s.diagnose_domain(&id)).await.map(Json)
}

async fn domain_records_post(
    State(st): State<WaState>,
    hd: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Res<Json<DnsSyncReport>> {
    let (s, ops) = wsvc(&st)?;
    let who = admin_user(&ops, &hd, "web access")?;
    audit(&who, "records", &id);
    blocking(move || {
        let cfg = s.applied();
        if !cfg.domains.iter().any(|d| d.id == id) {
            return Err(Error::not_found(format!("no domain {id}")));
        }
        let facts = s.facts();
        let mut r = s.sync_records(&cfg, &cfg, &facts, Some(&id), &|l| tracing::info!("{l}"));
        Ok(r.pop().unwrap_or_default())
    })
    .await
    .map(Json)
}

// imported certificates (§5.5): immediate actions, like `/system/tls/import`

async fn imports_get(State(st): State<WaState>, hd: HeaderMap) -> Res<Json<Vec<ImportedCert>>> {
    let (s, ops) = wsvc(&st)?;
    admin_user(&ops, &hd, "web access")?;
    blocking(move || Ok(s.imports())).await.map(Json)
}

async fn imports_post(
    State(st): State<WaState>,
    hd: HeaderMap,
    body: Result<Json<CertImportRequest>, JsonRejection>,
) -> Res<Json<ImportedCert>> {
    let (s, ops) = wsvc(&st)?;
    let who = admin_user(&ops, &hd, "imported certificates")?;
    let Json(req) = body?;
    let v = blocking(move || s.save_import(&req)).await?;
    // the id and the certificate's identity, never key material
    let what = match &v.certificate {
        Some(c) => format!("{} {} ({})", v.id, c.subject, c.fingerprint_sha256),
        None => format!("{} (watched folder)", v.id),
    };
    audit(&who, "import", &what);
    Ok(Json(v))
}

async fn imports_inspect(
    State(st): State<WaState>,
    hd: HeaderMap,
    body: Result<Json<CertImportRequest>, JsonRejection>,
) -> Res<Json<ImportedCert>> {
    let (s, ops) = wsvc(&st)?;
    admin_user(&ops, &hd, "imported certificates")?;
    let Json(req) = body?;
    blocking(move || s.inspect_import(&req)).await.map(Json)
}

async fn import_check(
    State(st): State<WaState>,
    hd: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Res<Json<ImportedCert>> {
    let (s, ops) = wsvc(&st)?;
    admin_user(&ops, &hd, "imported certificates")?;
    blocking(move || s.check_import(&id)).await.map(Json)
}

/// The administrator's consent to the new DNS names a watched folder pushed (M1).
async fn import_accept(
    State(st): State<WaState>,
    hd: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Res<Json<ImportedCert>> {
    let (s, ops) = wsvc(&st)?;
    let who = admin_user(&ops, &hd, "imported certificates")?;
    let (v, accepted) = blocking(move || s.accept_import(&id)).await?;
    audit(
        &who,
        "import-accept",
        &format!("{} names {}", v.id, accepted.join(",")),
    );
    Ok(Json(v))
}

async fn import_delete(
    State(st): State<WaState>,
    hd: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Res<StatusCode> {
    let (s, ops) = wsvc(&st)?;
    let who = admin_user(&ops, &hd, "imported certificates")?;
    let id2 = id.clone();
    blocking(move || s.delete_import(&id2)).await?;
    audit(&who, "import-delete", &id);
    Ok(StatusCode::NO_CONTENT)
}

async fn certificates_get(
    State(st): State<WaState>,
    hd: HeaderMap,
) -> Res<Json<CertificateInventory>> {
    let (s, ops) = wsvc(&st)?;
    admin_user(&ops, &hd, "web access")?;
    blocking(move || Ok(s.certificates())).await.map(Json)
}

async fn providers_get(
    State(st): State<WaState>,
    hd: HeaderMap,
) -> Res<Json<Vec<DnsProviderInfo>>> {
    let (_, ops) = wsvc(&st)?;
    admin_user(&ops, &hd, "web access")?;
    Ok(Json(
        DnsProvider::ALL
            .iter()
            .map(|p| rynas_dns::info(*p))
            .collect(),
    ))
}

pub fn router(st: WaState) -> Router {
    Router::new()
        .route("/v1/web-access", get(state_get).put(state_put))
        .route("/v1/web-access/plan", post(plan_post))
        .route("/v1/web-access/sync", post(sync_post))
        .route("/v1/web-access/confirm", post(confirm_post))
        .route("/v1/web-access/routes/{id}/test", post(test_post))
        .route("/v1/web-access/domains/{id}/test", post(domain_test_post))
        .route(
            "/v1/web-access/domains/{id}/records",
            post(domain_records_post),
        )
        .route("/v1/web-access/certificates", get(certificates_get))
        .route("/v1/web-access/providers", get(providers_get))
        .route(
            "/v1/web-access/imports",
            get(imports_get).post(imports_post),
        )
        .route("/v1/web-access/imports/inspect", post(imports_inspect))
        .route("/v1/web-access/imports/{id}/check", post(import_check))
        .route("/v1/web-access/imports/{id}/accept", post(import_accept))
        .route(
            "/v1/web-access/imports/{id}",
            axum::routing::delete(import_delete),
        )
        .with_state(st)
}
