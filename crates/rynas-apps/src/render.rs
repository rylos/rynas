//! Rendering an entry into an ordinary stack folder: `compose.yaml` (the entry's compose
//! with the enabled overlays merged, `name:` and an `x-rynas` block added, `${VAR}`
//! references kept) and `.env` (every value). No templating: the only substitution is the
//! one `docker compose` itself does from `.env`.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use yaml_serde::{Mapping, Value};

use crate::catalog::CatalogApp;
use crate::compose::{self, ImageRef};
use crate::error::RenderError;
use crate::form::FieldType;
use crate::manifest::{AppManifest, Encoding, Intent, Ssd};
use crate::version::Version;

/// Name of the metadata block in the rendered compose file.
pub const X_RYNAS: &str = "x-rynas";

/// Values rynas provides to every render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provided {
    /// Absolute stack folder, `/<vol>/docker/stacks/<stack>`.
    pub appdata: String,
    /// Absolute stacks folder, `/<vol>/docker/stacks`.
    pub stacks_dir: String,
    /// Host name or address of the NAS as users reach it.
    pub host: String,
    pub puid: u32,
    pub pgid: u32,
    pub tz: String,
    pub docker_gid: u32,
    pub render_gid: u32,
    pub cpu_avx2: bool,
    /// Host address of proxied ports: `127.0.0.1` behind the rynas proxy, `0.0.0.0` when
    /// the user publishes the app on the LAN directly.
    pub bind: String,
    /// Addresses the app trusts for `X-Forwarded-*` (the proxy as seen from containers).
    pub trusted_proxies: String,
}

impl Provided {
    /// Generic example values (CI renders, documentation).
    pub fn example(stack: &str) -> Self {
        Self {
            appdata: format!("/volume1/docker/stacks/{stack}"),
            stacks_dir: "/volume1/docker/stacks".into(),
            host: "nas.example.lan".into(),
            puid: 1500,
            pgid: 1500,
            tz: "Etc/UTC".into(),
            docker_gid: 990,
            render_gid: 992,
            cpu_avx2: true,
            bind: "127.0.0.1".into(),
            trusted_proxies: "10.200.0.0/16".into(),
        }
    }

    fn vars(&self) -> Vec<(&'static str, String)> {
        vec![
            ("RYNAS_APPDATA", self.appdata.clone()),
            ("RYNAS_STACKS_DIR", self.stacks_dir.clone()),
            ("RYNAS_HOST", self.host.clone()),
            ("PUID", self.puid.to_string()),
            ("PGID", self.pgid.to_string()),
            ("TZ", self.tz.clone()),
            ("DOCKER_GID", self.docker_gid.to_string()),
            ("RENDER_GID", self.render_gid.to_string()),
            ("RYNAS_CPU_AVX2", self.cpu_avx2.to_string()),
            ("RYNAS_BIND", self.bind.clone()),
            ("RYNAS_TRUSTED_PROXIES", self.trusted_proxies.clone()),
        ]
    }
}

/// What the user chose in the wizard (or what an update recovered from `.env`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InstallRequest {
    /// Compose project name and stack folder name; usually the app id.
    pub stack: String,
    /// Form answers by property name.
    #[serde(default)]
    pub form: BTreeMap<String, serde_json::Value>,
    /// Share env → absolute host path of the chosen shared folder.
    #[serde(default)]
    pub shares: BTreeMap<String, String>,
    /// Folder env → absolute host path, for folders placed outside the stack (SSD volume).
    #[serde(default)]
    pub folders: BTreeMap<String, String>,
    /// Port env → host port, when not the default.
    #[serde(default)]
    pub ports: BTreeMap<String, u16>,
    /// Secret env → value already known (updates); the others are generated.
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
    /// The shares of the entry's `[share_set]`, in order (empty: none mounted).
    #[serde(default)]
    pub share_set: Vec<ShareSetItem>,
    /// The NAS user the app runs as (entries with `run_as`); `None`: the `docker-apps`
    /// account. The caller puts that user's uid and primary gid in [`Provided`]; the render
    /// keeps the name in `.env` ([`crate::manifest::RUN_AS_ENV`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_as: Option<String>,
}

