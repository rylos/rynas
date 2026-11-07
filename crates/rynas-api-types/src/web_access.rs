//! Web access (wave W1 of `docs/research/web-access.md`, D-PROXY / D-EXPOSE as revised on
//! 2026-11-07): the reverse proxy `rynas-caddy` in front of the web interface and the apps.
//!
//! - `GET /api/v1/web-access` → [`WebAccessState`];
//! - `PUT /api/v1/web-access` ([`WebAccessConfig`]) stages the configuration key
//!   `web_access` (apply with `/config/apply`) → [`WebAccessState`];
//! - `POST /api/v1/web-access/plan` (optional [`WebAccessConfig`]) → [`WebAccessPlan`];
//! - `POST /api/v1/web-access/sync` → [`ProxyEngine`];
//! - `POST /api/v1/web-access/confirm` → 204;
//! - `POST /api/v1/web-access/routes/{id}/test` → [`RouteDiagnostics`].
//!
//! The model is engine-neutral: nothing here is Caddy-specific. Names (local `.local` name
//! today; own domain, Tailscale and tunnel names in later waves) are separate from who may
//! connect ([`AccessLevel`]).

use serde::{Deserialize, Serialize};

use crate::apps::WebAuth;

/// Who serves ports 80/443.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    /// rynas-caddy owns 80/443 (the default from install).
    #[default]
    Builtin,
    /// "Use another reverse proxy": rynas-caddy is stopped and releases 80/443 for an app
    /// (Nginx Proxy Manager, Traefik, Zoraxy...). The interface stays on the emergency port.
    External,
}

/// Who may connect to a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AccessLevel {
    /// The NAS's connected subnets (IPv4 and IPv6, link-local included) and `allow`.
    #[default]
    Lan,
    /// `lan` + the tailnet ranges (only while `tailscale0` exists; the firewall drops them
    /// on other interfaces) + WireGuard subnets.
    LanVpn,
    /// Anyone. Not available before the public-exposure flow (wave W3): refused.
    Public,
}

/// What a route fronts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    /// An app installed from the catalogue (`stack` names it).
    #[default]
    App,
    /// Any other Docker stack (`stack` + optional `service`).
    Stack,
    /// Another address (a host on the LAN). Wave W2: refused for now.
    Custom,
}

/// Scheme the upstream speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamScheme {
    #[default]
    Http,
    /// HTTPS with the container's own (usually self-signed) certificate: not verified, the
    /// upstream is on loopback.
    Https,
}

/// Security headers added to answers that lack them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HeaderPreset {
    /// `X-Content-Type-Options: nosniff`, `Referrer-Policy: strict-origin-when-cross-origin`,
    /// `X-Frame-Options: SAMEORIGIN`; `Server` and `X-Powered-By` removed.
    #[default]
    Default,
    /// As `default` but framing allowed (dashboards embedded elsewhere).
    Relaxed,
}

fn yes() -> bool {
    true
}

fn slash() -> String {
    "/".into()
}

fn loopback() -> String {
    "127.0.0.1".into()
}

fn http_port() -> u16 {
    80
}

fn https_port() -> u16 {
    443
}

/// The route of the web interface itself (`https://<host>.local`, upstream rynas-web).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiRoute {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub access: AccessLevel,
    /// Extra source ranges (CIDR or single address).
    #[serde(default)]
    pub allow: Vec<String>,
}

