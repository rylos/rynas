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
pub mod diag;
pub mod model;

use std::collections::BTreeSet;
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
}

impl Default for EnginePaths {
    fn default() -> Self {
        Self {
            admin_socket: ADMIN_SOCKET.into(),
            storage: format!("{STATE_DIR}/data"),
            cert: CERT.into(),
            key: KEY.into(),
        }
    }
}

/// Everything a render depends on.
pub struct RenderInput<'a> {
    pub cfg: &'a WebAccessConfig,
    pub facts: &'a HostFacts,
    pub paths: &'a EnginePaths,
    /// rynas-web's plain HTTP port on loopback (the interface's upstream).
    pub ui_port: u16,
    /// rynas-web's own HTTPS port.
    pub emergency_port: u16,
}

/// A route with the names it gets on this host.
#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub route: WebRoute,
    pub names: Vec<String>,
    pub problems: Vec<String>,
}

/// Served names: the interface's own and each route's.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub host: String,
    pub ui: Vec<String>,
    pub routes: Vec<ResolvedRoute>,
}

impl Resolved {
    /// Every served name (for the certificate).
    pub fn all_names(&self) -> Vec<String> {
        let mut v = self.ui.clone();
        for r in &self.routes {
            v.extend(r.names.iter().cloned());
        }
        v
    }
}