/// One share of a `[share_set]`: mounted at `<target>/<name>` of the set's service.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareSetItem {
    /// Folder name inside the target (usually the share's name).
    pub name: String,
    /// Absolute host path, or `./<folder>` below the stack folder (the fallback folder of a
    /// layout with one optional share, kept by a migration).
    pub path: String,
    /// The bind follows mounts made on the host after the container started (propagation
    /// `rslave`): for an encrypted share whose plaintext mount comes and goes.
    #[serde(default)]
    pub follow: bool,
}

/// A share set item name: 1-64 characters, no `/ ; = ! $ ' " \`, whitespace or control
/// characters, not `.` or `..`.
pub fn valid_share_set_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s != "."
        && s != ".."
        && !s.chars().any(|c| c.is_control() || c.is_whitespace() || "/;=!$'\"\\".contains(c))
}

/// A share set item path: absolute (no `..`, no `:`/`'`, see [`abs_path_ok`]) or
/// `./<relative folder>`; never with `;`, a final `!` or control characters (the `.env`
/// encoding).
fn share_set_path_ok(p: &str) -> bool {
    let shape = match p.strip_prefix("./") {
        Some(rel) => crate::manifest::is_rel_path(rel),
        None => abs_path_ok(p) && p.len() > 1,
    };
    shape && !p.contains(';') && !p.ends_with('!') && !p.chars().any(char::is_control)
}

/// Checks a share set: valid and unique names, valid and unique paths.
pub fn check_share_set(field: &str, items: &[ShareSetItem]) -> Result<(), RenderError> {
    let bad = |reason: String| RenderError::InvalidValue { field: field.to_owned(), reason };
    let mut names = std::collections::BTreeSet::new();
    let mut paths = std::collections::BTreeSet::new();
    for it in items {
        if !valid_share_set_name(&it.name) {
            return Err(bad(format!("`{}` is not a valid folder name (no / ; = ! $ quotes or spaces, not . or ..)", it.name)));
        }
        if !share_set_path_ok(&it.path) {
            return Err(bad(format!("{}: `{}` must be an absolute path (or ./<folder> of the stack)", it.name, it.path)));
        }
        if !names.insert(it.name.as_str()) {
            return Err(bad(format!("the name `{}` is used twice", it.name)));
        }
        if !paths.insert(it.path.trim_end_matches('/')) {
            return Err(bad(format!("{} is chosen twice", it.path)));
        }
    }
    Ok(())
}

/// The `.env` value of a share set: `name=path` items joined by `;`, a trailing `!` on an
/// item that follows host mounts (`photos=/volume1/photos;vault=/volume1/vault!`). Names
/// never contain `= ; !` and paths never contain `;` nor end with `!`
/// ([`check_share_set`]), so the text splits back unambiguously ([`decode_share_set`]).
pub fn encode_share_set(items: &[ShareSetItem]) -> String {
    items
        .iter()
        .map(|i| format!("{}={}{}", i.name, i.path, if i.follow { "!" } else { "" }))
        .collect::<Vec<_>>()
        .join(";")
}

