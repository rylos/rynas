//! `app.toml`: the metadata of a catalogue entry. Normative description in
//! `docs/phase2/docker-apps.md` § 6.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::Issues;
use crate::version::Version;

/// The `schema` value this crate reads.
pub const SCHEMA: u32 = 1;

/// Variables rynas always provides to a render. An entry may reference them in its compose
/// file but never declare them itself.
pub const PROVIDED_VARS: &[&str] = &[
    // absolute path of the stack folder (`/<vol>/docker/stacks/<stack>`)
    "RYNAS_APPDATA",
    // absolute path of the stacks folder (`/<vol>/docker/stacks`), for Docker managers
    "RYNAS_STACKS_DIR",
    // host name or address users reach the NAS on (for app URLs)
    "RYNAS_HOST",
    // the `docker-apps` user and group
    "PUID",
    "PGID",
    "TZ",
    // gid of /var/run/docker.sock, for managers that run as PUID
    "DOCKER_GID",
    // gid owning /dev/dri/renderD128 (`render`), for hardware transcoding
    "RENDER_GID",
    // "true" when the CPU has AVX2 (overlays for baseline images)
    "RYNAS_CPU_AVX2",
    // host address proxied ports bind to: 127.0.0.1 (behind the rynas web access proxy, the
    // default) or 0.0.0.0 when the user publishes the port on the LAN directly
    "RYNAS_BIND",
    // comma-separated addresses/CIDRs the app must trust for X-Forwarded-* headers
    "RYNAS_TRUSTED_PROXIES",
];

/// Parsed `app.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppManifest {
    /// Format version of this file, currently [`SCHEMA`].
    pub schema: u32,
    /// Identifier, equal to the folder name: `[a-z0-9][a-z0-9-]*`, at most 40 characters.
    pub id: String,
    pub name: String,
    /// Package version of the entry (bumped on every change of the entry).
    pub version: Version,
    /// Upstream version of the main image (free text, the tag without the digest).
    pub app_version: String,
    /// One line for the catalogue card. The long text is `description.md`.
    pub description: String,
    pub homepage: String,
    /// Upstream source repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub changelog_url: String,
    /// SPDX expression of the upstream licence.
    pub license: String,
    /// Who maintains this entry (not upstream).
    pub maintainers: Vec<String>,
    pub categories: Vec<Category>,
    /// Oldest rynas that can install this entry.
    pub min_rynas: Version,
    pub arch: Vec<Arch>,
    #[serde(default)]
    pub antifeatures: Vec<Antifeature>,
    /// The service that is "the app": web UI, health shown on the card.
    pub main_service: String,
    /// All services use `network_mode: host`; `ports` then lists the host ports the app
    /// listens on (not relocatable).
    #[serde(default)]
    pub host_network: bool,
    /// A Docker manager: gets the Docker socket and the stacks folder at the same path inside
    /// and outside the container (`${RYNAS_STACKS_DIR}:${RYNAS_STACKS_DIR}`).
    #[serde(default)]
    pub manager: bool,
    /// A reverse proxy (Nginx Proxy Manager, Traefik): publishes host ports 80 and 443
    /// itself, so it installs only while the rynas web access runs in external proxy mode
    /// (the built-in proxy owns 80/443 otherwise). Its 80/443 ports are exempt from the
    /// catalogue-wide unique-default rule: two proxies exclude each other anyway.
    #[serde(default)]
    pub reverse_proxy: bool,
    /// Listed in the catalogue's Featured section (a curated pick of the catalogue).
    #[serde(default)]
    pub featured: bool,
    pub resources: Resources,
    #[serde(default)]
    pub ports: Vec<Port>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<Web>,
    #[serde(default)]
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub shares: Vec<Share>,
    #[serde(default)]
    pub secrets: Vec<Secret>,
    #[serde(default)]
    pub hardware: Hardware,
    #[serde(default)]
    pub security: Security,
    #[serde(default)]
    pub overlays: Vec<Overlay>,
    #[serde(default)]
    pub named_volumes: Vec<NamedVolume>,
    #[serde(default)]
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    Management,
    Media,
    Photos,
    Files,
    Documents,
    Productivity,
    Security,
    HomeAutomation,
    Monitoring,
    Network,
    Sync,
    Development,
    Backup,
    Ai,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    Amd64,
    Arm64,
}

