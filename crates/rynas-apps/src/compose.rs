//! Compose file helpers: `${VAR}` references, image references, RFC 7396 merge patches of
//! overlays, and the structural lint of a catalogue compose file against its `app.toml`.

use std::collections::BTreeSet;
use std::fmt;

use yaml_serde::{Mapping, Value};

use crate::error::Issues;
use crate::manifest::{Access, AppManifest, GpuNeed, PROVIDED_VARS, Protocol};

pub fn parse(text: &str) -> Result<Value, String> {
    let v: Value = yaml_serde::from_str(text).map_err(|e| e.to_string())?;
    if !v.is_mapping() {
        return Err("the document is not a mapping".into());
    }
    Ok(v)
}

/// A `$` reference found in a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarRef {
    pub name: String,
    /// `${NAME}` exactly (no default, no error text, not bare `$NAME`).
    pub plain: bool,
}

/// The variable references of one string, following compose interpolation (`$$` escapes).
pub fn refs_in(s: &str) -> Result<Vec<VarRef>, String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'$' {
            i += 1;
            continue;
        }
        match b.get(i + 1) {
            Some(b'$') => i += 2,
            Some(b'{') => {
                let end = s[i + 2..].find('}').ok_or_else(|| format!("unterminated ${{ in `{s}`"))? + i + 2;
                let inner = &s[i + 2..end];
                let name_len = inner.bytes().take_while(|c| c.is_ascii_alphanumeric() || *c == b'_').count();
                if name_len == 0 {
                    return Err(format!("invalid ${{…}} in `{s}`"));
                }
                out.push(VarRef { name: inner[..name_len].to_owned(), plain: name_len == inner.len() });
                i = end + 1;
            }
            Some(c) if c.is_ascii_alphabetic() || *c == b'_' => {
                let n = b[i + 1..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_').count();
                out.push(VarRef { name: s[i + 1..i + 1 + n].to_owned(), plain: false });
                i += 1 + n;
            }
            _ => return Err(format!("lone `$` in `{s}` (write $$)")),
        }
    }
    Ok(out)
}

/// Every reference in every string (keys included) of a YAML value.
pub fn refs(v: &Value) -> Result<Vec<VarRef>, String> {
    let mut out = Vec::new();
    walk_strings(v, &mut |s| {
        out.extend(refs_in(s)?);
        Ok(())
    })?;
    Ok(out)
}

fn walk_strings(v: &Value, f: &mut dyn FnMut(&str) -> Result<(), String>) -> Result<(), String> {
    match v {
        Value::String(s) => f(s),
        Value::Sequence(seq) => seq.iter().try_for_each(|x| walk_strings(x, f)),
        Value::Mapping(m) => m.iter().try_for_each(|(k, x)| {
            walk_strings(k, f)?;
            walk_strings(x, f)
        }),
        Value::Tagged(t) => walk_strings(&t.value, f),
        _ => Ok(()),
    }
}

/// An image reference `[registry/]repository[:tag][@sha256:<hex>]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ImageRef {
    /// Registry and repository, as written (`ghcr.io/immich-app/immich-server`, `postgres`).
    pub name: String,
    pub tag: Option<String>,
    /// `sha256:<64 hex>`.
    pub digest: Option<String>,
}

impl ImageRef {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (rest, digest) = match s.split_once('@') {
            Some((r, d)) => {
                let hex = d.strip_prefix("sha256:").ok_or_else(|| format!("digest of `{s}` is not sha256"))?;
                if hex.len() != 64 || !hex.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
                    return Err(format!("digest of `{s}` is not 64 lowercase hex characters"));
                }
                (r, Some(d.to_owned()))
            }
            None => (s, None),
        };
        // a ':' after the last '/' is the tag (a ':' before it is a registry port)
        let slash = rest.rfind('/').map_or(0, |p| p + 1);
        let (name, tag) = match rest[slash..].find(':') {
            Some(p) => (&rest[..slash + p], Some(rest[slash + p + 1..].to_owned())),
            None => (rest, None),
        };
        if name.is_empty() || tag.as_deref() == Some("") {
            return Err(format!("`{s}` is not an image reference"));
        }
        Ok(Self { name: name.to_owned(), tag, digest })
    }

    /// Name without the default registry prefixes (`docker.io/`, `docker.io/library/`).
    pub fn normalized_name(&self) -> &str {
        let n = self.name.strip_prefix("docker.io/").unwrap_or(&self.name);
        n.strip_prefix("library/").unwrap_or(n)
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

/// RFC 7396 merge patch on YAML values: mappings merge recursively, `null` deletes the key,
/// anything else replaces.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Mapping(p) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_mapping() {
        *target = Value::Mapping(Mapping::new());
    }
    let Value::Mapping(t) = target else { unreachable!() };
    for (k, v) in p {
        if v.is_null() {
            t.remove(k);
        } else {
            match t.get_mut(k) {
                Some(existing) => merge_patch(existing, v),
                None => {
                    let mut fresh = Value::Null;
                    merge_patch(&mut fresh, v);
                    t.insert(k.clone(), fresh);
                }
            }
        }
    }
}