/// Reads [`encode_share_set`] back. An item without a name (`/volume1/sync`, `./sync`: the
/// value of a single-share variable renamed by a migration) takes the last component of
/// its path as name. Invalid items are errors.
pub fn decode_share_set(text: &str) -> Result<Vec<ShareSetItem>, String> {
    let out = split_share_set(text)?;
    check_share_set("share_set", &out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// [`decode_share_set`] without the final check (an update renders what it read, and the
/// render names the bad item).
fn split_share_set(text: &str) -> Result<Vec<ShareSetItem>, String> {
    let mut out = Vec::new();
    for raw in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (body, follow) = match raw.strip_suffix('!') {
            Some(b) => (b, true),
            None => (raw, false),
        };
        let (name, path) = if body.starts_with('/') || body.starts_with("./") {
            // the folder's own name, made a valid item name
            let last = body.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            let mut name: String = last.chars().map(|c| if c.is_control() || c.is_whitespace() || "/;=!$'\"\\".contains(c) { '_' } else { c }).collect();
            if !valid_share_set_name(&name) {
                name = "share".into();
            }
            (name, body.to_owned())
        } else {
            match body.split_once('=') {
                Some((n, p)) => (n.to_owned(), p.to_owned()),
                None => return Err(format!("`{raw}` is not name=path")),
            }
        };
        out.push(ShareSetItem { name, path, follow });
    }
    Ok(out)
}

/// Escapes `$` for a literal string in the compose file (compose interpolates `$`).
fn compose_literal(s: &str) -> String {
    s.replace('$', "$$")
}

/// Appends one long-syntax bind per share set item to the volumes of `service`.
fn add_share_set_volumes(doc: &mut Value, service: &str, target: &str, ro: bool, items: &[ShareSetItem]) -> Result<(), RenderError> {
    let svc = doc
        .get_mut("services")
        .and_then(|s| s.get_mut(service))
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| RenderError::Compose(format!("share_set service `{service}` is not in the compose file")))?;
    if !svc.contains_key("volumes") {
        svc.insert("volumes".into(), Value::Sequence(Vec::new()));
    }
    let Some(Value::Sequence(seq)) = svc.get_mut("volumes") else {
        return Err(RenderError::Compose(format!("service `{service}`: volumes is not a list")));
    };
    for it in items {
        let mut m = Mapping::new();
        m.insert("type".into(), "bind".into());
        m.insert("source".into(), Value::String(compose_literal(&it.path)));
        m.insert("target".into(), Value::String(format!("{}/{}", target.trim_end_matches('/'), it.name)));
        if ro {
            m.insert("read_only".into(), Value::Bool(true));
        }
        if it.follow {
            let mut b = Mapping::new();
            b.insert("propagation".into(), "rslave".into());
            m.insert("bind".into(), Value::Mapping(b));
        }
        seq.push(Value::Mapping(m));
    }
    Ok(())
}

/// One file of the stack folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFile {
    /// Relative to the stack folder.
    pub path: String,
    pub contents: Vec<u8>,
    pub mode: u32,
}

/// A host folder to create (owned by PUID:PGID) before `up`, or a share to check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderPlan {
    pub env: String,
    /// Absolute host path.
    pub host_path: String,
    pub intent: Option<Intent>,
    pub ssd: Ssd,
    /// A user's shared folder: must exist, never created or chowned.
    pub share: bool,
    /// Owner of a folder to create (`uid`, `gid`); PUID:PGID unless app.toml says otherwise.
    pub owner: (u32, u32),
    /// Created without copy-on-write when it lands on btrfs (`nocow` in app.toml).
    pub nocow: bool,
}

/// The `x-rynas` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XRynas {
    pub app: String,
    pub version: Version,
    pub app_version: String,
    pub catalog: String,
}

/// A rendered stack, ready for "create stack from files".
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedStack {
    pub stack: String,
    pub meta: XRynas,
    /// `compose.yaml` (0644) and `.env` (0600).
    pub files: Vec<StackFile>,
    pub folders: Vec<FolderPlan>,
    /// Every value written to `.env`.
    pub env: BTreeMap<String, String>,
    /// Image of every service.
    pub images: Vec<(String, ImageRef)>,
    /// Overlay files merged in.
    pub overlays: Vec<String>,
}

impl RenderedStack {
    pub fn file(&self, path: &str) -> Option<&StackFile> {
        self.files.iter().find(|f| f.path == path)
    }
}

/// Compose project name rules: `[a-z0-9][a-z0-9_-]*`.
pub fn valid_stack_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

fn abs_path_ok(p: &str) -> bool {
    p.starts_with('/') && !p.split('/').any(|s| s == "..") && !p.contains(['\n', '\0', '\'', ':'])
}

/// Generates one secret from system randomness.
pub fn generate_secret(len: u16, enc: Encoding) -> Result<String, RenderError> {
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let len = len as usize;
    match enc {
        Encoding::Alnum => {
            let mut out = String::with_capacity(len);
            let mut buf = [0u8; 64];
            while out.len() < len {
                getrandom::fill(&mut buf).map_err(|e| RenderError::Random(e.to_string()))?;
                // rejection sampling: 248 = 62 * 4, no modulo bias
                for b in buf.iter().filter(|b| **b < 248) {
                    if out.len() == len {
                        break;
                    }
                    out.push(ALNUM[(*b % 62) as usize] as char);
                }
            }
            Ok(out)
        }
        Encoding::Hex | Encoding::Base64 => {
            let mut bytes = vec![0u8; len];
            getrandom::fill(&mut bytes).map_err(|e| RenderError::Random(e.to_string()))?;
            Ok(if enc == Encoding::Hex {
                bytes.iter().map(|b| format!("{b:02x}")).collect()
            } else {
                base64::engine::general_purpose::STANDARD.encode(bytes)
            })
        }
    }
}

