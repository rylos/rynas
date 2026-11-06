//! Pre-install checks (port conflicts, RAM gate) and the update plan.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::catalog::CatalogApp;
use crate::compose::{self, ImageRef};
use crate::error::RenderError;
use crate::manifest::{AppManifest, Protocol};
use crate::render::{self, InstallRequest, Provided, RenderedStack};
use crate::version::Version;

/// A host port already in use (listening sockets plus ports of other stacks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UsedPort {
    pub port: u16,
    pub protocol: Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortConflict {
    /// Port name in app.toml.
    pub name: String,
    pub env: String,
    pub port: u16,
    pub protocol: Protocol,
    pub relocatable: bool,
    /// A free port to propose, for relocatable ports.
    pub suggestion: Option<u16>,
}

/// Checks the chosen host ports (`chosen` overrides, defaults otherwise) against `used` and
/// against each other.
pub fn port_conflicts(app: &AppManifest, chosen: &BTreeMap<String, u16>, used: &[UsedPort]) -> Vec<PortConflict> {
    let mut taken: BTreeSet<UsedPort> = used.iter().copied().collect();
    let effective: Vec<(&crate::manifest::Port, u16)> = app.ports.iter().map(|p| (p, chosen.get(&p.env).copied().unwrap_or(p.default))).collect();
    let mut out = Vec::new();
    let mut mine: BTreeSet<UsedPort> = BTreeSet::new();
    for (p, port) in &effective {
        let key = UsedPort { port: *port, protocol: p.protocol };
        if taken.contains(&key) || !mine.insert(key) {
            out.push(PortConflict { name: p.name.clone(), env: p.env.clone(), port: *port, protocol: p.protocol, relocatable: p.relocatable && !app.host_network, suggestion: None });
        }
    }
    // suggestions skip used ports, the app's own ports and earlier suggestions
    taken.extend(mine);
    for c in out.iter_mut().filter(|c| c.relocatable) {
        let start = c.port.max(1024);
        c.suggestion = (start..=u16::MAX).chain(1024..start).find(|n| !taken.contains(&UsedPort { port: *n, protocol: c.protocol }));
        if let Some(n) = c.suggestion {
            taken.insert(UsedPort { port: n, protocol: c.protocol });
        }
    }
    out
}

/// Result of the RAM gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RamGate {
    Ok,
    /// Install allowed with a warning.
    BelowRecommended { installed_mb: u64, recommended_mb: u32 },
    /// Install refused.
    BelowMinimum { installed_mb: u64, minimum_mb: u32 },
}

/// Compares the installed RAM with the entry's resources. `installed_mb` is MemTotal; part
/// of the RAM is reserved by firmware and the integrated GPU (12.5 % tolerance here), so a
/// 4 GB machine reporting 3.7 GiB still counts as 4 GB.
pub fn ram_gate(app: &AppManifest, installed_mb: u64) -> RamGate {
    let eff = installed_mb + installed_mb / 8;
    if eff < app.resources.ram_min_mb as u64 {
        RamGate::BelowMinimum { installed_mb, minimum_mb: app.resources.ram_min_mb }
    } else if eff < app.resources.ram_recommended_mb as u64 {
        RamGate::BelowRecommended { installed_mb, recommended_mb: app.resources.ram_recommended_mb }
    } else {
        RamGate::Ok
    }
}