/// Honest warnings shown on the card (YunoHost style).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Antifeature {
    /// Upstream is not free software.
    NonFree,
    /// Needs more than 2 GB of RAM to be usable.
    HighResources,
    /// Needs a GPU or accelerator to be usable.
    NeedsGpu,
    /// Sends usage data upstream by default.
    Telemetry,
    /// Upstream had no release for a long time.
    Unmaintained,
    /// Clients refuse plain HTTP (secure context): needs the web access module or a proxy.
    RequiresHttps,
    /// Gets root-equivalent access to the host (Docker socket, privileged, host devices).
    RootAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    /// Below this the install is refused.
    pub ram_min_mb: u32,
    /// Below this the install shows a warning.
    pub ram_recommended_mb: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

fn yes() -> bool {
    true
}

/// A published port. In the compose file it appears as `"${RYNAS_BIND}:${ENV}:<container>"`
/// when `proxied` (an HTTP port fronted by the rynas proxy, loopback by default), otherwise
/// as `"${ENV}:<container>[/udp]"` (a protocol port reachable on the LAN).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Port {
    /// Short identifier (`web`, `sync`, …), referenced by `web.port`.
    pub name: String,
    /// The `.env` variable holding the host port.
    pub env: String,
    pub service: String,
    /// Port inside the container (with `host_network`, the port the app listens on).
    pub container: u16,
    /// Default host port.
    pub default: u16,
    pub protocol: Protocol,
    /// Shown next to the port ("Web interface", "Sync protocol").
    pub purpose: String,
    /// The installer may move the host port when the default is taken.
    #[serde(default = "yes")]
    pub relocatable: bool,
    /// An HTTP port fronted by the rynas web access proxy: bound to `${RYNAS_BIND}`.
    #[serde(default)]
    pub proxied: bool,
}

/// Who authenticates web users.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebAuth {
    /// The app has its own login (the proxy may still add rynas SSO in front).
    App,
    /// No authentication at all: the proxy must add rynas SSO.
    None,
    /// The app trusts the proxy's identity headers (forward_auth).
    Proxy,
}

/// May the app be published on the Internet through the proxy?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Public {
    Ok,
    /// Allowed with a warning (weak auth history, large attack surface).
    Warn,
    /// Refused (Docker managers, root-equivalent tools).
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    Http,
    Https,
}

/// How the web UI is reached and proxied (consumed by the web access module, D-PROXY).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Web {
    /// Name of the port in `ports`.
    pub port: String,
    /// Entry path of the UI.
    #[serde(default = "slash")]
    pub path: String,
    /// Scheme the container speaks on that port.
    pub scheme: Scheme,
    pub auth: WebAuth,
    /// Needs WebSocket upgrades through the proxy.
    #[serde(default)]
    pub websocket: bool,
    /// Works under a path prefix (`https://nas/<app>/`); otherwise it needs its own host.
    #[serde(default)]
    pub subpath: bool,
    /// Largest request body the proxy must accept (`10G`, `512M`); none = proxy default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Upstream read timeout (`60s`, `10m`, `1h`); none = proxy default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Proxy response buffering; `false` for streaming, uploads and server-sent events.
    #[serde(default = "yes")]
    pub buffering: bool,
    /// Path prefixes that skip rynas SSO (mobile apps, API, sync clients).
    #[serde(default)]
    pub bypass: Vec<String>,
    pub public: Public,
    /// Environment variable of the main service listing trusted proxies; compose must set it
    /// to `${RYNAS_TRUSTED_PROXIES}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_proxies_env: Option<String>,
}

fn size_ok(s: &str) -> bool {
    let d = s.trim_end_matches(['K', 'M', 'G']);
    !d.is_empty() && d.len() + 1 >= s.len() && d.bytes().all(|c| c.is_ascii_digit())
}

fn duration_ok(s: &str) -> bool {
    match s.strip_suffix(['s', 'm', 'h']) {
        Some(d) => !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

fn slash() -> String {
    "/".into()
}

/// What a data folder holds; drives snapshot, backup and placement advice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Intent {
    Config,
    Data,
    Db,
    Media,
    Cache,
}

/// May the folder live on a faster (SSD) volume than the stack?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ssd {
    /// Keep it with the stack.
    #[default]
    No,
    /// Allowed on an SSD volume.
    Ok,
    /// Strongly advised on an SSD volume (databases).
    Preferred,
}