/// Quotes a value for `.env` (compose dotenv: single quotes are literal).
fn env_value(key: &str, v: &str) -> Result<String, RenderError> {
    // a backslash before the closing quote: compose reads an escaped quote, and the value
    // runs on into the next lines
    if v.contains(['\n', '\r', '\0', '\'']) || v.ends_with('\\') {
        return Err(RenderError::UnsafeValue(key.to_owned()));
    }
    let plain = v.bytes().all(|c| c.is_ascii_alphanumeric() || b"_./:@+=,%-".contains(&c));
    Ok(if plain { v.to_owned() } else { format!("'{v}'") })
}

/// Parses a `.env` file as written by [`render`] (and the plain `KEY=value` /
/// `KEY="value"` lines a user may add).
pub fn parse_env(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let l = line.trim_start();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let l = l.strip_prefix("export ").unwrap_or(l);
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim_end();
        let v = if let Some(x) = v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
            x.to_owned()
        } else if let Some(x) = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
            x.replace("\\\"", "\"").replace("\\\\", "\\")
        } else {
            // unquoted: an inline comment starts at " #"
            v.split(" #").next().unwrap_or("").trim_end().to_owned()
        };
        out.insert(k.trim().to_owned(), v);
    }
    out
}

/// The compose document with the overlays whose condition holds in `env` merged in.
pub(crate) fn merged_compose(app: &CatalogApp, env: &BTreeMap<String, String>) -> (Value, Vec<String>) {
    let mut doc = app.compose.clone();
    let mut applied = Vec::new();
    for o in &app.overlays {
        let (var, negated) = o.spec.condition();
        let on = env.get(var).map(String::as_str) == Some("true");
        if on != negated {
            compose::merge_patch(&mut doc, &o.patch);
            applied.push(o.spec.file.clone());
        }
    }
    (doc, applied)
}