impl Default for UiRoute {
    fn default() -> Self {
        Self {
            enabled: true,
            access: AccessLevel::Lan,
            allow: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebAccessSettings {
    #[serde(default)]
    pub mode: ProxyMode,
    #[serde(default = "http_port")]
    pub http_port: u16,
    #[serde(default = "https_port")]
    pub https_port: u16,
    /// HTTP/3 (UDP on the HTTPS port).
    #[serde(default = "yes")]
    pub h3: bool,
    /// Publish `<label>-<host>.local` for routes through Avahi.
    #[serde(default = "yes")]
    pub local_names: bool,
    #[serde(default)]
    pub ui: UiRoute,
}

impl Default for WebAccessSettings {
    fn default() -> Self {
        Self {
            mode: ProxyMode::Builtin,
            http_port: 80,
            https_port: 443,
            h3: true,
            local_names: true,
            ui: UiRoute::default(),
        }
    }
}

/// Where a route sends requests. Wave W1: loopback only (`127.0.0.1` or `::1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebUpstream {
    #[serde(default)]
    pub scheme: UpstreamScheme,
    #[serde(default = "loopback")]
    pub host: String,
    pub port: u16,
}

impl Default for WebUpstream {
    fn default() -> Self {
        Self {
            scheme: UpstreamScheme::Http,
            host: loopback(),
            port: 0,
        }
    }
}

/// The names of a route. Later waves add the own-domain, Tailscale and tunnel names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteNames {
    /// `<label>-<host>.local` (published through Avahi when `settings.local_names`).
    #[serde(default = "yes")]
    pub local: bool,
}

impl Default for RouteNames {
    fn default() -> Self {
        Self { local: true }
    }
}

/// Proxy limits (from the catalogue `[web]` block).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteLimits {
    /// Largest request body, `<digits>[K|M|G]`; none = no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Upstream read timeout, `<digits>s|m|h`; none = no timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Response buffering; `false` flushes every write at once (streaming, server-sent
    /// events, large downloads).
    #[serde(default = "yes")]
    pub buffering: bool,
}

impl Default for RouteLimits {
    fn default() -> Self {
        Self {
            body: None,
            timeout: None,
            buffering: true,
        }
    }
}

/// The rynas login in front of a route (`forward_auth` to rynas-web, an app-only cookie).
/// Wave W3: refused for now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SsoRule {
    /// Groups allowed (default `users`).
    #[serde(default)]
    pub groups: Vec<String>,
    /// Path prefixes that skip the login (mobile apps, DAV, API tokens).
    #[serde(default)]
    pub bypass: Vec<String>,
}

/// One route: names → upstream, with who may connect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebRoute {
    /// `[a-z0-9-]`, 1-63 characters, unique; never changes.
    pub id: String,
    #[serde(default)]
    pub kind: RouteKind,
    /// Stack the route belongs to (apps and stacks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// Catalogue id (apps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Compose service (stacks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// DNS label of the app: the local name is `<label>-<host>.local`. Letters, digits and
    /// `-`, at most 40 characters, unique across routes.
    pub label: String,
    pub upstream: WebUpstream,
    #[serde(default)]
    pub names: RouteNames,
    #[serde(default)]
    pub access: AccessLevel,
    /// Extra source ranges (CIDR or single address).
    #[serde(default)]
    pub allow: Vec<String>,
    /// How the app logs users in (catalogue `web.auth`).
    #[serde(default)]
    pub auth: WebAuth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sso: Option<SsoRule>,
    #[serde(default)]
    pub limits: RouteLimits,
    /// Informational: WebSocket upgrades always pass.
    #[serde(default)]
    pub websocket: bool,
    /// Informational: the app also works below a path prefix (routes always get their own
    /// host name).
    #[serde(default)]
    pub subpath: bool,
    /// Entry path for the Open button.
    #[serde(default = "slash")]
    pub path: String,
    #[serde(default)]
    pub headers: HeaderPreset,
    #[serde(default = "yes")]
    pub enabled: bool,
}

impl Default for WebRoute {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: RouteKind::App,
            stack: None,
            app: None,
            service: None,
            label: String::new(),
            upstream: WebUpstream::default(),
            names: RouteNames::default(),
            access: AccessLevel::Lan,
            allow: Vec::new(),
            auth: WebAuth::App,
            sso: None,
            limits: RouteLimits::default(),
            websocket: false,
            subpath: false,
            path: slash(),
            headers: HeaderPreset::Default,
            enabled: true,
        }
    }
}