/// The services mapping, in file order.
pub fn services(doc: &Value) -> Vec<(String, &Value)> {
    doc.get("services")
        .and_then(Value::as_mapping)
        .map(|m| m.iter().filter_map(|(k, v)| Some((k.as_str()?.to_owned(), v))).collect())
        .unwrap_or_default()
}

/// The image of every service, in file order.
pub fn images(doc: &Value) -> Vec<(String, String)> {
    services(doc)
        .into_iter()
        .filter_map(|(n, s)| Some((n, s.get("image")?.as_str()?.to_owned())))
        .collect()
}

const TOP_KEYS: &[&str] = &["services", "volumes", "networks"];
const SERVICE_FORBIDDEN: &[&str] = &["build", "container_name", "extends", "pid", "ipc", "userns_mode", "cgroup_parent", "volumes_from"];
/// Host paths any entry may bind (read-only unless noted in the lint).
const HOST_PATHS_RO: &[&str] = &["/etc/localtime", "/run/dbus"];
const DOCKER_SOCK: &[&str] = &["/var/run/docker.sock", "/run/docker.sock"];

/// Short-syntax volume entry split into (source, target, mode).
fn split_volume(s: &str) -> (&str, &str, &str) {
    let mut parts = s.splitn(3, ':');
    let src = parts.next().unwrap_or("");
    let dst = parts.next().unwrap_or("");
    let mode = parts.next().unwrap_or("");
    (src, dst, mode)
}