/// Applies the migrations above `from` up to the entry version; returns the versions applied.
pub fn migrate(app: &CatalogApp, from: Version, values: &mut BTreeMap<String, String>) -> Vec<Version> {
    let mut applied = Vec::new();
    for m in app.migrations.iter().filter(|m| m.to > from && m.to <= app.manifest.version) {
        m.apply(values);
        applied.push(m.to);
    }
    applied
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageChangeKind {
    Added,
    Removed,
    /// The tag changed (and so the digest).
    Tag,
    /// Same tag, new digest (rebuilt image).
    Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageChange {
    pub service: String,
    pub kind: ImageChangeKind,
    pub from: Option<ImageRef>,
    pub to: Option<ImageRef>,
}

/// What an update of an installed app would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePlan {
    pub app: String,
    pub from_version: Version,
    pub to_version: Version,
    pub from_app_version: String,
    pub to_app_version: String,
    /// Same version and same images.
    pub up_to_date: bool,
    /// The catalogue is older than the installed entry (never applied automatically).
    pub downgrade: bool,
    pub migrations: Vec<Version>,
    pub image_changes: Vec<ImageChange>,
    /// Required form fields the stored values do not answer (the wizard must ask).
    pub missing_values: Vec<String>,
    pub changelog_url: String,
}

/// Compares an installed stack (its `compose.yaml` and `.env`) with the catalogue entry.
pub fn update_plan(app: &CatalogApp, installed_compose: &str, installed_env: &str) -> Result<UpdatePlan, RenderError> {
    let meta = render::installed_meta(installed_compose)?.ok_or_else(|| RenderError::NotInstalledApp("no x-rynas block (custom stack)".into()))?;
    if meta.app != app.manifest.id {
        return Err(RenderError::NotInstalledApp(format!("stack is `{}`, not `{}`", meta.app, app.manifest.id)));
    }
    let installed_doc: yaml_serde::Value = yaml_serde::from_str(installed_compose).map_err(|e| RenderError::Compose(e.to_string()))?;
    let mut values = render::parse_env(installed_env);
    let migrations = migrate(app, meta.version, &mut values);
    let (new_doc, _) = render::merged_compose(app, &values);

    let old: BTreeMap<String, ImageRef> = compose::images(&installed_doc).into_iter().filter_map(|(s, i)| Some((s, ImageRef::parse(&i).ok()?))).collect();
    let new: BTreeMap<String, ImageRef> = compose::images(&new_doc).into_iter().filter_map(|(s, i)| Some((s, ImageRef::parse(&i).ok()?))).collect();
    let mut image_changes = Vec::new();
    for svc in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        let (a, b) = (old.get(svc), new.get(svc));
        let kind = match (a, b) {
            (None, Some(_)) => ImageChangeKind::Added,
            (Some(_), None) => ImageChangeKind::Removed,
            (Some(a), Some(b)) if a.normalized_name() != b.normalized_name() || a.tag != b.tag => ImageChangeKind::Tag,
            (Some(a), Some(b)) if a.digest != b.digest => ImageChangeKind::Digest,
            _ => continue,
        };
        image_changes.push(ImageChange { service: svc.clone(), kind, from: a.cloned(), to: b.cloned() });
    }

    let req = render::request_from_env(app, "probe", &values);
    let missing_values = match app.form.resolve(&req.form) {
        Err(RenderError::MissingValue(v)) => vec![v],
        _ => Vec::new(),
    };
    let to = app.manifest.version;
    Ok(UpdatePlan {
        app: meta.app,
        from_version: meta.version,
        to_version: to,
        from_app_version: meta.app_version,
        to_app_version: app.manifest.app_version.clone(),
        up_to_date: meta.version == to && image_changes.is_empty(),
        downgrade: to < meta.version,
        migrations,
        image_changes,
        missing_values,
        changelog_url: app.manifest.changelog_url.clone(),
    })
}

/// Migrates the stored values of an installed stack and renders the new version with
/// them (secrets, ports, folders and answers kept). `answers` fills `missing_values`.
pub fn prepare_update(
    app: &CatalogApp,
    catalog: &str,
    provided: &Provided,
    stack: &str,
    installed_compose: &str,
    installed_env: &str,
    answers: &BTreeMap<String, serde_json::Value>,
) -> Result<RenderedStack, RenderError> {
    let meta = render::installed_meta(installed_compose)?.ok_or_else(|| RenderError::NotInstalledApp("no x-rynas block (custom stack)".into()))?;
    if meta.app != app.manifest.id {
        return Err(RenderError::NotInstalledApp(format!("stack is `{}`, not `{}`", meta.app, app.manifest.id)));
    }
    let mut values = render::parse_env(installed_env);
    migrate(app, meta.version, &mut values);
    let mut req: InstallRequest = render::request_from_env(app, stack, &values);
    req.form.extend(answers.iter().map(|(k, v)| (k.clone(), v.clone())));
    render::render(app, catalog, provided, &req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_skip_taken_ports() {
        let app: AppManifest = toml::from_str(
            r#"
            schema = 1
            id = "t"
            name = "T"
            version = "1.0.0"
            app_version = "1"
            description = "t"
            homepage = "https://example.org"
            changelog_url = "https://example.org"
            license = "MIT"
            maintainers = ["x"]
            categories = ["media"]
            min_rynas = "0.1.0"
            arch = ["amd64"]
            main_service = "t"
            resources = { ram_min_mb = 256, ram_recommended_mb = 512 }
            [[ports]]
            name = "web"
            env = "WEB_PORT"
            service = "t"
            container = 80
            default = 8080
            protocol = "tcp"
            purpose = "Web"
            [[ports]]
            name = "disc"
            env = "DISC_PORT"
            service = "t"
            container = 7000
            default = 7000
            protocol = "udp"
            purpose = "Discovery"
            relocatable = false
            "#,
        )
        .unwrap();
        let used = [
            UsedPort { port: 8080, protocol: Protocol::Tcp },
            UsedPort { port: 8081, protocol: Protocol::Tcp },
            UsedPort { port: 7000, protocol: Protocol::Udp },
        ];
        let c = port_conflicts(&app, &BTreeMap::new(), &used);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].suggestion, Some(8082));
        assert!(!c[1].relocatable);
        assert_eq!(c[1].suggestion, None);
        assert!(port_conflicts(&app, &BTreeMap::from([("WEB_PORT".into(), 9000)]), &used[1..2]).is_empty());

        assert_eq!(ram_gate(&app, 3900), RamGate::Ok);
        assert!(matches!(ram_gate(&app, 400), RamGate::BelowRecommended { .. }));
        assert!(matches!(ram_gate(&app, 128), RamGate::BelowMinimum { .. }));
    }
}
