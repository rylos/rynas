//! Loading and validating a catalogue directory:
//!
//! ```text
//! apps/<id>/
//!   app.toml              metadata (manifest.rs)
//!   compose.yaml          ordinary compose, ${VAR} interpolation only
//!   form.json             install questions (form.rs), optional
//!   description.md        long English description
//!   metadata/icon.svg     card icon
//!   metadata/screenshots/ optional *.webp / *.png
//!   overlays/*.yaml       optional compose merge patches, listed in app.toml
//!   migrations/*.toml     optional value migrations, one per package version
//!   tests/values.toml     answers used by CI to render the entry
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use yaml_serde::Value;

use crate::compose;
use crate::error::Issues;
use crate::form::FormSchema;
use crate::manifest::{AppManifest, PROVIDED_VARS, is_app_id};
use crate::version::Version;

/// One step of a migration, applied to the stored `.env` values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum MigrationOp {
    /// Renames a value (no-op when `from` is absent).
    Rename { from: String, to: String },
    /// Sets a value only when it is absent.
    SetDefault { key: String, value: String },
    /// Overwrites a value.
    Set { key: String, value: String },
    /// Removes a value.
    Delete { key: String },
    /// Replaces a value through a table; values not in the table are kept.
    Map { key: String, values: BTreeMap<String, String> },
}

/// `migrations/<version>.toml`: runs when updating from a version below `to` to `to` or later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Migration {
    pub to: Version,
    pub description: String,
    pub ops: Vec<MigrationOp>,
}

impl Migration {
    pub fn apply(&self, values: &mut BTreeMap<String, String>) {
        for op in &self.ops {
            match op {
                MigrationOp::Rename { from, to } => {
                    if let Some(v) = values.remove(from) {
                        values.insert(to.clone(), v);
                    }
                }
                MigrationOp::SetDefault { key, value } => {
                    values.entry(key.clone()).or_insert_with(|| value.clone());
                }
                MigrationOp::Set { key, value } => {
                    values.insert(key.clone(), value.clone());
                }
                MigrationOp::Delete { key } => {
                    values.remove(key);
                }
                MigrationOp::Map { key, values: table } => {
                    if let Some(v) = values.get_mut(key)
                        && let Some(n) = table.get(v.as_str())
                    {
                        *v = n.clone();
                    }
                }
            }
        }
    }
}

/// `tests/values.toml`: what a user would answer, used by CI.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestValues {
    #[serde(default)]
    pub form: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub shares: BTreeMap<String, String>,
    #[serde(default)]
    pub folders: BTreeMap<String, String>,
    #[serde(default)]
    pub ports: BTreeMap<String, u16>,
}

impl TestValues {
    /// The form answers as JSON values.
    pub fn form_json(&self) -> BTreeMap<String, serde_json::Value> {
        self.form.iter().map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or(serde_json::Value::Null))).collect()
    }
}

/// A compose overlay with its parsed patch.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedOverlay {
    pub spec: crate::manifest::Overlay,
    pub patch: Value,
}

/// A validated catalogue entry.
#[derive(Debug, Clone)]
pub struct CatalogApp {
    pub dir: PathBuf,
    pub manifest: AppManifest,
    pub compose: Value,
    pub form: FormSchema,
    pub overlays: Vec<LoadedOverlay>,
    /// Sorted by `to`.
    pub migrations: Vec<Migration>,
    pub tests: TestValues,
    pub description: String,
    /// Path of `metadata/icon.svg`.
    pub icon: PathBuf,
    pub screenshots: Vec<PathBuf>,
}

/// A loaded catalogue.
#[derive(Debug, Clone)]
pub struct Catalog {
    /// Source id written into `x-rynas.catalog` (`rynas` for the official catalogue).
    pub source: String,
    pub apps: BTreeMap<String, CatalogApp>,
}

impl Catalog {
    /// Loads every `apps/<id>/` entry; collects the problems of all entries.
    pub fn load(dir: &Path, source: &str) -> Result<Self, Issues> {
        let mut issues = Issues::default();
        let mut apps = BTreeMap::new();
        let mut entries: Vec<PathBuf> = match fs::read_dir(dir) {
            Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect(),
            Err(e) => {
                issues.push("-", &dir.display().to_string(), format!("cannot read the catalogue: {e}"));
                return Err(issues);
            }
        };
        entries.sort();
        for p in entries {
            match CatalogApp::load(&p) {
                Ok(app) => {
                    apps.insert(app.manifest.id.clone(), app);
                }
                Err(e) => issues.0.extend(e.0),
            }
        }
        // default host ports unique across the catalogue (TrueNAS rule): fewer relocations
        let mut owners: BTreeMap<(u16, &str), &str> = BTreeMap::new();
        for app in apps.values() {
            if app.manifest.host_network {
                continue;
            }
            for p in &app.manifest.ports {
                // reverse proxies all want 80/443 and exclude each other
                if app.manifest.reverse_proxy && matches!(p.default, 80 | 443) {
                    continue;
                }
                if let Some(other) = owners.insert((p.default, p.protocol.as_str()), &app.manifest.id) {
                    issues.push(&app.manifest.id, "app.toml", format!("default port {}/{} is also the default of `{other}`", p.default, p.protocol.as_str()));
                }
            }
        }
        issues.into_result(Self { source: source.to_owned(), apps })
    }