/// Lints one (base or overlay-merged) compose document. `label` names the variant in
/// messages; `known` is every variable a render provides.
pub(crate) fn lint(app: &AppManifest, doc: &Value, label: &str, known: &BTreeSet<String>, issues: &mut Issues) {
    let id = app.id.as_str();
    let mut err = |m: String| issues.push(id, label, m);
    let Some(top) = doc.as_mapping() else {
        err("not a mapping".into());
        return;
    };
    for k in top.keys() {
        match k.as_str() {
            Some(k) if TOP_KEYS.contains(&k) => {}
            Some(k) => err(format!("top-level key `{k}` is not allowed (only services, volumes, networks; rynas writes name and x-rynas)")),
            None => err("non-string top-level key".into()),
        }
    }
    let svcs = services(doc);
    if svcs.is_empty() {
        err("no services".into());
    }
    let names: BTreeSet<&str> = svcs.iter().map(|(n, _)| n.as_str()).collect();
    if !names.contains(app.main_service.as_str()) {
        err(format!("main_service `{}` is not a service", app.main_service));
    }
    for s in app.security.privileged.iter().chain(&app.security.docker_socket).chain(app.ports.iter().map(|p| &p.service)) {
        if !names.contains(s.as_str()) {
            err(format!("app.toml names service `{s}`, which is not in the compose file"));
        }
    }

    // variables
    match refs(doc) {
        Err(e) => err(e),
        Ok(rs) => {
            for r in rs {
                if !r.plain {
                    err(format!("`{}`: only ${{VAR}} is allowed (no defaults, no bare $VAR)", r.name));
                }
                if !known.contains(&r.name) {
                    err(format!("${{{}}} is not provided by rynas, app.toml or form.json", r.name));
                }
            }
        }
    }

    // named volumes
    let declared_vols: BTreeSet<&str> = app.named_volumes.iter().map(|v| v.name.as_str()).collect();
    let top_vols: BTreeSet<String> = doc
        .get("volumes")
        .and_then(Value::as_mapping)
        .map(|m| m.keys().filter_map(|k| k.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    for v in &top_vols {
        if !declared_vols.contains(v.as_str()) {
            err(format!("named volume `{v}` is not in app.toml named_volumes (data must be bind folders)"));
        }
    }
    let share_access = |src: &str| app.shares.iter().find(|s| format!("${{{}}}", s.env) == src).map(|s| s.access);
    let folder_env = |src: &str| app.folders.iter().any(|f| format!("${{{}}}", f.env) == src);

    let mut published: Vec<(String, Published)> = Vec::new();
    for (name, svc) in &svcs {
        let Some(m) = svc.as_mapping() else {
            err(format!("service `{name}` is not a mapping"));
            continue;
        };
        let get = |k: &str| m.get(k);
        for k in SERVICE_FORBIDDEN {
            if get(k).is_some() {
                err(format!("service `{name}`: `{k}` is not allowed"));
            }
        }
        // image
        match get("image").and_then(Value::as_str) {
            None => err(format!("service `{name}` has no image")),
            Some(img) if img.contains('$') => err(format!("service `{name}`: the image must be literal, no variables")),
            Some(img) => match ImageRef::parse(img) {
                Err(e) => err(format!("service `{name}`: {e}")),
                Ok(r) => {
                    match r.tag.as_deref() {
                        None => err(format!("service `{name}`: image `{img}` has no tag")),
                        Some("latest" | "stable" | "release" | "main" | "master" | "nightly" | "edge") => {
                            err(format!("service `{name}`: image `{img}` uses a moving tag"))
                        }
                        _ => {}
                    }
                    if r.digest.is_none() {
                        err(format!("service `{name}`: image `{img}` is not pinned by digest"));
                    }
                    if *name == app.main_service
                        && let Some(t) = &r.tag
                        && t != &app.app_version
                        && !t.starts_with(&format!("{}-", app.app_version))
                    {
                        err(format!("main image tag `{t}` does not match app_version `{}`", app.app_version));
                    }
                }
            },
        }
        match get("restart").and_then(Value::as_str) {
            Some("unless-stopped" | "always" | "on-failure") => {}
            _ => err(format!("service `{name}`: restart must be unless-stopped, always or on-failure")),
        }
        if get("privileged").and_then(Value::as_bool) == Some(true) && !app.security.privileged.contains(name) {
            err(format!("service `{name}`: privileged is not declared in app.toml security.privileged"));
        }
        let host_net = get("network_mode").and_then(Value::as_str) == Some("host");
        if let Some(nm) = get("network_mode").and_then(Value::as_str)
            && nm != "host"
            && !nm.starts_with("service:")
        {
            err(format!("service `{name}`: network_mode `{nm}` is not allowed"));
        }
        if host_net != app.host_network {
            err(format!("service `{name}`: network_mode host must match app.toml host_network"));
        }
        if let Some(caps) = get("cap_add").and_then(Value::as_sequence) {
            for c in caps.iter().filter_map(Value::as_str) {
                if !app.security.cap_add.iter().any(|a| a == c) {
                    err(format!("service `{name}`: cap_add {c} is not declared in app.toml security.cap_add"));
                }
            }
        }
        if get("devices").is_some() && app.hardware.gpu == GpuNeed::None {
            err(format!("service `{name}`: devices need hardware.gpu in app.toml"));
        }
        if let Some(ef) = get("env_file") {
            let ok = match ef {
                Value::String(s) => s == ".env",
                Value::Sequence(v) => v.iter().all(|x| x.as_str() == Some(".env")),
                _ => false,
            };
            if !ok {
                err(format!("service `{name}`: env_file may only be .env"));
            }
        }
        // ports
        if let Some(ports) = get("ports") {
            if app.host_network {
                err(format!("service `{name}`: ports with host_network"));
            }
            for p in ports.as_sequence().into_iter().flatten() {
                match parse_port(p) {
                    Ok(p) => published.push((name.clone(), p)),
                    Err(e) => err(format!("service `{name}`: {e}")),
                }
            }
        }
        // volumes
        for v in get("volumes").and_then(Value::as_sequence).into_iter().flatten() {
            let (src, mode) = match v {
                Value::String(s) => {
                    let (src, _dst, mode) = split_volume(s);
                    (src.to_owned(), mode.to_owned())
                }
                Value::Mapping(vm) => {
                    let src = vm.get("source").and_then(Value::as_str).unwrap_or("").to_owned();
                    let ro = vm.get("read_only").and_then(Value::as_bool) == Some(true);
                    (src, if ro { "ro".into() } else { String::new() })
                }
                _ => {
                    err(format!("service `{name}`: unreadable volume entry"));
                    continue;
                }
            };
            let ro = mode.split(',').any(|m| m == "ro");
            if DOCKER_SOCK.contains(&src.as_str()) {
                let allowed = app.security.docker_socket.contains(name) || (app.manager && *name == app.main_service);
                if !allowed {
                    err(format!("service `{name}`: Docker socket mount is not declared (manager or security.docker_socket)"));
                }
            } else if HOST_PATHS_RO.contains(&src.as_str()) {
                if !ro {
                    err(format!("service `{name}`: {src} must be mounted read-only"));
                }
            } else if src == "${RYNAS_STACKS_DIR}" {
                if !app.manager {
                    err(format!("service `{name}`: only a manager mounts the stacks folder"));
                }
            } else if let Some(access) = share_access(&src) {
                if (access == Access::Ro) != ro {
                    err(format!("service `{name}`: mount of {src} must {}be :ro as declared", if ro { "not " } else { "" }));
                }
            } else if !(folder_env(&src) || top_vols.contains(&src)) {
                err(format!("service `{name}`: volume source `{src}` must be a declared folder, share or named volume"));
            }
        }
    }

    // managers: socket + same-path stacks folder
    if app.manager {
        let main = svcs.iter().find(|(n, _)| *n == app.main_service).map(|(_, s)| *s);
        let vols: Vec<String> = main
            .and_then(|s| s.get("volumes"))
            .and_then(Value::as_sequence)
            .map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        if !vols.iter().any(|v| DOCKER_SOCK.contains(&split_volume(v).0)) {
            err("a manager must mount the Docker socket".into());
        }
        if !vols.iter().any(|v| {
            let (s, d, _) = split_volume(v);
            s == "${RYNAS_STACKS_DIR}" && d == "${RYNAS_STACKS_DIR}"
        }) {
            err("a manager must mount ${RYNAS_STACKS_DIR}:${RYNAS_STACKS_DIR} (same path inside and outside)".into());
        }
    }

    // ports declared in app.toml == ports published in compose
    if !app.host_network {
        for p in &app.ports {
            let n = published.iter().filter(|(s, q)| *s == p.service && q.env == p.env && q.container == p.container && q.proto == p.protocol).count();
            if n != 1 {
                err(format!("port `{}` (${{{}}}:{}/{} on {}) is published {n} times in compose", p.name, p.env, p.container, p.protocol.as_str(), p.service));
            }
        }
        for (s, q) in &published {
            match app.ports.iter().find(|p| p.service == *s && p.env == q.env && p.container == q.container && p.protocol == q.proto) {
                None => err(format!("compose publishes ${{{}}}:{}/{} on {s}, which app.toml does not declare", q.env, q.container, q.proto.as_str())),
                Some(p) if p.proxied != q.bound => err(format!(
                    "port `{}`: {}",
                    p.name,
                    if p.proxied { "a proxied port must be published as \"${RYNAS_BIND}:${ENV}:<container>\"" } else { "only proxied ports bind to ${RYNAS_BIND}" }
                )),
                _ => {}
            }
        }
    }

    // the app must trust the proxy through the declared variable
    if let Some(e) = app.web.as_ref().and_then(|w| w.trusted_proxies_env.as_deref()) {
        let main = svcs.iter().find(|(n, _)| *n == app.main_service).map(|(_, s)| *s);
        let env = main.and_then(|s| s.get("environment"));
        let value = match env {
            Some(Value::Mapping(m)) => m.get(e).and_then(Value::as_str).map(str::to_owned),
            Some(Value::Sequence(v)) => v.iter().filter_map(Value::as_str).find_map(|x| x.strip_prefix(&format!("{e}=")).map(str::to_owned)),
            _ => None,
        };
        if value.as_deref() != Some("${RYNAS_TRUSTED_PROXIES}") {
            err(format!("web.trusted_proxies_env: main service must set {e}: ${{RYNAS_TRUSTED_PROXIES}}"));
        }
    }
}

/// A published port of the compose file.
struct Published {
    env: String,
    container: u16,
    proto: Protocol,
    /// Bound to `${RYNAS_BIND}`.
    bound: bool,
}

/// `"[${RYNAS_BIND}:]${ENV}:<container>[/tcp|/udp]"`, or the long syntax with
/// `published: ${ENV}` and optional `host_ip: ${RYNAS_BIND}`.
fn parse_port(p: &Value) -> Result<Published, String> {
    const BIND: &str = "${RYNAS_BIND}";
    let (host_ip, published, target, proto) = match p {
        Value::String(s) => {
            let (body, proto) = match s.rsplit_once('/') {
                Some((b, pr)) => (b, pr),
                None => (s.as_str(), "tcp"),
            };
            let (h, c) = body.rsplit_once(':').ok_or_else(|| format!("port `{s}` must be \"${{ENV}}:<container>\""))?;
            let (ip, h) = match h.strip_prefix(BIND) {
                Some(rest) => (BIND, rest.strip_prefix(':').unwrap_or("")),
                None => ("", h),
            };
            (ip.to_owned(), h.to_owned(), c.to_owned(), proto.to_owned())
        }
        Value::Mapping(m) => {
            let host_ip = m.get("host_ip").and_then(Value::as_str).unwrap_or("").to_owned();
            let published = m.get("published").and_then(Value::as_str).unwrap_or("").to_owned();
            let target = match m.get("target") {
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            let proto = m.get("protocol").and_then(Value::as_str).unwrap_or("tcp").to_owned();
            (host_ip, published, target, proto)
        }
        _ => return Err("unreadable port entry".into()),
    };
    if !host_ip.is_empty() && host_ip != BIND {
        return Err(format!("host address `{host_ip}`: only ${{RYNAS_BIND}} is allowed"));
    }
    let env = published
        .strip_prefix("${")
        .and_then(|x| x.strip_suffix('}'))
        .filter(|x| crate::manifest::is_env_name(x))
        .ok_or_else(|| format!("published port `{published}` must be exactly ${{ENV}} (no host address, no fixed number)"))?;
    let container: u16 = target.parse().map_err(|_| format!("container port `{target}` is not a number"))?;
    let proto = match proto.as_str() {
        "tcp" => Protocol::Tcp,
        "udp" => Protocol::Udp,
        o => return Err(format!("protocol `{o}`")),
    };
    Ok(Published { env: env.to_owned(), container, proto, bound: !host_ip.is_empty() })
}

/// Every variable a render of this entry defines.
pub(crate) fn known_vars(app: &AppManifest, form_props: impl Iterator<Item = String>) -> BTreeSet<String> {
    let mut k: BTreeSet<String> = PROVIDED_VARS.iter().map(|s| (*s).to_owned()).collect();
    k.extend(app.declared_envs().into_iter().map(str::to_owned));
    k.extend(form_props);
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_refs() {
        let r = refs_in("${A}:x $$B ${C:-d} $E").unwrap();
        assert_eq!(r, vec![
            VarRef { name: "A".into(), plain: true },
            VarRef { name: "C".into(), plain: false },
            VarRef { name: "E".into(), plain: false },
        ]);
        assert!(refs_in("${A").is_err());
        assert!(refs_in("a $ b").is_err());
    }

    #[test]
    fn image_refs() {
        let r = ImageRef::parse("ghcr.io/immich-app/immich-server:v3.3.6@sha256:e1981983b1c13f6024b57d72e7ecae3342eed1581201ab09fb248932d86c593a").unwrap();
        assert_eq!(r.name, "ghcr.io/immich-app/immich-server");
        assert_eq!(r.tag.as_deref(), Some("v3.3.6"));
        assert!(r.digest.is_some());
        let r = ImageRef::parse("registry.example.lan:5000/app").unwrap();
        assert_eq!((r.name.as_str(), r.tag), ("registry.example.lan:5000/app", None));
        assert_eq!(ImageRef::parse("docker.io/library/postgres:18").unwrap().normalized_name(), "postgres");
        assert!(ImageRef::parse("x:1@sha256:abc").is_err());
    }

    #[test]
    fn merge_patch_rfc7396() {
        let mut t: Value = yaml_serde::from_str("services:\n  a: {image: x, ports: [1]}\n  b: {image: y}\n").unwrap();
        let p: Value = yaml_serde::from_str("services:\n  a: {ports: [2], devices: [/dev/dri]}\n  b: null\n").unwrap();
        merge_patch(&mut t, &p);
        let want: Value = yaml_serde::from_str("services:\n  a: {image: x, ports: [2], devices: [/dev/dri]}\n").unwrap();
        assert_eq!(t, want);
    }
}