/// The names each route gets: `<label>-<host>.local` when it is enabled, local names are
/// on and the name fits; routes sorted by id.
pub fn resolve(cfg: &WebAccessConfig, facts: &HostFacts) -> Resolved {
    let host = facts.host();
    let builtin = cfg.settings.mode == ProxyMode::Builtin;
    let ui = if builtin && cfg.settings.ui.enabled {
        model::ui_names(&host)
    } else {
        Vec::new()
    };
    let mut routes: Vec<ResolvedRoute> = cfg
        .routes
        .iter()
        .map(|r| {
            let mut problems = Vec::new();
            let mut names = Vec::new();
            if !r.enabled {
                problems.push("disabled".to_string());
            } else if !cfg.settings.local_names || !r.names.local {
                problems.push("local_names_off".to_string());
            } else {
                match model::local_name(&r.label, &host) {
                    Some(n) => names.push(n),
                    None => problems.push("name_too_long".to_string()),
                }
            }
            if !builtin {
                names.clear();
            }
            ResolvedRoute {
                route: r.clone(),
                names,
                problems,
            }
        })
        .collect();
    routes.sort_by(|a, b| a.route.id.cmp(&b.route.id));
    Resolved { host, ui, routes }
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
            root,
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
        }
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
        v
    }

    fn input<'a>(&'a self, cfg: &'a WebAccessConfig, facts: &'a HostFacts) -> RenderInput<'a> {
        RenderInput {
            cfg,
            facts,
            paths: &self.paths,
            ui_port: self.ui_port,
            emergency_port: self.emergency_port,
        }
    }

    /// The resolved names and the engine configuration of `cfg` on this host.
    pub fn render(&self, cfg: &WebAccessConfig, facts: &HostFacts) -> (Resolved, Value) {
        let resolved = resolve(cfg, facts);
        let json = Caddy.render(&self.input(cfg, facts), &resolved);
        (resolved, json)
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

    /// The engine's state folder (`/var/lib/rynas-caddy`, writable by the engine's user):
    /// made when missing (its parents are root's), opened as a descriptor.
    fn engine_state_dir(&self) -> io::Result<std::fs::File> {
        let p = self.path(LAST_GOOD);
        let dir = p.parent().expect("has a parent");
        std::fs::create_dir_all(dir)?;
        rynas_docker::safefs::open_start(dir)
    }

    /// `rynas/` in the engine's state folder: rynasd's own files (certificate and key),
    /// `0750 root:rynas-caddy` so that the engine can read but never replace them. A symlink
    /// planted there is refused; an existing folder gets the owner and mode back.
    fn rynas_dir(&self, gid: Option<u32>) -> io::Result<std::fs::File> {
        let state = self.engine_state_dir()?;
        let owner = gid.map(|g| (0, g));
        let (d, made) = rynas_docker::safefs::ensure_dir_at(&state, "rynas", 0o750, owner)?;
        if !made && owner.is_some() {
            rynas_docker::safefs::set_owner_mode(&d, 0o750, owner)?;
        }
        Ok(d)
    }

    fn write_last_good(&self, json: &Value) -> io::Result<()> {
        let mut text = serde_json::to_vec_pretty(json).map_err(io::Error::other)?;
        text.push(b'\n');
        let dir = self.engine_state_dir()?;
        let name = LAST_GOOD.rsplit('/').next().unwrap_or(LAST_GOOD);
        if rynas_docker::safefs::read_at(&dir, name, 16 << 20).is_ok_and(|old| old.as_bytes() == text) {
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
        let (resolved, json) = self.render(cfg, facts);
        if json.is_null() {
            self.publish_ui(&[]);
            return Ok(false);
        }
        let names = resolved.all_names();
        let cert_changed = if names.is_empty() {
            false
        } else {
            self.ensure_cert(&names)
                .map_err(|e| format!("the certificate for the local names: {e}"))?
        };
        if cert_changed {
            log("web access: new certificate for the local names");
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
        let routes = r
            .routes
            .iter()
            .map(|x| RouteView {
                id: x.route.id.clone(),
                kind: x.route.kind,
                stack: x.route.stack.clone(),
                app: x.route.app.clone(),
                urls: x
                    .names
                    .iter()
                    .map(|n| url_of(n, port, &x.route.path))
                    .collect(),
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
            v.dedup();
            v
        };
        Ok(WebAccessState {
            pending: (eff != cur).then_some(eff),
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
            applied: cur,
        })
    }

    /// `POST /web-access/plan`.
    pub fn plan(&self, cfg: &WebAccessConfig, previous: &WebAccessConfig) -> WebAccessPlan {
        let facts = self.facts();
        let (resolved, json) = self.render(cfg, &facts);
        WebAccessPlan {
            engine_config: json,
            avahi_hosts: avahi_block(cfg, &facts),
            names: resolved.all_names(),
            ui_route_changes: ui_fingerprint(cfg) != ui_fingerprint(previous)
                && !resolved.ui.is_empty(),
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
        let name = r.names.first().cloned();
        // 0. the name is published through Avahi
        checks.push(match &name {
            None => check(
                "local_name",
                CheckState::Skipped,
                format!("the route has no name ({})", r.problems.join(", ")),
            ),
            Some(n) => local_name_check(n, &self.names.report()),
        });
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
        // 3. the route is loaded
        let want_id = format!("rynas-route-{id}");
        checks.push(match &running {
            None => check("route_loaded", CheckState::Skipped, ""),
            Some(v) => {
                let found = v["apps"]["http"]["servers"]["rynas_https"]["routes"]
                    .as_array()
                    .is_some_and(|rs| rs.iter().any(|x| x["@id"] == want_id.as_str()));
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
        // 6. the upstream answers
        let up: Option<SocketAddr> = r
            .route
            .upstream
            .host
            .parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, r.route.upstream.port));
        checks.push(match up {
            None => check("upstream", CheckState::Fail, "bad upstream address"),
            Some(a) => match self.probe.connect(a) {
                Ok(()) => check("upstream", CheckState::Ok, format!("{a} answers")),
                Err(e) => check(
                    "upstream",
                    CheckState::Fail,
                    format!("{a}: {e} (is the app running?)"),
                ),
            },
        });
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
                x.names
                    .first()
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
        let c: WebAccessConfig = serde_json::from_value(value)
            .map_err(|e| Invalid::from(format!("invalid web_access document: {e}")))?;
        let c = model::validate(c, &[self.svc.ui_port, self.svc.emergency_port])?;
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
        self.svc.publish_names(&config_of(to), &self.svc.facts());
        self.svc.engine_apply(from, to, ctx.system, ctx.log)
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
    let Json(c) = body?;
    let c = model::validate(c, &[s.ui_port, s.emergency_port]).map_err(invalid)?;
    audit(&who, "settings", "stage");
    blocking(move || {
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
        let c = model::validate(given.unwrap_or(eff), &[s.ui_port, s.emergency_port])
            .map_err(invalid)?;
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

pub fn router(st: WaState) -> Router {
    Router::new()
        .route("/v1/web-access", get(state_get).put(state_put))
        .route("/v1/web-access/plan", post(plan_post))
        .route("/v1/web-access/sync", post(sync_post))
        .route("/v1/web-access/confirm", post(confirm_post))
        .route("/v1/web-access/routes/{id}/test", post(test_post))
        .with_state(st)
}