/// A bind folder owned by the app. Default location `./<path>` in the stack folder (so the
/// stack snapshot covers it); the installer may place it elsewhere (SSD volume).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Folder {
    pub env: String,
    /// Relative path below the stack folder, `[a-z0-9._-]` segments.
    pub path: String,
    pub intent: Intent,
    #[serde(default)]
    pub ssd: Ssd,
    /// `uid:gid` the image needs on this folder when it does not run as PUID and does not
    /// fix ownership itself (e.g. `33:33` for www-data); default PUID:PGID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Ro,
    Rw,
}

/// A shared folder of the NAS the user maps into the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Share {
    pub env: String,
    pub label: String,
    pub description: String,
    pub access: Access,
    #[serde(default)]
    pub required: bool,
    /// For optional shares: folder below the stack used when none is chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    /// `[A-Za-z0-9]`, `length` characters (safe for every app and URL).
    #[default]
    Alnum,
    /// `length` random bytes, hex encoded.
    Hex,
    /// `length` random bytes, standard base64 with padding.
    Base64,
}

/// A secret generated once at install and kept across updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    pub env: String,
    #[serde(default = "secret_len")]
    pub length: u16,
    #[serde(default)]
    pub encoding: Encoding,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// A value the user needs (an initial administrator password): the install answer shows
    /// it once. Other secrets are never shown by rynas (they stay in the stack's `.env`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub show: bool,
}

fn secret_len() -> u16 {
    32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuNeed {
    #[default]
    None,
    Optional,
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GpuKind {
    IntelQsv,
    Vaapi,
    Nvidia,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hardware {
    #[serde(default)]
    pub gpu: GpuNeed,
    #[serde(default)]
    pub gpu_kinds: Vec<GpuKind>,
    /// CPU features the images need (`x86-64-v2`, `avx`, `avx2`).
    #[serde(default)]
    pub cpu_features: Vec<String>,
}

/// Elevated access the compose file is allowed to request; lint refuses anything else.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Security {
    /// Services allowed `privileged: true`.
    #[serde(default)]
    pub privileged: Vec<String>,
    /// Services allowed to mount `/var/run/docker.sock` (implied for the main service of a
    /// manager).
    #[serde(default)]
    pub docker_socket: Vec<String>,
    /// Capabilities services may add (`NET_ADMIN`, …).
    #[serde(default)]
    pub cap_add: Vec<String>,
    /// Services allowed to map host devices that are not a GPU (a USB Zigbee or Z-Wave
    /// adapter, a Coral). Without it, `devices:` needs `hardware.gpu`.
    #[serde(default)]
    pub devices: Vec<String>,
}

/// A compose fragment merged into `compose.yaml` (RFC 7396 merge patch: maps merge, `null`
/// deletes, everything else replaces) when the value named by `when` is `true` (or, with a
/// leading `!`, not `true`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overlay {
    /// File below the entry, `overlays/<name>.yaml`.
    pub file: String,
    /// A boolean form field or provided variable, optionally prefixed with `!`.
    pub when: String,
    pub description: String,
}

impl Overlay {
    /// The variable and whether the condition is negated.
    pub fn condition(&self) -> (&str, bool) {
        match self.when.strip_prefix('!') {
            Some(v) => (v, true),
            None => (self.when.as_str(), false),
        }
    }
}

/// A named Docker volume. Named volumes live in the Docker data-root, outside snapshots and
/// backups, so only disposable content (caches, model downloads) may use them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedVolume {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteLevel {
    Info,
    Warning,
}

/// Shown in the install wizard before confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Note {
    pub level: NoteLevel,
    pub text: String,
}

/// `uid:gid` → numbers.
pub fn parse_owner(s: &str) -> Option<(u32, u32)> {
    let (u, g) = s.split_once(':')?;
    Some((u.parse().ok()?, g.parse().ok()?))
}

pub(crate) fn is_env_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_uppercase()
        && b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

pub(crate) fn is_app_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// A relative path made of `[a-z0-9._-]` segments, no `.`/`..`, no leading `/`.
pub(crate) fn is_rel_path(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
        })
}

impl AppManifest {
    /// Every `.env` name the entry declares (ports, folders, shares, secrets).
    pub fn declared_envs(&self) -> Vec<&str> {
        let mut v: Vec<&str> = Vec::new();
        v.extend(self.ports.iter().map(|p| p.env.as_str()));
        v.extend(self.folders.iter().map(|f| f.env.as_str()));
        v.extend(self.shares.iter().map(|s| s.env.as_str()));
        v.extend(self.secrets.iter().map(|s| s.env.as_str()));
        v
    }

