//! The engine configuration on a rynas volume.
//!
//! ```text
//! /<vol>/docker/              subvolume
//!   engine/                   subvolume: data-root (a separate subvolume: snapshots of
//!                             /<vol>/docker do not include it, images can be pulled again)
//!   containerd/               subvolume: containerd root (images of the containerd store)
//!   stacks/<stack>/           one subvolume per stack (compose.yaml, .env, ./data...)
//!   .snapshots/<stack>/<id>   read-only snapshots of a stack (and <id>.json)
//! ```
//!
//! - `/etc/docker/daemon.json` is **merged**: `data-root` is rynas's; `log-driver: local`,
//!   `live-restore: true` and `default-address-pools` are added only when the file has no
//!   value of its own; every other key stays as it is. A file that is not a JSON object is
//!   never replaced (the apply fails and says so).
//! - `/etc/containerd/config.toml`: the top-level `root = "…"` line is set (replaced or
//!   inserted before the first table), the rest of the file is kept with its comments.
//! - Drop-ins `RequiresMountsFor=` for `docker.service` and `containerd.service`: the engine
//!   never starts on an empty folder of the system disk before the volume is mounted.

use std::path::Path;

use rynas_api_types::docker::{AddressPool, DockerSettings};
use serde_json::{Map, Value};

pub const DAEMON_JSON: &str = "/etc/docker/daemon.json";
pub const CONTAINERD_TOML: &str = "/etc/containerd/config.toml";
pub const DOCKER_DROPIN: &str = "/etc/systemd/system/docker.service.d/rynas-mounts.conf";
pub const CONTAINERD_DROPIN: &str = "/etc/systemd/system/containerd.service.d/rynas-mounts.conf";
/// Defaults of an engine nobody configured.
pub const DEFAULT_DATA_ROOT: &str = "/var/lib/docker";
pub const DEFAULT_CONTAINERD_ROOT: &str = "/var/lib/containerd";

/// The folders of Docker on a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Mount point of the volume (`/vol1`).
    pub volume: String,
}

impl Layout {
    pub fn new(volume_path: impl Into<String>) -> Self {
        let mut volume: String = volume_path.into();
        while volume.len() > 1 && volume.ends_with('/') {
            volume.pop();
        }
        Self { volume }
    }

    /// The layout of a volume label (`vol1` → `/vol1`), the rynas convention.
    pub fn of_label(label: &str) -> Self {
        Self::new(format!("/{label}"))
    }

    pub fn docker(&self) -> String {
        format!("{}/docker", self.volume)
    }
    pub fn engine(&self) -> String {
        format!("{}/docker/engine", self.volume)
    }
    pub fn containerd(&self) -> String {
        format!("{}/docker/containerd", self.volume)
    }
    pub fn stacks(&self) -> String {
        format!("{}/docker/stacks", self.volume)
    }
    pub fn snapshots(&self) -> String {
        format!("{}/docker/.snapshots", self.volume)
    }
    pub fn stack(&self, name: &str) -> String {
        format!("{}/{name}", self.stacks())
    }
    pub fn stack_snapshots(&self, name: &str) -> String {
        format!("{}/{name}", self.snapshots())
    }

    /// Subvolumes, in creation order.
    pub fn subvolumes(&self) -> [String; 4] {
        [
            self.docker(),
            self.engine(),
            self.containerd(),
            self.stacks(),
        ]
    }
}

/// `daemon.json` with rynas's values merged in. `existing` is the current file (`None`: no
/// file). Errors when the file is not a JSON object: it is never overwritten then.
pub fn merge_daemon_json(
    existing: Option<&str>,
    layout: &Layout,
    pool: &AddressPool,
) -> Result<String, String> {
    let mut map: Map<String, Value> = match existing.map(str::trim) {
        None | Some("") => Map::new(),
        Some(t) => match serde_json::from_str::<Value>(t) {
            Ok(Value::Object(m)) => m,
            Ok(_) => {
                return Err(format!(
                    "{DAEMON_JSON} is not a JSON object: fix it by hand"
                ));
            }
            Err(e) => {
                return Err(format!(
                    "{DAEMON_JSON} is not valid JSON ({e}): fix it by hand"
                ));
            }
        },
    };
    map.insert("data-root".into(), Value::String(layout.engine()));
    map.entry("log-driver")
        .or_insert_with(|| Value::String("local".into()));
    map.entry("live-restore").or_insert(Value::Bool(true));
    let has_pools = map
        .get("default-address-pools")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    if !has_pools {
        map.insert(
            "default-address-pools".into(),
            serde_json::json!([{ "base": pool.base, "size": pool.size }]),
        );
    }
    let mut out = serde_json::to_string_pretty(&Value::Object(map)).map_err(|e| e.to_string())?;
    out.push('\n');
    Ok(out)
}

/// The data-root a `daemon.json` text sets (`None`: the default).
pub fn data_root_of(daemon_json: Option<&str>) -> Option<String> {
    let v: Value = serde_json::from_str(daemon_json?.trim()).ok()?;
    v.get("data-root")?.as_str().map(str::to_string)
}

