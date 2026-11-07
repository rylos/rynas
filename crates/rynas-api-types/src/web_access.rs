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
//! Wave W2 (own domains, `docs/phase2/docker-apps.md` §5.4):
//! - `POST /api/v1/web-access/domains/{id}/test` → [`RouteDiagnostics`] (the domain checklist);
//! - `POST /api/v1/web-access/domains/{id}/records` → [`DnsSyncReport`] (write the LAN
//!   records now);
//! - `GET /api/v1/web-access/certificates` → [`CertificateInventory`];
//! - `GET /api/v1/web-access/providers` → `Vec<`[`DnsProviderInfo`]`>`.
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

/// The names of a route. Later waves add the Tailscale and tunnel names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteNames {
    /// `<label>-<host>.local` (published through Avahi when `settings.local_names`).
    #[serde(default = "yes")]
    pub local: bool,
    /// `<label>.<domain>` for every enabled own domain (wave W2).
    #[serde(default = "yes")]
    pub domain: bool,
}

impl Default for RouteNames {
    fn default() -> Self {
        Self {
            local: true,
            domain: true,
        }
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
    /// Own domains (wave W2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<WebDomain>,
    /// DNS provider credentials the domains refer to (wave W2). Secrets are write-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<DnsCredential>,
    /// ACME account settings shared by every domain (wave W2).
    #[serde(default, skip_serializing_if = "AcmeSettings::is_default")]
    pub acme: AcmeSettings,
}

// ---------------------------------------------------------------------------------------------
// wave W2: own domains, DNS providers, ACME, certificates
// ---------------------------------------------------------------------------------------------

/// DNS providers rynas knows: the ACME DNS-01 module compiled into rynas-caddy, and for some
/// rynas's own client that writes the LAN records ([`DnsProviderInfo::records`]).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum DnsProvider {
    #[default]
    Cloudflare,
    Desec,
    Hetzner,
    Porkbun,
    Ovh,
    Duckdns,
    Gandi,
    Route53,
    Digitalocean,
    Rfc2136,
    /// acme-dns: only for `cname_delegation` (it holds nothing but the challenge TXT).
    Acmedns,
}

impl DnsProvider {
    pub const ALL: [DnsProvider; 11] = [
        Self::Cloudflare,
        Self::Desec,
        Self::Hetzner,
        Self::Porkbun,
        Self::Ovh,
        Self::Duckdns,
        Self::Gandi,
        Self::Route53,
        Self::Digitalocean,
        Self::Rfc2136,
        Self::Acmedns,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cloudflare => "cloudflare",
            Self::Desec => "desec",
            Self::Hetzner => "hetzner",
            Self::Porkbun => "porkbun",
            Self::Ovh => "ovh",
            Self::Duckdns => "duckdns",
            Self::Gandi => "gandi",
            Self::Route53 => "route53",
            Self::Digitalocean => "digitalocean",
            Self::Rfc2136 => "rfc2136",
            Self::Acmedns => "acmedns",
        }
    }
}

/// How the certificate of a domain is validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChallengeKind {
    /// ACME DNS-01 through the provider's API (the default: wildcard, no open port).
    #[default]
    Dns01,
    /// ACME HTTP-01: port 80 reachable from the internet, one certificate per name (no
    /// wildcard; app names appear in Certificate Transparency logs).
    Http01,
    /// DNS-01 with `_acme-challenge.<domain>` delegated by a CNAME to `delegate_to` (a
    /// throw-away zone or acme-dns): the credential cannot touch the main zone, so rynas does
    /// not write the LAN records either (the plan lists the records to add by hand).
    CnameDelegation,
}