    pub fn port(&self, name: &str) -> Option<&Port> {
        self.ports.iter().find(|p| p.name == name)
    }

    /// Checks the manifest on its own (cross-checks with compose and form live in lint).
    pub(crate) fn check(&self, folder: &str, issues: &mut Issues) {
        let f = "app.toml";
        let mut err = |m: String| issues.push(folder, f, m);
        if self.schema != SCHEMA {
            err(format!("schema {} is not supported (expected {SCHEMA})", self.schema));
        }
        if !is_app_id(&self.id) {
            err(format!("id `{}` must match [a-z0-9][a-z0-9-]*, at most 40 characters", self.id));
        }
        if self.id != folder {
            err(format!("id `{}` differs from the folder name `{folder}`", self.id));
        }
        for (k, v) in [("name", &self.name), ("app_version", &self.app_version), ("description", &self.description), ("license", &self.license), ("main_service", &self.main_service)] {
            if v.trim().is_empty() {
                err(format!("{k} is empty"));
            }
        }
        if self.description.len() > 140 {
            err("description is longer than 140 characters (the long text goes in description.md)".into());
        }
        for (k, v) in [("homepage", Some(&self.homepage)), ("changelog_url", Some(&self.changelog_url)), ("source", self.source.as_ref())] {
            if let Some(v) = v
                && !v.starts_with("https://")
            {
                err(format!("{k} must be an https:// URL"));
            }
        }
        if self.maintainers.is_empty() {
            err("maintainers is empty".into());
        }
        if self.categories.is_empty() {
            err("categories is empty".into());
        }
        if self.arch.is_empty() {
            err("arch is empty".into());
        }
        if self.resources.ram_min_mb == 0 || self.resources.ram_recommended_mb < self.resources.ram_min_mb {
            err("resources: ram_min_mb must be > 0 and ram_recommended_mb >= ram_min_mb".into());
        }

        // names: unique, env-shaped, not provided by rynas
        let mut envs = BTreeSet::new();
        for e in self.declared_envs() {
            if !is_env_name(e) {
                err(format!("`{e}` is not an env name ([A-Z][A-Z0-9_]*)"));
            }
            if PROVIDED_VARS.contains(&e) {
                err(format!("`{e}` is provided by rynas and cannot be declared"));
            }
            if !envs.insert(e) {
                err(format!("`{e}` is declared twice"));
            }
        }

        let mut names = BTreeSet::new();
        let mut hosts = BTreeSet::new();
        for p in &self.ports {
            if !is_app_id(&p.name) || !names.insert(p.name.as_str()) {
                err(format!("port name `{}` is invalid or duplicated", p.name));
            }
            if p.container == 0 || p.default == 0 {
                err(format!("port `{}`: ports must be 1-65535", p.name));
            }
            if !hosts.insert((p.default, p.protocol)) {
                err(format!("port `{}`: default host port {}/{} used twice", p.name, p.default, p.protocol.as_str()));
            }
            if self.host_network && (p.relocatable || p.container != p.default) {
                err(format!("port `{}`: with host_network the port is the app's own (container = default, relocatable = false)", p.name));
            }
            if p.purpose.trim().is_empty() {
                err(format!("port `{}`: purpose is empty", p.name));
            }
        }
        if let Some(w) = &self.web {
            match self.port(&w.port) {
                None => err(format!("web.port `{}` is not a declared port", w.port)),
                Some(p) if p.protocol != Protocol::Tcp => err(format!("web.port `{}` is not tcp", w.port)),
                _ => {}
            }
            if let Some(p) = self.port(&w.port)
                && !self.host_network
                && !p.proxied
            {
                err(format!("web.port `{}` must be proxied (bound to ${{RYNAS_BIND}})", w.port));
            }
            if !w.path.starts_with('/') {
                err("web.path must start with /".into());
            }
            if let Some(b) = &w.body
                && !size_ok(b)
            {
                err(format!("web.body `{b}` must be <digits>[K|M|G]"));
            }
            if let Some(t) = &w.timeout
                && !duration_ok(t)
            {
                err(format!("web.timeout `{t}` must be <digits>s|m|h"));
            }
            for b in &w.bypass {
                if !b.starts_with('/') || b.contains(char::is_whitespace) {
                    err(format!("web.bypass `{b}` must be a path starting with /"));
                }
            }
            if let Some(e) = &w.trusted_proxies_env
                && !is_env_name(e)
            {
                err(format!("web.trusted_proxies_env `{e}` is not an env name"));
            }
            if self.manager && w.public != Public::Never {
                err("a manager's web.public must be never".into());
            }
            if w.auth == WebAuth::None && w.public == Public::Ok {
                err("web.auth = none cannot be public = ok".into());
            }
        }
        // the web access proxy owns 80/443: only a reverse proxy app defaults to them, and
        // then as fixed, LAN-facing ports (it installs in external proxy mode only)
        let http_port = |p: &&Port| p.protocol == Protocol::Tcp && matches!(p.default, 80 | 443);
        if self.reverse_proxy {
            if self.host_network {
                err("reverse_proxy needs a bridged app (it publishes 80 and 443)".into());
            }
            for want in [80, 443] {
                match self.ports.iter().find(|p| p.protocol == Protocol::Tcp && p.default == want) {
                    Some(p) if !p.relocatable && !p.proxied => {}
                    Some(p) => err(format!("port `{}`: a reverse proxy's port {want} must be relocatable = false and not proxied", p.name)),
                    None => err(format!("reverse_proxy: no tcp port with default {want}")),
                }
            }
        } else if let Some(p) = self.ports.iter().find(http_port)
            && !self.host_network
        {
            err(format!("port `{}`: default host port {} belongs to the web access proxy (only a reverse_proxy app may use it)", p.name, p.default));
        }
        for p in &self.ports {
            if p.proxied && (self.host_network || p.protocol != Protocol::Tcp) {
                err(format!("port `{}`: only tcp ports of a bridged app can be proxied", p.name));
            }
        }

        let mut paths = BTreeSet::new();
        for fo in &self.folders {
            if !is_rel_path(&fo.path) {
                err(format!("folder {}: path `{}` must be relative, [a-z0-9._-] segments", fo.env, fo.path));
            }
            if !paths.insert(fo.path.as_str()) {
                err(format!("folder path `{}` used twice", fo.path));
            }
            if let Some(o) = &fo.owner
                && parse_owner(o).is_none()
            {
                err(format!("folder {}: owner `{o}` must be <uid>:<gid>", fo.env));
            }
        }
        for s in &self.shares {
            match (&s.fallback, s.required) {
                (Some(_), true) => err(format!("share {}: a required share has no fallback", s.env)),
                (None, false) => err(format!("share {}: an optional share needs a fallback folder", s.env)),
                (Some(p), false) if !is_rel_path(p) || !paths.insert(p.as_str()) => {
                    err(format!("share {}: fallback `{p}` is not a free relative path", s.env))
                }
                _ => {}
            }
        }
        for s in &self.secrets {
            if s.length < 16 || s.length > 256 {
                err(format!("secret {}: length must be 16-256", s.env));
            }
        }
        if self.hardware.gpu != GpuNeed::None && self.hardware.gpu_kinds.is_empty() {
            err("hardware: gpu set but gpu_kinds is empty".into());
        }
        if self.hardware.gpu == GpuNeed::Required && !self.antifeatures.contains(&Antifeature::NeedsGpu) {
            err("hardware.gpu = required needs the needs-gpu antifeature".into());
        }
        for o in &self.overlays {
            let (var, _) = o.condition();
            if !o.file.starts_with("overlays/") || !o.file.ends_with(".yaml") || !is_rel_path(&o.file) {
                err(format!("overlay `{}` must be overlays/<name>.yaml", o.file));
            }
            if !is_env_name(var) {
                err(format!("overlay `{}`: when `{}` is not a variable name", o.file, o.when));
            }
        }
        let elevated = self.manager || !self.security.privileged.is_empty() || !self.security.docker_socket.is_empty();
        if elevated && !self.antifeatures.contains(&Antifeature::RootAccess) {
            err("manager, privileged or docker_socket needs the root-access antifeature".into());
        }
        let ram_high = self.resources.ram_recommended_mb > 2048;
        if ram_high != self.antifeatures.contains(&Antifeature::HighResources) {
            err("high-resources antifeature must be set exactly when ram_recommended_mb > 2048".into());
        }
        let mut vols = BTreeSet::new();
        for v in &self.named_volumes {
            if !vols.insert(v.name.as_str()) {
                err(format!("named volume `{}` declared twice", v.name));
            }
        }
    }
}