/// Renders an entry. `catalog` is the source id written to `x-rynas.catalog`.
pub fn render(app: &CatalogApp, catalog: &str, provided: &Provided, req: &InstallRequest) -> Result<RenderedStack, RenderError> {
    let m: &AppManifest = &app.manifest;
    if !valid_stack_name(&req.stack) {
        return Err(RenderError::InvalidStackName(req.stack.clone()));
    }
    for (k, known) in [
        (req.shares.keys().find(|k| !m.shares.iter().any(|s| &s.env == *k)), "share"),
        (req.folders.keys().find(|k| !m.folders.iter().any(|f| &f.env == *k)), "folder"),
        (req.ports.keys().find(|k| !m.ports.iter().any(|p| &p.env == *k)), "port"),
        (req.secrets.keys().find(|k| !m.secrets.iter().any(|s| &s.env == *k)), "secret"),
    ] {
        if let Some(k) = k {
            return Err(RenderError::UnknownValue(format!("{known} {k}")));
        }
    }
    if m.share_set.is_none() && !req.share_set.is_empty() {
        return Err(RenderError::UnknownValue("share_set".into()));
    }
    match (&req.run_as, m.run_as) {
        (Some(_), None) => return Err(RenderError::UnknownValue("run_as".into())),
        (Some(u), Some(_)) if !crate::manifest::is_user_name(u) => {
            return Err(RenderError::InvalidValue { field: "run_as".into(), reason: format!("`{u}` is not a user name") });
        }
        (Some(_), Some(_)) if provided.puid == 0 || provided.pgid == 0 => {
            return Err(RenderError::InvalidValue { field: "run_as".into(), reason: "an app never runs as root".into() });
        }
        _ => {}
    }
    let form_values = app.form.resolve(&req.form)?;

    let mut env: Vec<(String, String)> = provided.vars().into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    if let Some(u) = &req.run_as {
        env.push((crate::manifest::RUN_AS_ENV.to_owned(), u.clone()));
    }
    let mut folders = Vec::new();
    for p in &m.ports {
        let port = match req.ports.get(&p.env) {
            Some(_) if m.host_network || !p.relocatable => {
                return Err(RenderError::InvalidValue { field: p.env.clone(), reason: "this port cannot be moved".into() });
            }
            Some(0) => return Err(RenderError::InvalidValue { field: p.env.clone(), reason: "port 0".into() }),
            Some(n) => *n,
            None => p.default,
        };
        env.push((p.env.clone(), port.to_string()));
    }
    let appdata = provided.appdata.trim_end_matches('/');
    for f in &m.folders {
        let (value, host) = match req.folders.get(&f.env) {
            Some(p) if !abs_path_ok(p) => {
                return Err(RenderError::InvalidValue { field: f.env.clone(), reason: "must be an absolute path".into() });
            }
            Some(p) => (p.clone(), p.clone()),
            None => (format!("./{}", f.path), format!("{appdata}/{}", f.path)),
        };
        env.push((f.env.clone(), value));
        let owner = f.owner.as_deref().and_then(crate::manifest::parse_owner).unwrap_or((provided.puid, provided.pgid));
        folders.push(FolderPlan { env: f.env.clone(), host_path: host, intent: Some(f.intent), ssd: f.ssd, share: false, owner, nocow: f.nocow });
    }
    for s in &m.shares {
        match (req.shares.get(&s.env), &s.fallback) {
            (Some(p), _) if !abs_path_ok(p) => {
                return Err(RenderError::InvalidValue { field: s.env.clone(), reason: "must be an absolute path".into() });
            }
            (Some(p), _) => {
                env.push((s.env.clone(), p.clone()));
                folders.push(FolderPlan { env: s.env.clone(), host_path: p.clone(), intent: None, ssd: Ssd::No, share: true, owner: (provided.puid, provided.pgid), nocow: false });
            }
            (None, Some(fb)) => {
                env.push((s.env.clone(), format!("./{fb}")));
                folders.push(FolderPlan { env: s.env.clone(), host_path: format!("{appdata}/{fb}"), intent: Some(Intent::Data), ssd: Ssd::No, share: false, owner: (provided.puid, provided.pgid), nocow: false });
            }
            (None, None) => return Err(RenderError::MissingValue(s.env.clone())),
        }
    }
    if let Some(ss) = &m.share_set {
        check_share_set(&ss.env, &req.share_set)?;
        env.push((ss.env.clone(), encode_share_set(&req.share_set)));
        for it in &req.share_set {
            let env = format!("{}/{}", ss.env, it.name);
            let owner = (provided.puid, provided.pgid);
            folders.push(match it.path.strip_prefix("./") {
                // a folder of the stack (a kept fallback): made like the app's own folders
                Some(rel) => FolderPlan { env, host_path: format!("{appdata}/{rel}"), intent: Some(Intent::Data), ssd: Ssd::No, share: false, owner, nocow: false },
                None => FolderPlan { env, host_path: it.path.clone(), intent: None, ssd: Ssd::No, share: true, owner, nocow: false },
            });
        }
    }
    for s in &m.secrets {
        let v = match req.secrets.get(&s.env) {
            Some(v) if !v.is_empty() => v.clone(),
            _ => generate_secret(s.length, s.encoding)?,
        };
        env.push((s.env.clone(), v));
    }
    env.extend(form_values);
    let env_map: BTreeMap<String, String> = env.iter().cloned().collect();

    let (mut doc, applied) = merged_compose(app, &env_map);
    if let Some(ss) = &m.share_set {
        add_share_set_volumes(&mut doc, &ss.service, &ss.target, ss.access == crate::manifest::Access::Ro, &req.share_set)?;
    }
    for r in compose::refs(&doc).map_err(RenderError::Compose)? {
        if !env_map.contains_key(&r.name) {
            return Err(RenderError::UndefinedVariable(r.name));
        }
    }
    let mut images = Vec::new();
    for (svc, img) in compose::images(&doc) {
        images.push((svc, ImageRef::parse(&img).map_err(RenderError::Compose)?));
    }

    let meta = XRynas { app: m.id.clone(), version: m.version, app_version: m.app_version.clone(), catalog: catalog.to_owned() };
    let mut out = Mapping::new();
    out.insert("name".into(), Value::String(req.stack.clone()));
    if let Value::Mapping(body) = doc {
        for (k, v) in body {
            out.insert(k, v);
        }
    }
    out.insert(X_RYNAS.into(), yaml_serde::to_value(&meta).map_err(|e| RenderError::Compose(e.to_string()))?);
    let yaml = yaml_serde::to_string(&Value::Mapping(out)).map_err(|e| RenderError::Compose(e.to_string()))?;
    let compose_text = format!(
        "# {} {} from the rynas app catalogue ({catalog}).\n\
         # An ordinary compose stack: any manager may edit it. Values live in .env.\n\
         # Removing the x-rynas block turns it into a custom stack.\n{yaml}",
        m.name, m.version
    );

    let mut env_text = format!("# Values of the {} stack ({} {}). Secrets were generated at install.\n", req.stack, m.id, m.version);
    for (k, v) in &env {
        env_text.push_str(&format!("{k}={}\n", env_value(k, v)?));
    }

    Ok(RenderedStack {
        stack: req.stack.clone(),
        meta,
        files: vec![
            StackFile { path: "compose.yaml".into(), contents: compose_text.into_bytes(), mode: 0o644 },
            StackFile { path: ".env".into(), contents: env_text.into_bytes(), mode: 0o600 },
        ],
        folders,
        env: env_map,
        images,
        overlays: applied,
    })
}