/// One own domain (ideally a subzone such as `nas.example.com`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebDomain {
    /// `[a-z0-9-]`, 1-63 characters, unique; never changes.
    pub id: String,
    /// Lowercase DNS name, at least two labels, no wildcard.
    pub name: String,
    /// DNS provider of the zone (DNS-01 and records). Optional for `http01` without records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<DnsProvider>,
    /// Id of a [`DnsCredential`] of the same provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    #[serde(default)]
    pub challenge: ChallengeKind,
    /// `cname_delegation`: the name `_acme-challenge.<name>` points to (the CNAME target;
    /// with acme-dns, the account's `fulldomain`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate_to: Option<String>,
    /// One certificate for `*.<name>` + `<name>` (DNS-01 only).
    #[serde(default = "yes")]
    pub wildcard: bool,
    /// rynas writes `*.<name>` and `<name>` A (and AAAA with `lan_ipv6`) = the NAS's LAN
    /// address through the provider, and keeps them current.
    #[serde(default = "yes")]
    pub lan_record: bool,
    /// Also AAAA records (a stable IPv6 address of the LAN interface).
    #[serde(default)]
    pub lan_ipv6: bool,
    /// The interface is also served on `<name>` itself.
    #[serde(default = "yes")]
    pub ui: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Record names (`nas.example.com`, `*.nas.example.com`) whose existing values rynas may
    /// replace although it did not write them (a plan shows them as `conflict` until then).
    /// Each is the domain's name or a name below it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub take_over: Vec<String>,
}

impl Default for WebDomain {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            provider: None,
            credential: None,
            challenge: ChallengeKind::Dns01,
            delegate_to: None,
            wildcard: true,
            lan_record: true,
            lan_ipv6: false,
            ui: true,
            enabled: true,
            take_over: Vec::new(),
        }
    }
}

/// A DNS provider credential (the TrueNAS "DNS authenticator" idea: reusable by domains).
///
/// The values are **write-only**: a PUT that carries `secrets` replaces them; a PUT without
/// `secrets` keeps the stored ones (the UI sends back what GET gave). Answers never carry
/// them: `fields` lists the names that are stored. `sealed` is rynasd's encrypted envelope
/// inside the configuration key: never in an answer, ignored on input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DnsCredential {
    /// `[a-z0-9-]`, 1-63 characters, unique.
    pub id: String,
    pub provider: DnsProvider,
    /// Free text for the list ("token for example.com").
    #[serde(default)]
    pub label: String,
    /// Write-only values by field name ([`DnsProviderInfo::fields`]).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub secrets: std::collections::BTreeMap<String, String>,
    /// Names of the stored values (answers only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    /// Internal: the encrypted values (never in answers, ignored on input).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed: Option<String>,
}

/// ACME account settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AcmeSettings {
    /// Contact address of the ACME account (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Let's Encrypt staging (untrusted certificates, high rate limits): for tests.
    #[serde(default)]
    pub staging: bool,
    /// ACME profile (`classic`, `tlsserver`); none = the CA's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

impl AcmeSettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// One value a provider credential needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DnsProviderField {
    /// Field name (the key in [`DnsCredential::secrets`]).
    pub name: String,
    /// Secret: a password field, never returned.
    pub secret: bool,
    /// Optional (a default applies).
    #[serde(default)]
    pub optional: bool,
}

/// `GET /api/v1/web-access/providers`: what each provider supports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DnsProviderInfo {
    pub provider: DnsProvider,
    /// Values a credential needs.
    pub fields: Vec<DnsProviderField>,
    /// rynas writes the LAN records with this provider.
    pub records: bool,
    /// The provider stores wildcard records (`*.<name>`); otherwise one record per name.
    pub wildcard_records: bool,
    /// Tokens can be limited to one zone or record set. When false the form recommends
    /// `cname_delegation`.
    pub scoped_tokens: bool,
    /// Lowest TTL the provider accepts (seconds); rynas writes at least this.
    pub min_ttl: u32,
    /// English guidance for a minimum-scope token, shown under the form.
    pub token_hint: String,
}

/// What a record sync does to one record set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DnsAction {
    /// Already right.
    #[default]
    Keep,
    Create,
    Update,
    Delete,
    /// The administrator adds it by hand (CNAME delegation, providers without a client).
    Manual,
    /// Not known: the provider could not be read (`detail` says why) or was not asked.
    Unknown,
    /// A record set rynas did not write (or that changed since rynas wrote it) holds other
    /// values: left as it is. A wanted set is written only after the administrator takes
    /// it over ([`WebDomain::take_over`]); a set rynas no longer wants is not deleted and no
    /// longer managed. `current` holds the provider's values, `detail` says which case.
    Conflict,
}