    pub fn get(&self, id: &str) -> Option<&CatalogApp> {
        self.apps.get(id)
    }
}

const ALLOWED_TOP: &[&str] = &["app.toml", "compose.yaml", "form.json", "description.md", "metadata", "overlays", "migrations", "tests"];

fn read(dir: &Path, rel: &str, app: &str, issues: &mut Issues) -> Option<String> {
    match fs::read_to_string(dir.join(rel)) {
        Ok(s) => Some(s),
        Err(e) => {
            issues.push(app, rel, format!("cannot read: {e}"));
            None
        }
    }
}

fn list(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

impl CatalogApp {
    /// Loads and fully validates one entry (manifest, form, compose and every overlay
    /// variant, migrations, test values, metadata).
    pub fn load(dir: &Path) -> Result<Self, Issues> {
        let folder = file_name(dir);
        let mut issues = Issues::default();
        let id = folder.as_str();
        if !is_app_id(id) {
            issues.push(id, ".", "folder name must match [a-z0-9][a-z0-9-]*");
        }
        for p in list(dir) {
            let n = file_name(&p);
            if !ALLOWED_TOP.contains(&n.as_str()) {
                issues.push(id, &n, "unexpected file in the entry");
            }
        }

        let manifest: Option<AppManifest> = read(dir, "app.toml", id, &mut issues).and_then(|t| match toml::from_str(&t) {
            Ok(m) => Some(m),
            Err(e) => {
                issues.push(id, "app.toml", e.to_string());
                None
            }
        });
        let compose_doc = read(dir, "compose.yaml", id, &mut issues).and_then(|t| match compose::parse(&t) {
            Ok(v) => Some(v),
            Err(e) => {
                issues.push(id, "compose.yaml", e);
                None
            }
        });
        let form = if dir.join("form.json").exists() {
            read(dir, "form.json", id, &mut issues).and_then(|t| match FormSchema::parse(&t) {
                Ok(f) => Some(f),
                Err(e) => {
                    issues.push(id, "form.json", e);
                    None
                }
            })
        } else {
            Some(FormSchema::empty())
        };
        let description = read(dir, "description.md", id, &mut issues).unwrap_or_default();
        if description.trim().is_empty() {
            issues.push(id, "description.md", "empty");
        }

        // metadata
        let icon = dir.join("metadata/icon.svg");
        match fs::read(&icon) {
            Ok(b) => {
                let s = String::from_utf8_lossy(&b);
                if b.len() > 64 * 1024 || !s.contains("<svg") || s.to_ascii_lowercase().contains("<script") || s.contains("href=\"http") {
                    issues.push(id, "metadata/icon.svg", "must be an SVG under 64 KiB, without scripts or external references");
                }
            }
            Err(e) => issues.push(id, "metadata/icon.svg", format!("cannot read: {e}")),
        }
        let mut screenshots = Vec::new();
        for p in list(&dir.join("metadata")) {
            let n = file_name(&p);
            if n == "icon.svg" {
                continue;
            }
            if n == "screenshots" {
                for s in list(&p) {
                    let sn = file_name(&s);
                    if sn.ends_with(".webp") || sn.ends_with(".png") {
                        screenshots.push(s);
                    } else {
                        issues.push(id, &format!("metadata/screenshots/{sn}"), "screenshots must be .webp or .png");
                    }
                }
            } else {
                issues.push(id, &format!("metadata/{n}"), "unexpected file");
            }
        }

        // migrations
        let mut migrations = Vec::new();
        for p in list(&dir.join("migrations")) {
            let n = file_name(&p);
            let rel = format!("migrations/{n}");
            let Some(stem) = n.strip_suffix(".toml") else {
                issues.push(id, &rel, "migrations must be <version>.toml");
                continue;
            };
            let Some(text) = read(dir, &rel, id, &mut issues) else { continue };
            match toml::from_str::<Migration>(&text) {
                Ok(m) => {
                    if m.to.to_string() != stem {
                        issues.push(id, &rel, format!("`to = \"{}\"` differs from the file name", m.to));
                    }
                    if let Some(man) = &manifest
                        && m.to > man.version
                    {
                        issues.push(id, &rel, format!("targets {} above the entry version {}", m.to, man.version));
                    }
                    if m.ops.is_empty() {
                        issues.push(id, &rel, "no ops");
                    }
                    migrations.push(m);
                }
                Err(e) => issues.push(id, &rel, e.to_string()),
            }
        }
        migrations.sort_by_key(|m| m.to);

        // tests
        let tests = match read(dir, "tests/values.toml", id, &mut issues).map(|t| toml::from_str::<TestValues>(&t)) {
            Some(Ok(t)) => t,
            Some(Err(e)) => {
                issues.push(id, "tests/values.toml", e.to_string());
                TestValues::default()
            }
            None => TestValues::default(),
        };

        let (Some(manifest), Some(compose_doc), Some(form)) = (manifest, compose_doc, form) else {
            return Err(issues);
        };
        manifest.check(id, &mut issues);

        let mut taken: Vec<&str> = PROVIDED_VARS.to_vec();
        taken.extend(manifest.declared_envs());
        form.check(id, &taken, &mut issues);

        // overlays: declared ⇔ present, each parses, condition known and boolean
        let mut overlays = Vec::new();
        let declared: BTreeSet<&str> = manifest.overlays.iter().map(|o| o.file.as_str()).collect();
        for p in list(&dir.join("overlays")) {
            let rel = format!("overlays/{}", file_name(&p));
            if !declared.contains(rel.as_str()) {
                issues.push(id, &rel, "overlay not listed in app.toml");
            }
        }
        let booleans: BTreeSet<&str> = form.booleans().collect();
        for o in &manifest.overlays {
            let (var, _) = o.condition();
            if !booleans.contains(var) && var != "RYNAS_CPU_AVX2" {
                issues.push(id, "app.toml", format!("overlay `{}`: when `{}` is not a boolean form field or RYNAS_CPU_AVX2", o.file, o.when));
            }
            let Some(text) = read(dir, &o.file, id, &mut issues) else { continue };
            match compose::parse(&text) {
                Ok(patch) => overlays.push(LoadedOverlay { spec: o.clone(), patch }),
                Err(e) => issues.push(id, &o.file, e),
            }
        }

        // compose lint: base and every single overlay on top of it
        let known = compose::known_vars(&manifest, form.properties.keys().cloned());
        compose::lint(&manifest, &compose_doc, "compose.yaml", &known, &mut issues);
        for o in &overlays {
            let mut merged = compose_doc.clone();
            compose::merge_patch(&mut merged, &o.patch);
            compose::lint(&manifest, &merged, &format!("compose.yaml + {}", o.spec.file), &known, &mut issues);
        }
        // every declared name is used somewhere
        let mut used: BTreeSet<String> = BTreeSet::new();
        for v in std::iter::once(&compose_doc).chain(overlays.iter().map(|o| &o.patch)) {
            if let Ok(rs) = compose::refs(v) {
                used.extend(rs.into_iter().map(|r| r.name));
            }
        }
        for e in manifest.declared_envs() {
            // with host networking the port values are informative only
            let host_port = manifest.host_network && manifest.ports.iter().any(|p| p.env == e);
            if !used.contains(e) && !host_port {
                issues.push(id, "app.toml", format!("`{e}` is declared but compose never uses ${{{e}}}"));
            }
        }
        for v in &manifest.named_volumes {
            let present = compose_doc.get("volumes").and_then(Value::as_mapping).is_some_and(|m| m.contains_key(v.name.as_str()));
            if !present {
                issues.push(id, "app.toml", format!("named volume `{}` is not in compose volumes", v.name));
            }
        }

        let app = Self {
            dir: dir.to_owned(),
            manifest,
            compose: compose_doc,
            form,
            overlays,
            migrations,
            tests,
            description,
            icon,
            screenshots,
        };
        if issues.is_empty() {
            // the test values must render
            if let Err(e) = crate::render::render(&app, "lint", &crate::render::Provided::example(&app.manifest.id), &app.test_request()) {
                issues.push(id, "tests/values.toml", format!("render failed: {e}"));
            }
        }
        issues.into_result(app)
    }

    /// The install request described by `tests/values.toml`.
    pub fn test_request(&self) -> crate::render::InstallRequest {
        crate::render::InstallRequest {
            stack: self.manifest.id.clone(),
            form: self.tests.form_json(),
            shares: self.tests.shares.clone(),
            folders: self.tests.folders.clone(),
            ports: self.tests.ports.clone(),
            secrets: BTreeMap::new(),
        }
    }
}