/// The configuration key `web_access` (staged and applied like every key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WebAccessConfig {
    #[serde(default)]
    pub settings: WebAccessSettings,
    #[serde(default)]
    pub routes: Vec<WebRoute>,
}

/// State of the proxy engine (rynas-caddy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProxyEngine {
    /// The `rynas-caddy` package is installed (its unit or its admin socket exists).
    pub installed: bool,
    /// Its admin socket answers.
    pub running: bool,
    /// The running configuration equals rynas's render; `None` when not checked (not
    /// running, external mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_sync: Option<bool>,
    /// Last successful load, milliseconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_load_ms: Option<i64>,
    /// Last refusal or error of the engine (English, from the engine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// One route as the pages show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RouteView {
    pub id: String,
    pub kind: RouteKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Host names served (today the local name, when on).
    pub names: Vec<String>,
    /// `https://<name><path>` for each name (with `:<port>` when the HTTPS port is not 443).
    pub urls: Vec<String>,
    /// `scheme://host:port` of the upstream.
    pub upstream: String,
    pub access: AccessLevel,
    pub enabled: bool,
    /// Problems found while rendering, as codes: `name_too_long` (the local name would
    /// exceed 63 characters for this host name), `local_names_off`, `disabled`.
    #[serde(default)]
    pub problems: Vec<String>,
}

/// A change of the interface's own route waiting for the timed confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PendingConfirm {
    /// When the previous settings come back without a confirmation (ms since the epoch).
    pub deadline_ms: i64,
    /// Names the confirmation must arrive on (`https://<name>/`, through the proxy).
    pub names: Vec<String>,
}

/// `GET /api/v1/web-access`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WebAccessState {
    pub applied: WebAccessConfig,
    /// The caller's staged value, when it differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<WebAccessConfig>,
    pub engine: ProxyEngine,
    /// First label of the host name, lowercase (local names end in `-<host>.local`).
    pub host: String,
    /// Names of the interface's own route (`<host>.local`, `<host>`), empty when off.
    pub ui_names: Vec<String>,
    /// URLs of the interface through the proxy.
    pub ui_urls: Vec<String>,
    pub routes: Vec<RouteView>,
    /// Source ranges of `lan` (computed from the connected subnets).
    pub lan: Vec<String>,
    /// Ranges `lan_vpn` adds (tailnet while `tailscale0` exists, WireGuard subnets).
    pub vpn: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<PendingConfirm>,
    /// The port of rynas-web's own HTTPS (never routed through the proxy).
    pub emergency_port: u16,
    /// Host ports apps cannot publish (the proxy's, unless external mode).
    pub reserved_ports: Vec<u16>,
}

/// `POST /api/v1/web-access/plan`: what an apply would load. No side effects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WebAccessPlan {
    /// The full engine configuration (Caddy JSON), `null` in external mode.
    pub engine_config: serde_json::Value,
    /// rynas's block of `/etc/avahi/hosts` (`<address> <name>` lines).
    pub avahi_hosts: String,
    /// Every host name the proxy would answer for.
    pub names: Vec<String>,
    /// The interface's own route changes: the apply waits for a timed confirmation.
    pub ui_route_changes: bool,
}

/// State of one diagnostic step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Ok,
    Warn,
    Fail,
    /// Not run (an earlier step failed, or it does not apply).
    #[default]
    Skipped,
}

/// One diagnostic step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RouteCheck {
    /// `dns`, `proxy_running`, `route_loaded`, `https_listening`, `certificate`, `upstream`.
    pub id: String,
    pub state: CheckState,
    /// English detail (addresses, the engine's message...), shown as is.
    pub detail: String,
}

/// `POST /api/v1/web-access/routes/{id}/test`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RouteDiagnostics {
    pub route: String,
    pub checks: Vec<RouteCheck>,
    pub at_ms: i64,
}