/// Reads the `x-rynas` block of an installed compose file; `None` for a custom stack.
pub fn installed_meta(compose_text: &str) -> Result<Option<XRynas>, RenderError> {
    let doc: Value = yaml_serde::from_str(compose_text).map_err(|e| RenderError::NotInstalledApp(e.to_string()))?;
    match doc.get(X_RYNAS) {
        None => Ok(None),
        Some(v) => yaml_serde::from_value(v.clone()).map(Some).map_err(|e| RenderError::NotInstalledApp(e.to_string())),
    }
}

/// "Convert to custom stack": the same compose without `x-rynas`.
pub fn convert_to_custom(compose_text: &str) -> Result<String, RenderError> {
    let mut doc: Value = yaml_serde::from_str(compose_text).map_err(|e| RenderError::Compose(e.to_string()))?;
    if let Value::Mapping(m) = &mut doc {
        m.remove(X_RYNAS);
    }
    yaml_serde::to_string(&doc).map_err(|e| RenderError::Compose(e.to_string()))
}

/// Rebuilds the install request of an installed stack from its `.env` values (after
/// migrations), so an update re-renders with the user's answers and secrets.
pub fn request_from_env(app: &CatalogApp, stack: &str, env: &BTreeMap<String, String>) -> InstallRequest {
    let m = &app.manifest;
    let mut req = InstallRequest { stack: stack.to_owned(), ..Default::default() };
    for p in &m.ports {
        if let Some(n) = env.get(&p.env).and_then(|v| v.parse::<u16>().ok())
            && n != p.default
            && p.relocatable
            && !m.host_network
        {
            req.ports.insert(p.env.clone(), n);
        }
    }
    for f in &m.folders {
        if let Some(v) = env.get(&f.env)
            && v.starts_with('/')
        {
            req.folders.insert(f.env.clone(), v.clone());
        }
    }
    for s in &m.shares {
        if let Some(v) = env.get(&s.env)
            && v.starts_with('/')
        {
            req.shares.insert(s.env.clone(), v.clone());
        }
    }
    for s in &m.secrets {
        if let Some(v) = env.get(&s.env) {
            req.secrets.insert(s.env.clone(), v.clone());
        }
    }
    if m.run_as.is_some() {
        req.run_as = env.get(crate::manifest::RUN_AS_ENV).filter(|u| !u.is_empty()).cloned();
    }
    if let Some(ss) = &m.share_set
        && let Some(v) = env.get(&ss.env)
    {
        // a broken line is kept as read: the render then names the bad item instead of
        // silently unmounting the shares
        req.share_set = split_share_set(v).unwrap_or_else(|_| vec![ShareSetItem { name: String::new(), path: v.clone(), follow: false }]);
    }
    for (name, field) in &app.form.properties {
        let Some(v) = env.get(name) else { continue };
        let json = match field.ty {
            FieldType::Boolean => serde_json::Value::Bool(v == "true"),
            FieldType::Integer => match v.parse::<i64>() {
                Ok(n) => serde_json::Value::from(n),
                Err(_) => continue,
            },
            FieldType::String if v.is_empty() => continue,
            FieldType::String => serde_json::Value::String(v.clone()),
        };
        req.form.insert(name.clone(), json);
    }
    req
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_values_are_plain_quoted_or_refused() {
        assert_eq!(env_value("K", "plain-1.2:/x").unwrap(), "plain-1.2:/x");
        assert_eq!(env_value("K", "a b$c").unwrap(), "'a b$c'");
        assert_eq!(env_value("K", "in\\side").unwrap(), "'in\\side'");
        for bad in ["a'b", "a\nb", "ends\\", "ends\\\\"] {
            assert_eq!(env_value("K", bad), Err(RenderError::UnsafeValue("K".into())), "{bad:?}");
        }
    }

    fn item(name: &str, path: &str, follow: bool) -> ShareSetItem {
        ShareSetItem { name: name.into(), path: path.into(), follow }
    }

    #[test]
    fn share_set_round_trip() {
        let items = vec![item("photos", "/volume1/photos", false), item("vault", "/volume2/vault", true), item("odd=dir", "/v/a=b", false)];
        assert!(check_share_set("S", &items).is_err(), "= in a name");
        let items = vec![item("photos", "/volume1/photos", false), item("vault", "/volume2/my vault=x", true), item("sync", "./sync", false)];
        let text = encode_share_set(&items);
        assert_eq!(text, "photos=/volume1/photos;vault=/volume2/my vault=x!;sync=./sync");
        assert_eq!(decode_share_set(&text).unwrap(), items);
        // through .env quoting and back
        let line = env_value("S", &text).unwrap();
        assert!(line.starts_with('\''));
        assert_eq!(parse_env(&format!("S={line}\n"))["S"], text);
        assert_eq!(decode_share_set("").unwrap(), vec![]);
        assert_eq!(encode_share_set(&[]), "");
    }

    #[test]
    fn share_set_bare_paths_from_a_single_share_layout() {
        assert_eq!(decode_share_set("/volume1/sync").unwrap(), vec![item("sync", "/volume1/sync", false)]);
        assert_eq!(decode_share_set("./sync").unwrap(), vec![item("sync", "./sync", false)]);
        assert_eq!(decode_share_set("/volume1/My Files/").unwrap(), vec![item("My_Files", "/volume1/My Files/", false)]);
    }

    #[test]
    fn share_set_validation() {
        for (items, needle) in [
            (vec![item("", "/a", false)], "valid folder name"),
            (vec![item("..", "/a", false)], "valid folder name"),
            (vec![item("a/b", "/a", false)], "valid folder name"),
            (vec![item("a;b", "/a", false)], "valid folder name"),
            (vec![item("a!", "/a", false)], "valid folder name"),
            (vec![item("a\tb", "/a", false)], "valid folder name"),
            (vec![item("a", "relative", false)], "absolute path"),
            (vec![item("a", "/x/../etc", false)], "absolute path"),
            (vec![item("a", "/x;y", false)], "absolute path"),
            (vec![item("a", "/x!", false)], "absolute path"),
            (vec![item("a", "./../x", false)], "absolute path"),
            (vec![item("a", "/x", false), item("a", "/y", false)], "used twice"),
            (vec![item("a", "/x", false), item("b", "/x/", false)], "chosen twice"),
        ] {
            let e = check_share_set("S", &items).unwrap_err().to_string();
            assert!(e.contains(needle), "{items:?}: {e}");
            assert!(e.starts_with("S: "), "{e}");
        }
        assert!(decode_share_set("noequals").is_err());
        assert!(check_share_set("S", &[item("a", "/x", true), item("b", "./y", false)]).is_ok());
    }
}