/// The top-level `root` of a containerd `config.toml` text.
pub fn containerd_root_of(toml: Option<&str>) -> Option<String> {
    for l in toml?.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            return None;
        }
        if let Some(v) = top_key(t, "root") {
            return Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
        }
    }
    None
}

/// `Some(value text)` when the line is `key = value` (spaces allowed).
fn top_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?;
    let rest = rest.trim_start();
    rest.strip_prefix('=')
}

/// containerd's `config.toml` with the top-level `root` set to `root`; the rest kept.
pub fn merge_containerd_toml(existing: Option<&str>, root: &str) -> String {
    let line = format!("root = \"{root}\"");
    let Some(text) = existing.filter(|t| !t.trim().is_empty()) else {
        // what containerd.io ships, plus the root
        return format!(
            "# containerd of Docker (rynas sets the root on the volume; the rest is containerd.io's default)\ndisabled_plugins = [\"cri\"]\n{line}\n"
        );
    };
    let mut out: Vec<String> = Vec::new();
    let mut done = false;
    let mut in_top = true;
    for l in text.lines() {
        let t = l.trim();
        if in_top && t.starts_with('[') {
            in_top = false;
            if !done {
                // keep a blank line between the top keys and the first table
                while out.last().is_some_and(|x| x.trim().is_empty()) {
                    out.pop();
                }
                out.push(line.clone());
                out.push(String::new());
                done = true;
            }
        }
        if in_top && top_key(t, "root").is_some() {
            if !done {
                out.push(line.clone());
                done = true;
            }
            continue;
        }
        out.push(l.to_string());
    }
    if !done {
        while out.last().is_some_and(|x| x.trim().is_empty()) {
            out.pop();
        }
        out.push(line);
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// The `RequiresMountsFor=` drop-in of a unit (content without the rynas header).
pub fn dropin(paths: &[String]) -> String {
    format!(
        "# the engine starts only once its folders on the volume are mounted\n[Unit]\nRequiresMountsFor={}\n",
        paths.join(" ")
    )
}

/// The files of the engine configuration for `settings` (`(path, content)`); empty when no
/// volume is chosen. `read` gives the current content of a system file.
pub fn engine_files(
    settings: &DockerSettings,
    read: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<(&'static str, String)>, String> {
    let Some(label) = settings.volume.as_deref() else {
        return Ok(Vec::new());
    };
    let l = Layout::of_label(label);
    Ok(vec![
        (
            DAEMON_JSON,
            merge_daemon_json(read(DAEMON_JSON).as_deref(), &l, &settings.address_pool)?,
        ),
        (
            CONTAINERD_TOML,
            merge_containerd_toml(read(CONTAINERD_TOML).as_deref(), &l.containerd()),
        ),
        (DOCKER_DROPIN, dropin(&[l.engine(), l.containerd()])),
        (CONTAINERD_DROPIN, dropin(&[l.containerd()])),
    ])
}

/// The data-root and containerd root the engine uses with the files below `root`.
pub fn effective_roots(root: &Path) -> (String, String) {
    let read = |p: &str| std::fs::read_to_string(crate::under(root, p)).ok();
    (
        data_root_of(read(DAEMON_JSON).as_deref()).unwrap_or_else(|| DEFAULT_DATA_ROOT.into()),
        containerd_root_of(read(CONTAINERD_TOML).as_deref())
            .unwrap_or_else(|| DEFAULT_CONTAINERD_ROOT.into()),
    )
}

/// Checks an `AddressPool`: an IPv4 network in CIDR form and a longer prefix for the
/// networks cut from it.
pub fn check_pool(p: &AddressPool) -> Result<(), String> {
    let (ip, len) = p
        .base
        .split_once('/')
        .ok_or("the base must be in CIDR form, e.g. 10.200.0.0/16")?;
    let addr: std::net::Ipv4Addr = ip.parse().map_err(|_| "not an IPv4 address")?;
    let len: u8 = len.parse().map_err(|_| "invalid prefix length")?;
    if !(8..=28).contains(&len) {
        return Err("the base prefix must be /8 to /28".into());
    }
    let bits = u32::from(addr);
    let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
    if bits & !mask != 0 {
        return Err("the base has host bits set".into());
    }
    if p.size <= len || p.size > 30 {
        return Err(format!(
            "the size must be longer than /{len} and at most /30"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> AddressPool {
        AddressPool::default()
    }

    #[test]
    fn layout() {
        let l = Layout::of_label("vol1");
        assert_eq!(l.engine(), "/vol1/docker/engine");
        assert_eq!(l.stack("immich"), "/vol1/docker/stacks/immich");
        assert_eq!(
            l.stack_snapshots("immich"),
            "/vol1/docker/.snapshots/immich"
        );
        assert_eq!(
            Layout::new("/srv/data/").stacks(),
            "/srv/data/docker/stacks"
        );
    }

    #[test]
    fn daemon_json_from_nothing() {
        let out = merge_daemon_json(None, &Layout::of_label("vol1"), &pool()).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["data-root"], "/vol1/docker/engine");
        assert_eq!(v["log-driver"], "local");
        assert_eq!(v["live-restore"], true);
        assert_eq!(v["default-address-pools"][0]["base"], "10.200.0.0/16");
        assert_eq!(v["default-address-pools"][0]["size"], 24);
        assert!(out.ends_with("}\n"));
    }

    #[test]
    fn daemon_json_keeps_admin_choices() {
        let old = r#"{"log-driver":"json-file","log-opts":{"max-size":"10m"},"live-restore":false,
            "default-address-pools":[{"base":"172.80.0.0/16","size":24}],"registry-mirrors":["https://m.example"],
            "data-root":"/var/lib/docker"}"#;
        let out = merge_daemon_json(Some(old), &Layout::of_label("vol2"), &pool()).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["data-root"], "/vol2/docker/engine");
        assert_eq!(v["log-driver"], "json-file");
        assert_eq!(v["log-opts"]["max-size"], "10m");
        assert_eq!(v["live-restore"], false);
        assert_eq!(v["default-address-pools"][0]["base"], "172.80.0.0/16");
        assert_eq!(v["registry-mirrors"][0], "https://m.example");
        // an empty pool list counts as none
        let out = merge_daemon_json(
            Some(r#"{"default-address-pools":[]}"#),
            &Layout::of_label("v"),
            &pool(),
        )
        .unwrap();
        assert!(out.contains("10.200.0.0/16"));
    }

    #[test]
    fn daemon_json_never_replaces_garbage() {
        assert!(merge_daemon_json(Some("{oops"), &Layout::of_label("v"), &pool()).is_err());
        assert!(merge_daemon_json(Some("[1]"), &Layout::of_label("v"), &pool()).is_err());
        assert!(merge_daemon_json(Some("  \n"), &Layout::of_label("v"), &pool()).is_ok());
        assert_eq!(
            data_root_of(Some(r#"{"data-root":"/x"}"#)).as_deref(),
            Some("/x")
        );
        assert_eq!(data_root_of(Some("{}")), None);
    }

    #[test]
    fn containerd_root_merge() {
        let shipped = "#   Copyright 2018-2022 Docker Inc.\n\ndisabled_plugins = [\"cri\"]\n\n#root = \"/var/lib/containerd\"\n#state = \"/run/containerd\"\n";
        let out = merge_containerd_toml(Some(shipped), "/vol1/docker/containerd");
        assert!(out.contains("#   Copyright") && out.contains("disabled_plugins = [\"cri\"]"));
        assert!(
            out.ends_with("root = \"/vol1/docker/containerd\"\n"),
            "{out}"
        );
        assert_eq!(
            containerd_root_of(Some(&out)).as_deref(),
            Some("/vol1/docker/containerd")
        );

        let v3 = "version = 3\nroot = \"/var/lib/containerd\"\nstate = \"/run/containerd\"\n\n[grpc]\n  address = \"/run/containerd/containerd.sock\"\n  root = \"no\"\n";
        let out = merge_containerd_toml(Some(v3), "/v/docker/containerd");
        assert_eq!(
            out,
            "version = 3\nroot = \"/v/docker/containerd\"\nstate = \"/run/containerd\"\n\n[grpc]\n  address = \"/run/containerd/containerd.sock\"\n  root = \"no\"\n"
        );
        let tables = "[plugins]\n  x = 1\n";
        let out = merge_containerd_toml(Some(tables), "/v/c");
        assert_eq!(out, "root = \"/v/c\"\n\n[plugins]\n  x = 1\n");
        // idempotent
        assert_eq!(merge_containerd_toml(Some(&out), "/v/c"), out);
        let fresh = merge_containerd_toml(None, "/v/c");
        assert!(fresh.contains("disabled_plugins = [\"cri\"]\nroot = \"/v/c\"\n"));
    }

    #[test]
    fn files_for_settings() {
        let mut s = DockerSettings::default();
        assert!(engine_files(&s, &|_| None).unwrap().is_empty());
        s.volume = Some("vol1".into());
        let f = engine_files(&s, &|_| None).unwrap();
        assert_eq!(f.len(), 4);
        assert_eq!(f[2].0, DOCKER_DROPIN);
        assert!(
            f[2].1
                .contains("RequiresMountsFor=/vol1/docker/engine /vol1/docker/containerd\n")
        );
        assert!(
            f[3].1
                .contains("RequiresMountsFor=/vol1/docker/containerd\n")
        );
        assert!(engine_files(&s, &|p| (p == DAEMON_JSON).then(|| "nope".to_string())).is_err());
    }

    #[test]
    fn pools() {
        assert!(check_pool(&pool()).is_ok());
        for (b, s) in [
            ("10.200.0.1/16", 24),
            ("10.200.0.0", 24),
            ("10.200.0.0/16", 16),
            ("10.0.0.0/16", 31),
            ("x/16", 24),
        ] {
            assert!(
                check_pool(&AddressPool {
                    base: b.into(),
                    size: s
                })
                .is_err(),
                "{b} {s}"
            );
        }
    }
}