/// One record set and what happens to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DnsChange {
    /// The domain's name.
    pub domain: String,
    /// Fully qualified name (`*.nas.example.com`).
    pub name: String,
    /// `A`, `AAAA`, `CNAME`.
    pub rtype: String,
    pub action: DnsAction,
    /// Values at the provider now.
    #[serde(default)]
    pub current: Vec<String>,
    /// Values after the change (empty for `delete`).
    #[serde(default)]
    pub wanted: Vec<String>,
    #[serde(default)]
    pub ttl: u32,
    /// English detail (the provider's error...).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// `POST /api/v1/web-access/domains/{id}/records`, and the last sync in [`DomainView`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DnsSyncReport {
    pub domain: String,
    pub changes: Vec<DnsChange>,
    /// Every change went through.
    pub ok: bool,
    pub at_ms: i64,
}

/// One domain as the pages show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DomainView {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Names the certificate covers (`*.<name>` + `<name>`, or one per name with HTTP-01).
    pub certificate_names: Vec<String>,
    /// URLs of the interface on this domain.
    #[serde(default)]
    pub ui_urls: Vec<String>,
    /// The last record sync (none before the first apply, or without `lan_record`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub records: Option<DnsSyncReport>,
    /// Problem codes: `credential_missing`, `records_failed`, `records_manual`,
    /// `records_conflict` (a record set rynas did not write holds other values: see
    /// [`WebDomain::take_over`]), `no_lan_address`, `staging`.
    #[serde(default)]
    pub problems: Vec<String>,
}

/// Where a certificate comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CertKind {
    /// rynas's local CA (the root "Trust this NAS" installs).
    #[default]
    LocalCa,
    /// The certificate `:8443` serves (local leaf or imported).
    Interface,
    /// The proxy's leaf for the `.local` names (local CA).
    ProxyLocal,
    /// ACME, obtained and renewed by rynas-caddy for an own domain.
    Acme,
}

/// Health of one certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CertStatus {
    #[default]
    Ok,
    /// Expires within 14 days.
    ExpiresSoon,
    Expired,
    /// Past its renewal point and not renewed for a day: renewal is failing.
    RenewalOverdue,
    /// Expected (an applied domain) but not obtained yet.
    Pending,
    /// Expected for more than 15 minutes and still missing: issuance is failing.
    Failed,
}

/// One certificate of the inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CertEntry {
    /// Stable id: `local-ca`, `interface`, `proxy-local`, `acme:<name>` (one ACME
    /// certificate per name: `acme:*.nas.example.com`, `acme:nas.example.com`).
    pub id: String,
    pub kind: CertKind,
    pub status: CertStatus,
    /// Names it covers (or should cover, while `pending`/`failed`).
    pub names: Vec<String>,
    /// The domain's name (ACME).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Issued by Let's Encrypt staging (browsers do not trust it).
    #[serde(default)]
    pub staging: bool,
    /// `local` or `imported` for the interface certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The certificate itself, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<crate::tls::TlsCertificate>,
    /// From when rynas-caddy renews it (ms since the epoch; 2/3 of the lifetime, earlier
    /// when the CA's ARI window says so). ACME only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renew_after_ms: Option<i64>,
    /// Last issuance or renewal error the engine logged (English).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// `GET /api/v1/web-access/certificates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CertificateInventory {
    pub certificates: Vec<CertEntry>,
    pub at_ms: i64,
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
    /// exceed 63 characters for this host name), `local_names_off`, `disabled`,
    /// `name_collision`, `name_unpublished`, `domain_name_too_long` (`<label>.<domain>`
    /// over 253 characters).
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
    /// The own domains (wave W2), in the order of the key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<DomainView>,
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
    /// DNS records the apply would write, per domain (wave W2). The provider is read (never
    /// written) to say what changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns_changes: Vec<DnsChange>,
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
    /// Route test: `local_name`, `dns`, `proxy_running`, `route_loaded`, `https_listening`,
    /// `certificate`, `upstream`, then (wave W2, with an own-domain name) `domain_dns`,
    /// `domain_certificate`. Domain checklist: `dns_record`, `dns_nas`, `dns_public`,
    /// `rebind`, `certificate`, `renewal`, `hairpin`.
    pub id: String,
    pub state: CheckState,
    /// English detail (addresses, the engine's message...), shown as is.
    pub detail: String,
}

/// `POST /api/v1/web-access/routes/{id}/test` and `POST /api/v1/web-access/domains/{id}/test`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RouteDiagnostics {
    /// The route's (or the domain's) id.
    pub route: String,
    pub checks: Vec<RouteCheck>,
    pub at_ms: i64,
}
