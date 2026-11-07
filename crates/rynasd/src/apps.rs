//! The app catalogue (`/v1/apps/...`, public under `/api/v1/apps/...`): phase 2 wave A2,
//! `docs/phase2/docker-apps.md` §5.2. It joins [`rynas_apps`] (the catalogue format, the
//! renderer, migrations, the update plan) with the Docker side of [`crate::docker`] (stack
//! folders, compose tasks, snapshots and rollback).
//!
//! - The catalogue is read from `--apps-dir` (`/usr/share/rynas/apps`, the
//!   `rynas-apps-catalog` package) and kept in memory; a change of any file's mtime or size
//!   reloads it. A broken entry is listed in `issues` and never offered.
//! - The disk is the source of truth: an installed app is a stack whose `compose.yaml` has an
//!   `x-rynas` block. Nothing about installed apps is stored elsewhere.
//! - Every route needs the administrator role (rynasd checks it too): stack files hold
//!   secrets and apps get root-equivalent access through Docker.
//! - Every change is a task `apps.*` and a line of the audit log (`rynas::audit`).
//! - Secrets are generated at install and live only in the stack's `.env` (0600). No answer
//!   carries them, except the install answer for secrets the entry marks `show`.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::apps::*;
use rynas_api_types::docker::ContainerState;
use rynas_api_types::error_codes;
use rynas_apps::plan::UsedPort;
use rynas_apps::render::{FolderPlan, XRynas};
use rynas_apps::{Catalog, CatalogApp, RenderError, RenderedStack, Version};
use rynas_docker::stacks::{COMPOSE_FILE, ComposeAction, compose_cmd};
use rynas_docker::{Layout, install, model, snapshots};
use rynas_tasks::TaskCtx;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::docker::{
    DockerSvc, OwnedBusy, accepted, conflict, do_rollback, mark_busy, not_installed, optional_body,
    pre_snapshot,
};
use crate::ops::{Error, Res, user};

/// Where the `rynas-apps-catalog` package installs the catalogue.
pub const DEFAULT_CATALOG_DIR: &str = "/usr/share/rynas/apps";
/// Source id of the official catalogue (`x-rynas.catalog`).
pub const CATALOG_SOURCE: &str = "rynas";
/// How often the catalogue files are checked for changes.
const RECHECK: Duration = Duration::from_secs(2);
const MAX_ICON: u64 = 64 << 10;
const MAX_SCREENSHOT: u64 = 1 << 20;
/// What the `.env` preview shows instead of a secret.
pub const MASK: &str = "********";
/// Owner shown and used when the `docker-apps` account does not exist yet (plans, dry
/// runs): `nobody`, never root.
const PLACEHOLDER_ID: u32 = 65534;

struct Loaded {
    stamp: u64,
    checked: Instant,
    catalog: Arc<Catalog>,
    issues: Arc<Vec<CatalogIssue>>,
}

/// What the app endpoints work on.
pub struct AppsSvc {
    pub docker: Arc<DockerSvc>,
    /// The catalogue folder (`apps/<id>/...`).
    pub dir: PathBuf,
    cache: Mutex<Option<Loaded>>,
}

pub type AppsState = Option<Arc<AppsSvc>>;

fn asvc(st: &AppsState) -> Res<Arc<AppsSvc>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "the app catalogue is not enabled in this rynasd".into(),
        )
    })
}

/// The user, who must be an administrator (reads included).
fn admin(s: &AppsSvc, hd: &HeaderMap) -> Res<String> {
    let who = user(hd)?;
    s.docker
        .ops
        .admins
        .require_admin(&who, "the app catalogue")?;
    Ok(who)
}

fn audit(who: &str, action: &str, target: &str) {
    tracing::info!(target: "rynas::audit", "apps {action} {target} by {who}");
}

/// Same-shape conversion between the catalogue crate's types and the API types (both are
/// serde mirrors of `docs/phase2/docker-apps.md` §6).
fn conv<U: DeserializeOwned + Default>(v: &impl Serialize) -> U {
    serde_json::to_value(v)
        .ok()
        .and_then(|j| serde_json::from_value(j).ok())
        .unwrap_or_default()
}

/// Hash of every file's name, size and mtime below `dir` (three levels: entries, their
/// files, metadata and screenshots).
fn stamp_of(dir: &Path) -> u64 {
    fn walk(p: &Path, depth: u8, h: &mut std::collections::hash_map::DefaultHasher) {
        let Ok(rd) = std::fs::read_dir(p) else {
            return;
        };
        let mut v: Vec<_> = rd.flatten().collect();
        v.sort_by_key(|e| e.file_name());
        for e in v {
            let Ok(m) = e.metadata() else { continue };
            e.file_name().hash(h);
            m.len().hash(h);
            m.mtime().hash(h);
            m.mtime_nsec().hash(h);
            if m.is_dir() && depth > 0 {
                walk(&e.path(), depth - 1, h);
            }
        }
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    walk(dir, 3, &mut h);
    h.finish()
}

/// Loads every entry; a broken one goes to the issues instead of failing the catalogue.
fn load(dir: &Path) -> (Catalog, Vec<CatalogIssue>) {
    let mut apps = BTreeMap::new();
    let mut issues = Vec::new();
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && !p
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            })
            .collect(),
        Err(e) => {
            issues.push(CatalogIssue {
                app: "-".into(),
                file: dir.display().to_string(),
                message: format!("cannot read the catalogue: {e}"),
            });
            Vec::new()
        }
    };
    entries.sort();
    for p in entries {
        match CatalogApp::load(&p) {
            Ok(a) => {
                apps.insert(a.manifest.id.clone(), a);
            }
            Err(e) => issues.extend(e.0.into_iter().map(|i| CatalogIssue {
                app: i.app,
                file: i.file,
                message: i.message,
            })),
        }
    }
    for i in &issues {
        tracing::warn!("apps: catalogue entry {}/{}: {}", i.app, i.file, i.message);
    }
    (
        Catalog {
            source: CATALOG_SOURCE.into(),
            apps,
        },
        issues,
    )
}

/// `amd64`/`arm64` of this machine (the catalogue's names).
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn read_text(root: &Path, abs: &str) -> String {
    std::fs::read_to_string(rynas_docker::under(root, abs)).unwrap_or_default()
}

/// The gid of `name` in a group file.
fn group_gid(group: &str, name: &str) -> Option<u32> {
    group.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.first() == Some(&name))
            .then(|| f.get(2)?.parse().ok())
            .flatten()
    })
}

/// Host ports with a listening TCP socket or a bound UDP socket (`/proc/net/*`).
fn listening_ports(host: &Path) -> Vec<UsedPort> {
    let mut out = Vec::new();
    for (file, proto, state) in [
        ("tcp", rynas_apps::manifest::Protocol::Tcp, "0A"),
        ("tcp6", rynas_apps::manifest::Protocol::Tcp, "0A"),
        ("udp", rynas_apps::manifest::Protocol::Udp, "07"),
        ("udp6", rynas_apps::manifest::Protocol::Udp, "07"),
    ] {
        let text = read_text(host, &format!("/proc/net/{file}"));
        for l in text.lines().skip(1) {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 4 || f[3] != state {
                continue;
            }
            if let Some(port) = f[1]
                .rsplit_once(':')
                .and_then(|(_, p)| u16::from_str_radix(p, 16).ok())
            {
                out.push(UsedPort {
                    port,
                    protocol: proto,
                });
            }
        }
    }
    out
}

/// `/<label>/...` below a mounted (existing) rynas volume.
fn on_volume(root: &Path, p: &str) -> bool {
    let Some(rest) = p.strip_prefix('/') else {
        return false;
    };
    let label = rest.split('/').next().unwrap_or("");
    rest.len() > label.len() + 1
        && rynas_api_types::validate::volume_label(label)
        && !p.split('/').any(|s| s == ".." || s == ".")
        && !p.contains(['\n', '\0', '\'', ':'])
        && rynas_docker::under(root, &format!("/{label}")).is_dir()
}

fn field_error(e: &RenderError) -> FieldError {
    let (field, kind) = match e {
        RenderError::InvalidValue { field, .. } => (field.clone(), FieldErrorKind::InvalidValue),
        RenderError::MissingValue(f) => (f.clone(), FieldErrorKind::MissingValue),
        RenderError::UnknownValue(f) => (
            f.rsplit(' ').next().unwrap_or(f).to_string(),
            FieldErrorKind::UnknownValue,
        ),
        RenderError::UnsafeValue(f) => (f.clone(), FieldErrorKind::UnsafeValue),
        RenderError::InvalidStackName(_) => ("stack".into(), FieldErrorKind::BadStack),
        other => {
            tracing::warn!("apps: render: {other}");
            (String::new(), FieldErrorKind::InvalidValue)
        }
    };
    FieldError {
        field,
        kind,
        message: e.to_string(),
    }
}

/// A render failure as an API error (`invalid_value` with the field when there is one).
fn render_err(e: RenderError) -> Error {
    let f = field_error(&e);
    match e {
        RenderError::NotInstalledApp(m) => conflict(m),
        RenderError::Compose(_) | RenderError::Random(_) | RenderError::Migration(_) => {
            Error::internal(e)
        }
        _ => Error::invalid(f.field, f.message),
    }
}

/// The `.env` text with the secrets' values masked.
fn masked_env(text: &str, secrets: &BTreeSet<String>) -> String {
    let mut out = String::with_capacity(text.len());
    for l in text.lines() {
        match l.split_once('=') {
            Some((k, _)) if secrets.contains(k.trim()) => {
                out.push_str(k);
                out.push('=');
                out.push_str(MASK);
            }
            _ => out.push_str(l),
        }
        out.push('\n');
    }
    out
}

fn file_text(r: &RenderedStack, path: &str) -> String {
    r.file(path)
        .map(|f| String::from_utf8_lossy(&f.contents).into_owned())
        .unwrap_or_default()
}

/// An installed app stack read from disk.
struct OnDisk {
    stack: String,
    meta: XRynas,
    compose: String,
    env: String,
}

/// What an install needs after the plan accepted it.
struct Prepared {
    plan: AppInstallPlan,
    layout: Option<Layout>,
    request: rynas_apps::InstallRequest,
    rendered: Option<RenderedStack>,
    bind: String,
    docker_installed: bool,
}

impl AppsSvc {
    pub fn new(docker: Arc<DockerSvc>, dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            docker,
            dir,
            cache: Mutex::new(None),
        })
    }

    /// The catalogue, reloaded when a file changed.
    pub async fn catalog(&self) -> (Arc<Catalog>, Arc<Vec<CatalogIssue>>) {
        if let Some(l) = self.cache.lock().unwrap().as_ref()
            && l.checked.elapsed() < RECHECK
        {
            return (l.catalog.clone(), l.issues.clone());
        }
        let dir = self.dir.clone();
        let old = self.cache.lock().unwrap().as_ref().map(|l| l.stamp);
        let (stamp, loaded) = tokio::task::spawn_blocking(move || {
            let stamp = stamp_of(&dir);
            if old == Some(stamp) {
                return (stamp, None);
            }
            (stamp, Some(load(&dir)))
        })
        .await
        .unwrap_or((0, None));
        let mut g = self.cache.lock().unwrap();
        match (loaded, g.as_mut()) {
            (None, Some(l)) => {
                l.checked = Instant::now();
                (l.catalog.clone(), l.issues.clone())
            }
            (loaded, _) => {
                let (c, i) = loaded.unwrap_or_else(|| load(&self.dir));
                let l = Loaded {
                    stamp,
                    checked: Instant::now(),
                    catalog: Arc::new(c),
                    issues: Arc::new(i),
                };
                let r = (l.catalog.clone(), l.issues.clone());
                *g = Some(l);
                r
            }
        }
    }

    fn host(&self) -> &Path {
        &self.docker.host_root
    }

    /// `MemTotal` in MiB.
    fn mem_mb(&self) -> Option<u64> {
        let b = rynas_docker::stats::mem_total(&read_text(self.host(), "/proc/meminfo"));
        (b > 0).then_some(b >> 20)
    }

    fn ram(&self, app: &CatalogApp) -> RamGate {
        match self.mem_mb() {
            Some(mb) => conv(&rynas_apps::plan::ram_gate(&app.manifest, mb)),
            None => RamGate::Unknown,
        }
    }

    /// Every stack of the stacks folder whose compose file has an `x-rynas` block.
    fn on_disk(&self) -> Vec<OnDisk> {
        let Some(l) = self.docker.layout() else {
            return Vec::new();
        };
        let fs = self.docker.fs(None);
        self.docker
            .folders()
            .0
            .into_iter()
            .filter_map(|stack| {
                let f = fs.read_files(&l, &stack).ok()?;
                let meta = rynas_apps::render::installed_meta(&f.compose).ok()??;
                Some(OnDisk {
                    stack,
                    meta,
                    compose: f.compose,
                    env: f.env,
                })
            })
            .collect()
    }

    fn one_on_disk(&self, stack: &str) -> Res<(Layout, OnDisk)> {
        rynas_docker::stacks::check_name(stack)?;
        let l = self
            .docker
            .layout()
            .ok_or_else(|| Error::not_found(format!("no app stack {stack}")))?;
        let f = self
            .docker
            .fs(None)
            .read_files(&l, stack)
            .map_err(|_| Error::not_found(format!("no app stack {stack}")))?;
        match rynas_apps::render::installed_meta(&f.compose) {
            Ok(Some(meta)) => Ok((
                l,
                OnDisk {
                    stack: stack.into(),
                    meta,
                    compose: f.compose,
                    env: f.env,
                },
            )),
            _ => Err(Error::not_found(format!(
                "stack {stack} is not an app of the catalogue (no x-rynas block)"
            ))),
        }
    }

    fn update_available(cat: &Catalog, d: &OnDisk) -> bool {
        cat.get(&d.meta.app)
            .and_then(|a| rynas_apps::plan::update_plan(a, &d.compose, &d.env).ok())
            .is_some_and(|p| !p.up_to_date && !p.downgrade)
    }

    fn card(&self, app: &CatalogApp, installed: &[&OnDisk], cat: &Catalog) -> AppCard {
        let m = &app.manifest;
        let rynas = env!("CARGO_PKG_VERSION").parse::<Version>().ok();
        AppCard {
            id: m.id.clone(),
            name: m.name.clone(),
            summary: m.description.clone(),
            categories: conv(&m.categories),
            version: m.version.to_string(),
            app_version: m.app_version.clone(),
            license: m.license.clone(),
            antifeatures: conv(&m.antifeatures),
            ports: conv(&m.ports),
            ram_min_mb: m.resources.ram_min_mb,
            ram_recommended_mb: m.resources.ram_recommended_mb,
            ram: self.ram(app),
            arch_ok: m.arch.iter().any(|a| conv::<String>(a) == host_arch()),
            rynas_ok: rynas.is_none_or(|v| v >= m.min_rynas),
            manager: m.manager,
            icon: format!("/api/v1/apps/catalog/{}/icon", m.id),
            installed: installed.iter().map(|d| d.stack.clone()).collect(),
            update_available: installed.iter().any(|d| Self::update_available(cat, d)),
        }
    }

    pub async fn list(&self) -> AppCatalog {
        let (cat, issues) = self.catalog().await;
        let installed_ram_mb = self.mem_mb();
        let docker_installed = self.docker.facts(false).await.installed;
        let stacks_dir = self.docker.layout().map(|l| l.stacks());
        let disk = self.on_disk();
        let mut apps: Vec<AppCard> = cat
            .apps
            .values()
            .map(|a| {
                let mine: Vec<&OnDisk> = disk
                    .iter()
                    .filter(|d| d.meta.app == a.manifest.id)
                    .collect();
                self.card(a, &mine, &cat)
            })
            .collect();
        apps.sort_by_key(|a| a.name.to_lowercase());
        AppCatalog {
            source: cat.source.clone(),
            apps,
            issues: issues.as_ref().clone(),
            installed_ram_mb,
            arch: host_arch().into(),
            docker_installed,
            stacks_dir,
        }
    }

    pub async fn detail(&self, id: &str) -> Res<AppDetail> {
        let (cat, _) = self.catalog().await;
        let app = cat
            .get(id)
            .ok_or_else(|| Error::not_found(format!("no app {id} in the catalogue")))?;
        let disk = self.on_disk();
        let mine: Vec<&OnDisk> = disk.iter().filter(|d| d.meta.app == id).collect();
        let m = &app.manifest;
        Ok(AppDetail {
            card: self.card(app, &mine, &cat),
            description: app.description.clone(),
            homepage: m.homepage.clone(),
            source: m.source.clone(),
            changelog_url: m.changelog_url.clone(),
            maintainers: m.maintainers.clone(),
            host_network: m.host_network,
            form: conv(&app.form),
            web: m.web.as_ref().map(conv),
            folders: conv(&m.folders),
            shares: conv(&m.shares),
            secrets: conv(&m.secrets),
            hardware: conv(&m.hardware),
            overlays: conv(&m.overlays),
            named_volumes: conv(&m.named_volumes),
            notes: conv(&m.notes),
            screenshots: app
                .screenshots
                .iter()
                .filter_map(|p| p.file_name()?.to_str().map(str::to_string))
                .map(|f| format!("/api/v1/apps/catalog/{id}/screenshots/{f}"))
                .collect(),
        })
    }

    /// The values rynas provides to a render, read from the host.
    fn provided(
        &self,
        l: &Layout,
        stack: &str,
        bind: &str,
        ids: (u32, u32),
    ) -> rynas_apps::Provided {
        let host = self.host();
        let root = &self.docker.root;
        let group = read_text(root, "/etc/group");
        let hostname = read_text(host, "/etc/hostname").trim().to_string();
        let tz = {
            let t = read_text(host, "/etc/timezone").trim().to_string();
            if !t.is_empty() {
                t
            } else {
                std::fs::read_link(rynas_docker::under(host, "/etc/localtime"))
                    .ok()
                    .and_then(|p| {
                        let s = p.to_string_lossy().into_owned();
                        s.split_once("zoneinfo/").map(|(_, z)| z.to_string())
                    })
                    .unwrap_or_else(|| "Etc/UTC".into())
            }
        };
        let docker_gid = std::fs::metadata(rynas_docker::under(host, rynas_docker::DOCKER_SOCKET))
            .map(|m| m.gid())
            .ok()
            .or_else(|| group_gid(&group, "docker"))
            .unwrap_or(0);
        let render_gid = std::fs::read_dir(rynas_docker::under(host, "/dev/dri"))
            .into_iter()
            .flatten()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().starts_with("renderD"))
            .and_then(|e| e.metadata().ok())
            .map(|m| m.gid())
            .or_else(|| group_gid(&group, "render"))
            .unwrap_or(0);
        let cpuinfo = read_text(host, "/proc/cpuinfo");
        let cpu_avx2 = cpuinfo
            .lines()
            .find(|l| l.starts_with("flags"))
            .is_some_and(|l| l.split_whitespace().any(|w| w == "avx2"));
        rynas_apps::Provided {
            appdata: l.stack(stack),
            stacks_dir: l.stacks(),
            host: if hostname.is_empty() {
                "localhost".into()
            } else {
                hostname
            },
            puid: ids.0,
            pgid: ids.1,
            tz,
            docker_gid,
            render_gid,
            cpu_avx2,
            bind: bind.into(),
            trusted_proxies: self.docker.settings().address_pool.base,
        }
    }

    /// The `docker-apps` (uid, gid), whatever `--dry-system` says.
    fn apps_ids(&self) -> Option<(u32, u32)> {
        let root = &self.docker.root;
        match install::apps_ids(
            &read_text(root, "/etc/passwd"),
            &read_text(root, "/etc/group"),
        ) {
            (Some(u), Some(g)) => Some((u, g)),
            _ => None,
        }
    }

    /// Host ports in use: published ports of containers and listening sockets. `false`
    /// when the engine did not answer.
    async fn used_ports(&self) -> (Vec<UsedPort>, bool) {
        let mut used = listening_ports(self.host());
        let ok = match self.docker.containers().await {
            Ok(cs) => {
                for c in &cs {
                    for p in &c.c.ports {
                        let proto = match p.protocol.as_str() {
                            "tcp" => rynas_apps::manifest::Protocol::Tcp,
                            "udp" => rynas_apps::manifest::Protocol::Udp,
                            _ => continue,
                        };
                        if let Some(port) = p.public_port {
                            used.push(UsedPort {
                                port,
                                protocol: proto,
                            });
                        }
                    }
                }
                true
            }
            Err(_) => false,
        };
        used.sort();
        used.dedup();
        (used, ok)
    }

    /// Checks an install request and renders it: the plan, and what the install needs.
    async fn prepare(&self, id: &str, req: &AppInstallRequest) -> Res<Prepared> {
        let (cat, _) = self.catalog().await;
        let app = cat
            .get(id)
            .ok_or_else(|| Error::not_found(format!("no app {id} in the catalogue")))?;
        let m = &app.manifest;
        let root = self.docker.root.clone();
        let docker_installed = self.docker.facts(false).await.installed;
        let layout = self.docker.layout();
        let stack = req.stack.clone().unwrap_or_else(|| m.id.clone());
        let bind = if req.publish_lan {
            "0.0.0.0"
        } else {
            "127.0.0.1"
        }
        .to_string();
        let mut warnings = Vec::new();
        if !docker_installed {
            warnings.push(PlanWarning::DockerNotInstalled);
        }
        if layout.is_none() {
            warnings.push(PlanWarning::NoVolume);
        }
        let mut errors = Vec::new();
        let err = |field: &str, kind, message: String| FieldError {
            field: field.into(),
            kind,
            message,
        };
        if rynas_docker::stacks::check_name(&stack).is_err()
            || !rynas_apps::render::valid_stack_name(&stack)
        {
            errors.push(err(
                "stack",
                FieldErrorKind::BadStack,
                format!("`{stack}` is not a valid stack name (a-z, 0-9, - and _)"),
            ));
        } else if let Some(l) = &layout
            && rynas_docker::under(&root, &l.stack(&stack)).exists()
        {
            errors.push(err(
                "stack",
                FieldErrorKind::BadStack,
                format!("the stack folder {} exists", l.stack(&stack)),
            ));
        }
        if let Err(es) = app.form.resolve_all(&req.form) {
            errors.extend(es.iter().map(field_error));
        }
        for (k, p) in &req.shares {
            if !m.shares.iter().any(|s| &s.env == k) {
                errors.push(err(
                    k,
                    FieldErrorKind::UnknownValue,
                    format!("no share {k}"),
                ));
            } else if !on_volume(&root, p) {
                errors.push(err(
                    k,
                    FieldErrorKind::BadPath,
                    format!("{p} is not a folder of a mounted volume"),
                ));
            } else if !rynas_docker::under(&root, p).is_dir() {
                errors.push(err(
                    k,
                    FieldErrorKind::ShareMissing,
                    format!("{p} does not exist"),
                ));
            }
        }
        for s in &m.shares {
            if !req.shares.contains_key(&s.env) && s.fallback.is_none() {
                errors.push(err(
                    &s.env,
                    FieldErrorKind::MissingValue,
                    format!("choose a shared folder for {}", s.label),
                ));
            }
        }
        for (k, p) in &req.folders {
            if !m.folders.iter().any(|f| &f.env == k) {
                errors.push(err(
                    k,
                    FieldErrorKind::UnknownValue,
                    format!("no folder {k}"),
                ));
            } else if !on_volume(&root, p) {
                errors.push(err(
                    k,
                    FieldErrorKind::BadPath,
                    format!("{p} is not below a mounted volume"),
                ));
            }
        }
        for (k, n) in &req.ports {
            match m.ports.iter().find(|p| &p.env == k) {
                None => errors.push(err(k, FieldErrorKind::UnknownValue, format!("no port {k}"))),
                Some(p) if m.host_network || !p.relocatable => errors.push(err(
                    k,
                    FieldErrorKind::BadPort,
                    "this port cannot be moved".into(),
                )),
                Some(_) if *n == 0 => errors.push(err(k, FieldErrorKind::BadPort, "port 0".into())),
                Some(_) => {}
            }
        }
        let (used, engine_ok) = self.used_ports().await;
        if docker_installed && !engine_ok {
            warnings.push(PlanWarning::EngineUnreachable);
        }
        let port_conflicts: Vec<PortConflict> =
            conv(&rynas_apps::plan::port_conflicts(m, &req.ports, &used));
        let ram = self.ram(app);
        if matches!(ram, RamGate::BelowRecommended { .. }) {
            warnings.push(PlanWarning::RamBelowRecommended);
        }
        if req.publish_lan && m.ports.iter().any(|p| p.proxied) {
            warnings.push(PlanWarning::PublishedOnLan);
        }
        let ports = m
            .ports
            .iter()
            .map(|p| PlannedPort {
                name: p.name.clone(),
                env: p.env.clone(),
                port: req.ports.get(&p.env).copied().unwrap_or(p.default),
                protocol: conv(&p.protocol),
                proxied: p.proxied,
                bind: (!m.host_network).then(|| {
                    if p.proxied {
                        bind.clone()
                    } else {
                        "0.0.0.0".into()
                    }
                }),
                purpose: p.purpose.clone(),
            })
            .collect();
        let card = self.card(app, &[], &cat);
        let request = rynas_apps::InstallRequest {
            stack: stack.clone(),
            form: req.form.clone(),
            shares: req.shares.clone(),
            folders: req.folders.clone(),
            ports: req.ports.clone(),
            secrets: BTreeMap::new(),
        };
        let mut plan = AppInstallPlan {
            app: m.id.clone(),
            version: m.version.to_string(),
            stack: stack.clone(),
            stack_dir: layout.as_ref().map(|l| l.stack(&stack)).unwrap_or_default(),
            ok: false,
            errors,
            port_conflicts,
            ports,
            ram,
            arch_ok: card.arch_ok,
            rynas_ok: card.rynas_ok,
            secrets: conv(&m.secrets),
            notes: conv(&m.notes),
            warnings,
            ..Default::default()
        };
        let mut rendered = None;
        if plan.errors.is_empty()
            && let Some(l) = &layout
        {
            let ids = self.apps_ids().unwrap_or((PLACEHOLDER_ID, PLACEHOLDER_ID));
            let provided = self.provided(l, &stack, &bind, ids);
            match rynas_apps::render::render(app, &cat.source, &provided, &request) {
                Ok(r) => {
                    let ssd = self
                        .docker
                        .settings()
                        .apps_ssd_volume
                        .filter(|v| *v != l.volume.trim_start_matches('/'));
                    plan.folders = r
                        .folders
                        .iter()
                        .map(|f| self.planned_folder(app, f, &stack, ssd.as_deref(), &request))
                        .collect();
                    let secrets: BTreeSet<String> =
                        m.secrets.iter().map(|s| s.env.clone()).collect();
                    plan.compose = file_text(&r, COMPOSE_FILE);
                    plan.env = masked_env(&file_text(&r, ".env"), &secrets);
                    plan.images = r.images.iter().map(|(_, i)| i.to_string()).collect();
                    plan.overlays = r.overlays.clone();
                    rendered = Some(r);
                }
                Err(e) => plan.errors.push(field_error(&e)),
            }
        }
        plan.ok = plan.errors.is_empty()
            && plan.port_conflicts.is_empty()
            && !matches!(plan.ram, RamGate::BelowMinimum { .. })
            && plan.arch_ok
            && plan.rynas_ok
            && docker_installed
            && rendered.is_some();
        Ok(Prepared {
            plan,
            layout,
            request,
            rendered,
            bind,
            docker_installed,
        })
    }

    fn planned_folder(
        &self,
        app: &CatalogApp,
        f: &FolderPlan,
        stack: &str,
        ssd: Option<&str>,
        req: &rynas_apps::InstallRequest,
    ) -> PlannedFolder {
        let rel = app
            .manifest
            .folders
            .iter()
            .find(|x| x.env == f.env)
            .map(|x| x.path.clone());
        let suggested = match (ssd, &rel) {
            (Some(v), Some(rel))
                if !f.share
                    && f.ssd != rynas_apps::manifest::Ssd::No
                    && !req.folders.contains_key(&f.env) =>
            {
                Some(format!("/{v}/docker/appdata/{stack}/{rel}"))
            }
            _ => None,
        };
        PlannedFolder {
            env: f.env.clone(),
            path: f.host_path.clone(),
            intent: f.intent.as_ref().map(conv),
            ssd: conv(&f.ssd),
            share: f.share,
            exists: rynas_docker::under(&self.docker.root, &f.host_path).exists(),
            owner: format!("{}:{}", f.owner.0, f.owner.1),
            suggested,
        }
    }

    /// The installed app stacks.
    pub async fn installed(&self) -> Vec<InstalledApp> {
        let (cat, _) = self.catalog().await;
        let disk = self.on_disk();
        let cs = self.docker.containers().await.ok();
        let l = self.docker.layout();
        let mut v: Vec<InstalledApp> = disk
            .iter()
            .map(|d| self.installed_one(&cat, d, cs.as_deref(), l.as_ref()))
            .collect();
        v.sort_by(|a, b| a.stack.cmp(&b.stack));
        v
    }

    fn installed_one(
        &self,
        cat: &Catalog,
        d: &OnDisk,
        cs: Option<&[model::Seen]>,
        l: Option<&Layout>,
    ) -> InstalledApp {
        let entry = cat.get(&d.meta.app);
        let env = rynas_apps::render::parse_env(&d.env);
        let (state, containers, running) = match cs {
            Some(cs) => {
                let mine: Vec<_> = cs
                    .iter()
                    .filter(|c| c.c.stack.as_deref() == Some(d.stack.as_str()))
                    .collect();
                let total = mine.len() as u32;
                let run = mine
                    .iter()
                    .filter(|c| c.c.state == ContainerState::Running)
                    .count() as u32;
                (Some(model::state_for(run, total)), total, run)
            }
            None => (None, 0, 0),
        };
        let web = entry.and_then(|a| {
            let w = a.manifest.web.as_ref()?;
            let p = a.manifest.port(&w.port)?;
            Some((env.get(&p.env)?.parse::<u16>().ok()?, w.path.clone()))
        });
        InstalledApp {
            stack: d.stack.clone(),
            app: d.meta.app.clone(),
            name: entry
                .map(|a| a.manifest.name.clone())
                .unwrap_or_else(|| d.meta.app.clone()),
            version: d.meta.version.to_string(),
            app_version: d.meta.app_version.clone(),
            catalog: d.meta.catalog.clone(),
            in_catalog: entry.is_some(),
            catalog_version: entry.map(|a| a.manifest.version.to_string()),
            update_available: Self::update_available(cat, d),
            state,
            containers,
            running,
            stack_dir: l.map(|l| l.stack(&d.stack)).unwrap_or_default(),
            web_port: web.as_ref().map(|w| w.0),
            web_path: web.map(|w| w.1),
            bind: env.get("RYNAS_BIND").cloned(),
            snapshots: l.is_some_and(|l| snapshots::supported(&self.docker.root, l, &d.stack)),
        }
    }

    async fn update_plan(&self, stack: &str) -> Res<(Arc<Catalog>, Layout, OnDisk, AppUpdatePlan)> {
        let (cat, _) = self.catalog().await;
        let (l, d) = self.one_on_disk(stack)?;
        let app = cat.get(&d.meta.app).ok_or_else(|| {
            Error::not_found(format!(
                "the app {} of stack {stack} is no longer in the catalogue",
                d.meta.app
            ))
        })?;
        let p = rynas_apps::plan::update_plan(app, &d.compose, &d.env).map_err(render_err)?;
        let plan = AppUpdatePlan {
            stack: stack.into(),
            app: p.app,
            from_version: p.from_version.to_string(),
            to_version: p.to_version.to_string(),
            from_app_version: p.from_app_version,
            to_app_version: p.to_app_version,
            up_to_date: p.up_to_date,
            downgrade: p.downgrade,
            migrations: p.migrations.iter().map(|v| v.to_string()).collect(),
            image_changes: p
                .image_changes
                .iter()
                .map(|c| ImageChange {
                    service: c.service.clone(),
                    kind: conv::<Option<ImageChangeKind>>(&c.kind)
                        .unwrap_or(ImageChangeKind::Digest),
                    from: c.from.as_ref().map(|i| i.to_string()),
                    to: c.to.as_ref().map(|i| i.to_string()),
                })
                .collect(),
            missing_values: p.missing_values,
            changelog_url: p.changelog_url,
            snapshot: snapshots::supported(&self.docker.root, &l, stack),
        };
        Ok((cat, l, d, plan))
    }
}

// ---------------------------------------------------------------------------------------------
// tasks
// ---------------------------------------------------------------------------------------------

/// Creates the folders of a render below the root: missing parents one by one, each with
/// the folder's owner (not under `--dry-system`). Shares are only checked.
fn make_folders(s: &DockerSvc, folders: &[FolderPlan], log: &dyn Fn(&str)) -> Result<(), String> {
    for f in folders {
        let p = rynas_docker::under(&s.root, &f.host_path);
        if f.share {
            if !p.is_dir() {
                log(&format!(
                    "warning: shared folder {} does not exist",
                    f.host_path
                ));
            }
            continue;
        }
        if p.exists() {
            continue;
        }
        let mut missing = Vec::new();
        let mut cur = p.as_path();
        while !cur.exists() {
            missing.push(cur.to_path_buf());
            match cur.parent() {
                Some(x) => cur = x,
                None => break,
            }
        }
        for d in missing.iter().rev() {
            std::fs::create_dir(d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o750));
            if !s.dry {
                std::os::unix::fs::chown(d, Some(f.owner.0), Some(f.owner.1))
                    .map_err(|e| format!("chown {}: {e}", d.display()))?;
            }
        }
        log(&format!(
            "created folder {} (owner {}:{}{})",
            f.host_path,
            f.owner.0,
            f.owner.1,
            if s.dry {
                ", dry system: not changed"
            } else {
                ""
            }
        ));
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

/// `docker compose pull` / `up -d` of a rynas stack (logged only under `--dry-system`).
async fn compose_run(
    s: &DockerSvc,
    ctx: &TaskCtx,
    stack: &str,
    l: &Layout,
    a: ComposeAction,
) -> Result<(), String> {
    let dir = rynas_docker::under(&s.root, &l.stack(stack));
    let dir_s = dir.to_string_lossy().into_owned();
    let file = rynas_docker::stacks::find_compose(&dir).unwrap_or(COMPOSE_FILE);
    let out = s
        .run(ctx, &compose_cmd(stack, &dir_s, file, None, a))
        .await?;
    if !out.success() {
        return Err(format!(
            "docker compose {} failed: {}",
            a.as_str(),
            rynas_docker::stacks::compose_error(&out.stderr)
        ));
    }
    Ok(())
}

struct InstallJob {
    app: String,
    name: String,
    version: String,
    stack: String,
    layout: Layout,
    request: rynas_apps::InstallRequest,
    bind: String,
    start: bool,
}

async fn install_task(a: &Arc<AppsSvc>, ctx: &TaskCtx, j: InstallJob) -> Result<(), String> {
    let s = &a.docker;
    let dir = j.layout.stack(&j.stack);
    ctx.log(&format!(
        "installing {} {} ({}) as stack {} in {dir}",
        j.name, j.version, j.app, j.stack
    ));
    s.ensure_apps_account(ctx).await?;
    let ids = match a.apps_ids() {
        Some(ids) => ids,
        None if s.dry => {
            ctx.log(&format!(
                "the {} account does not exist (dry system): using {PLACEHOLDER_ID}:{PLACEHOLDER_ID}",
                rynas_docker::APPS_USER
            ));
            (PLACEHOLDER_ID, PLACEHOLDER_ID)
        }
        None => {
            return Err(format!(
                "the {} account could not be created",
                rynas_docker::APPS_USER
            ));
        }
    };
    let (cat, _) = a.catalog().await;
    let app = cat
        .get(&j.app)
        .ok_or_else(|| format!("{} left the catalogue", j.app))?;
    let provided = a.provided(&j.layout, &j.stack, &j.bind, ids);
    let r = rynas_apps::render::render(app, &cat.source, &provided, &j.request)
        .map_err(|e| e.to_string())?;
    ctx.progress(10);
    let (compose, env) = (file_text(&r, COMPOSE_FILE), file_text(&r, ".env"));
    let (s2, l2, n2, log) = (s.clone(), j.layout.clone(), j.stack.clone(), ctx.clone());
    blocking(move || {
        let owner = s2.apps_owner();
        s2.fs(owner)
            .create_stack(&l2, &n2, &compose, &env, &[], &|m| log.log(m))
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .await?;
    ctx.progress(20);
    let keep = |e: String| {
        format!(
            "{e}. The stack stays in {dir} for inspection: start it again with POST /api/v1/docker/stacks/{st}/up, or remove it with POST /api/v1/apps/installed/{st}/uninstall {{\"delete_data\": true, \"confirm\": \"{st}\"}}",
            st = j.stack
        )
    };
    let (s2, folders, log) = (s.clone(), r.folders.clone(), ctx.clone());
    blocking(move || make_folders(&s2, &folders, &|m| log.log(m)))
        .await
        .map_err(keep)?;
    s.changed();
    if j.start {
        ctx.log("starting the stack (images are downloaded first; this can take a while)");
        compose_run(s, ctx, &j.stack, &j.layout, ComposeAction::Up)
            .await
            .map_err(keep)?;
    } else {
        ctx.log(&format!(
            "not started: start it with POST /api/v1/docker/stacks/{}/up",
            j.stack
        ));
    }
    ctx.progress(100);
    ctx.log(&format!("{} installed", j.name));
    Ok(())
}

struct UpdateJob {
    stack: String,
    layout: Layout,
    old: (String, String),
    new: (String, String),
    folders: Vec<FolderPlan>,
    snapshot: bool,
    stop_first: bool,
    from: String,
    to: String,
}

async fn write_stack(
    s: &Arc<DockerSvc>,
    l: &Layout,
    stack: &str,
    files: &(String, String),
) -> Result<(), String> {
    let (s2, l2, n2, c, e) = (
        s.clone(),
        l.clone(),
        stack.to_string(),
        files.0.clone(),
        files.1.clone(),
    );
    blocking(move || {
        s2.fs(s2.apps_owner())
            .write_files(&l2, &n2, &c, &e)
            .map_err(|e| e.to_string())
    })
    .await
}

async fn update_task(s: &Arc<DockerSvc>, ctx: &TaskCtx, j: UpdateJob) -> Result<(), String> {
    ctx.log(&format!(
        "updating stack {} from {} to {}",
        j.stack, j.from, j.to
    ));
    let snap = if j.snapshot {
        pre_snapshot(s, ctx, &j.stack, &j.layout, "update", j.stop_first).await?
    } else {
        None
    };
    ctx.progress(20);
    let r = async {
        ctx.log("writing the new compose.yaml and .env (values migrated, secrets kept)");
        write_stack(s, &j.layout, &j.stack, &j.new).await?;
        let (s2, folders, log) = (s.clone(), j.folders.clone(), ctx.clone());
        blocking(move || make_folders(&s2, &folders, &|m| log.log(m))).await?;
        compose_run(s, ctx, &j.stack, &j.layout, ComposeAction::Pull).await?;
        ctx.progress(70);
        compose_run(s, ctx, &j.stack, &j.layout, ComposeAction::Up).await
    }
    .await;
    match (r, snap) {
        (Ok(()), _) => {
            ctx.progress(100);
            ctx.log(&format!("{} updated to {}", j.stack, j.to));
            Ok(())
        }
        (Err(e), Some(sn)) => {
            ctx.log(&format!(
                "update failed ({e}): rolling back to snapshot {}",
                sn.id
            ));
            match do_rollback(s, ctx, &j.stack, &j.layout, &sn.id).await {
                Ok(()) => Err(format!("update failed and was rolled back: {e}")),
                Err(e2) => Err(format!(
                    "update failed ({e}) and the rollback failed too: {e2}"
                )),
            }
        }
        (Err(e), None) => {
            ctx.log(&format!(
                "update failed ({e}): no snapshot (not a subvolume), putting the previous files back"
            ));
            let back = async {
                write_stack(s, &j.layout, &j.stack, &j.old).await?;
                compose_run(s, ctx, &j.stack, &j.layout, ComposeAction::Up).await
            }
            .await;
            match back {
                Ok(()) => Err(format!(
                    "update failed and the previous files were restored: {e}"
                )),
                Err(e2) => Err(format!(
                    "update failed ({e}) and restoring the previous files failed too: {e2}"
                )),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------------------------

async fn catalog_get(State(st): State<AppsState>, hd: HeaderMap) -> Res<Json<AppCatalog>> {
    let a = asvc(&st)?;
    admin(&a, &hd)?;
    Ok(Json(a.list().await))
}

async fn detail_get(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Res<Json<AppDetail>> {
    let a = asvc(&st)?;
    admin(&a, &hd)?;
    Ok(Json(a.detail(&id).await?))
}

/// An image file of an entry: bounded, typed by extension, cached by entity tag.
fn image(path: &Path, max: u64, hd: &HeaderMap) -> Res<Response> {
    let ct = match path.extension().and_then(|e| e.to_str()) {
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => return Err(Error::not_found("no such image")),
    };
    let meta = std::fs::metadata(path).map_err(|_| Error::not_found("no such image"))?;
    if !meta.is_file() || meta.len() > max {
        return Err(Error::not_found("no such image"));
    }
    let etag = format!("\"{:x}-{:x}\"", meta.len(), meta.mtime());
    let mut headers = vec![
        (header::CONTENT_TYPE, HeaderValue::from_static(ct)),
        (
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ),
        (
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"),
        ),
        (
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=3600"),
        ),
    ];
    if let Ok(v) = HeaderValue::from_str(&etag) {
        headers.push((header::ETAG, v));
    }
    if hd
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag)
    {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        for (k, v) in headers {
            r.headers_mut().insert(k, v);
        }
        return Ok(r);
    }
    let bytes = std::fs::read(path).map_err(Error::internal)?;
    if bytes.len() as u64 > max {
        return Err(Error::not_found("no such image"));
    }
    let mut r = bytes.into_response();
    for (k, v) in headers {
        r.headers_mut().insert(k, v);
    }
    Ok(r)
}

async fn icon_get(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Res<Response> {
    let a = asvc(&st)?;
    admin(&a, &hd)?;
    let (cat, _) = a.catalog().await;
    let app = cat
        .get(&id)
        .ok_or_else(|| Error::not_found(format!("no app {id} in the catalogue")))?;
    image(&app.icon, MAX_ICON, &hd)
}

async fn screenshot_get(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath((id, file)): UrlPath<(String, String)>,
) -> Res<Response> {
    let a = asvc(&st)?;
    admin(&a, &hd)?;
    let (cat, _) = a.catalog().await;
    let app = cat
        .get(&id)
        .ok_or_else(|| Error::not_found(format!("no app {id} in the catalogue")))?;
    // only a file the loader listed: no path from the request reaches the filesystem
    let path = app
        .screenshots
        .iter()
        .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(file.as_str()))
        .ok_or_else(|| Error::not_found("no such screenshot"))?;
    image(path, MAX_SCREENSHOT, &hd)
}

fn install_request(body: &axum::body::Bytes) -> Res<AppInstallRequest> {
    optional_body(body)
}

async fn plan_post(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: axum::body::Bytes,
) -> Res<Json<AppInstallPlan>> {
    let a = asvc(&st)?;
    admin(&a, &hd)?;
    let req = install_request(&body)?;
    Ok(Json(a.prepare(&id, &req).await?.plan))
}

async fn install_post(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: axum::body::Bytes,
) -> Res<Response> {
    let a = asvc(&st)?;
    let who = admin(&a, &hd)?;
    let req = install_request(&body)?;
    let p = a.prepare(&id, &req).await?;
    if !p.docker_installed {
        return Err(not_installed());
    }
    let Some(layout) = p.layout.clone() else {
        return Err(Error::invalid(
            "volume",
            "choose a volume for Docker first (Docker settings)",
        ));
    };
    if let Some(e) = p.plan.errors.first() {
        if e.kind == FieldErrorKind::BadStack && e.message.contains("exists") {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::ALREADY_EXISTS,
                e.message.clone(),
            )
            .field("stack"));
        }
        return Err(Error::invalid(e.field.clone(), e.message.clone()));
    }
    if let Some(c) = p.plan.port_conflicts.first() {
        return Err(conflict(format!(
            "port {}/{} is already in use{}",
            c.port,
            serde_json::to_value(c.protocol)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            c.suggestion
                .map(|n| format!(" (free: {n})"))
                .unwrap_or_default()
        ))
        .field(c.env.clone()));
    }
    if let RamGate::BelowMinimum {
        installed_mb,
        minimum_mb,
    } = p.plan.ram
    {
        return Err(conflict(format!(
            "{installed_mb} MiB of RAM, the app needs at least {minimum_mb} MiB"
        ))
        .field("ram"));
    }
    if !p.plan.arch_ok {
        return Err(conflict(format!("the app has no images for {}", host_arch())).field("arch"));
    }
    if !p.plan.rynas_ok {
        return Err(conflict("the app needs a newer rynas").field("min_rynas"));
    }
    let Some(r) = p.rendered else {
        return Err(Error::internal("the app could not be rendered"));
    };
    let (cat, _) = a.catalog().await;
    let app = cat
        .get(&id)
        .ok_or_else(|| Error::not_found(format!("no app {id} in the catalogue")))?;
    let m = &app.manifest;
    // the task renders again with these secrets and the real docker-apps ids
    let mut request = p.request;
    for s in &m.secrets {
        if let Some(v) = r.env.get(&s.env) {
            request.secrets.insert(s.env.clone(), v.clone());
        }
    }
    let shown = m
        .secrets
        .iter()
        .filter(|s| s.show)
        .filter_map(|s| {
            Some(ShownSecret {
                env: s.env.clone(),
                description: s.description.clone(),
                value: r.env.get(&s.env)?.clone(),
            })
        })
        .collect();
    let stack = p.plan.stack.clone();
    let guard: OwnedBusy = mark_busy(&a.docker, &stack)?;
    audit(&who, "install", &format!("{id} as {stack}"));
    let job = InstallJob {
        app: id.clone(),
        name: m.name.clone(),
        version: m.version.to_string(),
        stack: stack.clone(),
        layout: layout.clone(),
        request,
        bind: p.bind,
        start: req.start,
    };
    let a2 = a.clone();
    let task_id = a
        .docker
        .ops
        .tasks
        .spawn("apps.install", &who, move |ctx| async move {
            let _guard = guard;
            install_task(&a2, &ctx, job).await
        })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(AppInstallStarted {
            task_id,
            stack_dir: layout.stack(&stack),
            stack,
            secrets: shown,
        }),
    )
        .into_response())
}

async fn installed_get(State(st): State<AppsState>, hd: HeaderMap) -> Res<Json<Vec<InstalledApp>>> {
    let a = asvc(&st)?;
    admin(&a, &hd)?;
    Ok(Json(a.installed().await))
}

async fn installed_one_get(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(stack): UrlPath<String>,
) -> Res<Json<InstalledAppDetail>> {
    let a = asvc(&st)?;
    admin(&a, &hd)?;
    let (cat, _) = a.catalog().await;
    let (l, d) = a.one_on_disk(&stack)?;
    let cs = a.docker.containers().await.ok();
    let app = a.installed_one(&cat, &d, cs.as_deref(), Some(&l));
    Ok(Json(InstalledAppDetail {
        app,
        snapshots: snapshots::list(&a.docker.root, &l, &stack),
    }))
}

async fn update_plan_get(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(stack): UrlPath<String>,
) -> Res<Json<AppUpdatePlan>> {
    let a = asvc(&st)?;
    admin(&a, &hd)?;
    Ok(Json(a.update_plan(&stack).await?.3))
}

async fn update_post(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(stack): UrlPath<String>,
    body: axum::body::Bytes,
) -> Res<Response> {
    let a = asvc(&st)?;
    let who = admin(&a, &hd)?;
    let req: AppUpdateRequest = optional_body(&body)?;
    let (cat, l, d, plan) = a.update_plan(&stack).await?;
    if plan.downgrade {
        return Err(conflict(format!(
            "the catalogue has {} {}, older than the installed {}",
            plan.app, plan.to_version, plan.from_version
        )));
    }
    if plan.up_to_date {
        return Err(conflict(format!(
            "{stack} is up to date ({})",
            plan.from_version
        )));
    }
    if let Some(f) = plan
        .missing_values
        .iter()
        .find(|f| !req.answers.contains_key(*f))
    {
        return Err(Error::invalid(
            f.clone(),
            format!("the new version needs a value for {f}"),
        ));
    }
    let app = cat
        .get(&d.meta.app)
        .ok_or_else(|| Error::not_found("no such app"))?;
    let env = rynas_apps::render::parse_env(&d.env);
    let ids = match (
        env.get("PUID").and_then(|v| v.parse().ok()),
        env.get("PGID").and_then(|v| v.parse().ok()),
    ) {
        (Some(u), Some(g)) => (u, g),
        _ => a.apps_ids().unwrap_or((PLACEHOLDER_ID, PLACEHOLDER_ID)),
    };
    let bind = env
        .get("RYNAS_BIND")
        .cloned()
        .unwrap_or_else(|| "127.0.0.1".into());
    let provided = a.provided(&l, &stack, &bind, ids);
    let r = rynas_apps::plan::prepare_update(
        app,
        &cat.source,
        &provided,
        &stack,
        &d.compose,
        &d.env,
        &req.answers,
    )
    .map_err(render_err)?;
    let set = a.docker.settings();
    let job = UpdateJob {
        stack: stack.clone(),
        layout: l,
        old: (d.compose.clone(), d.env.clone()),
        new: (file_text(&r, COMPOSE_FILE), file_text(&r, ".env")),
        folders: r.folders.clone(),
        snapshot: req.snapshot.unwrap_or(set.snapshot_before_update),
        stop_first: req.stop_first.unwrap_or(set.stop_for_snapshot),
        from: plan.from_version.clone(),
        to: plan.to_version.clone(),
    };
    let guard = mark_busy(&a.docker, &stack)?;
    audit(
        &who,
        "update",
        &format!("{stack} {} -> {}", plan.from_version, plan.to_version),
    );
    let s2 = a.docker.clone();
    let id = a
        .docker
        .ops
        .tasks
        .spawn("apps.update", &who, move |ctx| async move {
            let _guard = guard;
            update_task(&s2, &ctx, job).await
        })?;
    Ok(accepted(id))
}

async fn rollback_post(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(stack): UrlPath<String>,
    body: Result<Json<AppRollbackRequest>, JsonRejection>,
) -> Res<Response> {
    let a = asvc(&st)?;
    let who = admin(&a, &hd)?;
    let Json(req) = body?;
    if rynas_docker::parse_stamp(&req.snapshot).is_none() {
        return Err(Error::invalid("snapshot", "not a snapshot id"));
    }
    let (l, _) = a.one_on_disk(&stack)?;
    if snapshots::get(&a.docker.root, &l, &stack, &req.snapshot).is_none() {
        return Err(Error::not_found(format!(
            "no snapshot {} of {stack}",
            req.snapshot
        )));
    }
    let guard = mark_busy(&a.docker, &stack)?;
    audit(&who, "rollback", &format!("{stack} to {}", req.snapshot));
    let (s2, n2) = (a.docker.clone(), stack.clone());
    let id = a
        .docker
        .ops
        .tasks
        .spawn("apps.rollback", &who, move |ctx| async move {
            let _guard = guard;
            do_rollback(&s2, &ctx, &n2, &l, &req.snapshot).await
        })?;
    Ok(accepted(id))
}

async fn convert_post(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(stack): UrlPath<String>,
) -> Res<StatusCode> {
    let a = asvc(&st)?;
    let who = admin(&a, &hd)?;
    let (l, d) = a.one_on_disk(&stack)?;
    let compose = rynas_apps::render::convert_to_custom(&d.compose).map_err(render_err)?;
    let _guard = mark_busy(&a.docker, &stack)?;
    audit(&who, "convert", &stack);
    let compose = format!(
        "# Converted from the rynas app {} {} to a custom stack: rynas no longer updates it.\n{compose}",
        d.meta.app, d.meta.version
    );
    write_stack(&a.docker, &l, &stack, &(compose, d.env))
        .await
        .map_err(Error::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn uninstall_post(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(stack): UrlPath<String>,
    body: axum::body::Bytes,
) -> Res<Response> {
    let a = asvc(&st)?;
    let who = admin(&a, &hd)?;
    let req: AppUninstallRequest = optional_body(&body)?;
    if req.delete_data && req.confirm.as_deref() != Some(stack.as_str()) {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            error_codes::CONFIRMATION_REQUIRED,
            "deleting the app's data needs the stack name as confirm".into(),
        )
        .field("confirm"));
    }
    let (cat, _) = a.catalog().await;
    let (l, d) = a.one_on_disk(&stack)?;
    // folders placed outside the stack folder (an SSD volume) go with the data
    let env = rynas_apps::render::parse_env(&d.env);
    let outside: Vec<String> = cat
        .get(&d.meta.app)
        .map(|app| {
            app.manifest
                .folders
                .iter()
                .filter_map(|f| env.get(&f.env))
                .filter(|v| {
                    v.starts_with('/') && !rynas_docker::stacks::is_below(v, &l.stack(&stack))
                })
                .filter(|v| on_volume(&a.docker.root, v))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let guard = mark_busy(&a.docker, &stack)?;
    audit(
        &who,
        if req.delete_data {
            "uninstall+delete"
        } else {
            "uninstall"
        },
        &stack,
    );
    let (s2, n2) = (a.docker.clone(), stack.clone());
    let id = a.docker.ops.tasks.spawn("apps.uninstall", &who, move |ctx| async move {
        let _guard = guard;
        let s = &s2;
        ctx.log(&format!("stopping and removing the containers of {n2}"));
        compose_run(s, &ctx, &n2, &l, ComposeAction::Down).await?;
        if !req.delete_data {
            ctx.log(&format!(
                "the stack folder {} stays (compose.yaml, .env, data): start it again with POST /api/v1/docker/stacks/{n2}/up, or delete it with POST /api/v1/apps/installed/{n2}/uninstall {{\"delete_data\": true, \"confirm\": \"{n2}\"}}",
                l.stack(&n2)
            ));
            return Ok(());
        }
        if s.dry {
            for p in outside.iter().chain(std::iter::once(&l.stack(&n2))) {
                ctx.log(&format!("deleting {p} (dry system: not done)"));
            }
            return Ok(());
        }
        let (s3, l3, n3, log) = (s.clone(), l.clone(), n2.clone(), ctx.clone());
        blocking(move || {
            for p in &outside {
                let real = rynas_docker::under(&s3.root, p);
                if real.exists() {
                    rynas_docker::stacks::remove_tree(&real, &|m| log.log(m))?;
                }
            }
            s3.fs(None).remove_stack_dir(&l3, &n3, &|m| log.log(m))
        })
        .await?;
        ctx.log(&format!("{n2} and its data deleted"));
        Ok(())
    })?;
    Ok(accepted(id))
}

pub fn router(st: AppsState) -> Router {
    Router::new()
        .route("/v1/apps/catalog", get(catalog_get))
        .route("/v1/apps/catalog/{id}", get(detail_get))
        .route("/v1/apps/catalog/{id}/icon", get(icon_get))
        .route(
            "/v1/apps/catalog/{id}/screenshots/{file}",
            get(screenshot_get),
        )
        .route("/v1/apps/catalog/{id}/plan", post(plan_post))
        .route("/v1/apps/catalog/{id}/install", post(install_post))
        .route("/v1/apps/installed", get(installed_get))
        .route("/v1/apps/installed/{stack}", get(installed_one_get))
        .route(
            "/v1/apps/installed/{stack}/update",
            get(update_plan_get).post(update_post),
        )
        .route("/v1/apps/installed/{stack}/rollback", post(rollback_post))
        .route("/v1/apps/installed/{stack}/convert", post(convert_post))
        .route("/v1/apps/installed/{stack}/uninstall", post(uninstall_post))
        .with_state(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_net_listeners() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("proc/net")).unwrap();
        std::fs::write(
            dir.path().join("proc/net/tcp"),
            "  sl  local_address rem_address   st\n   0: 00000000:1F90 00000000:0000 0A 0\n   1: 0100007F:0050 0100007F:9C40 01 0\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("proc/net/udp6"),
            "  sl  local_address rem_address   st\n   0: 00000000000000000000000000000000:14E9 00000000000000000000000000000000:0000 07 0\n",
        )
        .unwrap();
        let p = listening_ports(dir.path());
        assert_eq!(
            p,
            vec![
                UsedPort {
                    port: 8080,
                    protocol: rynas_apps::manifest::Protocol::Tcp
                },
                UsedPort {
                    port: 5353,
                    protocol: rynas_apps::manifest::Protocol::Udp
                },
            ]
        );
    }

    #[test]
    fn masks_secrets_and_checks_volumes() {
        let s: BTreeSet<String> = ["DB_PASSWORD".to_string()].into();
        assert_eq!(
            masked_env("# x\nDB_PASSWORD='a b'\nPUID=1\n", &s),
            format!("# x\nDB_PASSWORD={MASK}\nPUID=1\n")
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vol1")).unwrap();
        assert!(on_volume(dir.path(), "/vol1/media"));
        assert!(!on_volume(dir.path(), "/vol1"));
        assert!(!on_volume(dir.path(), "/vol2/media"));
        assert!(!on_volume(dir.path(), "/vol1/../etc"));
        assert!(!on_volume(dir.path(), "relative/x"));
    }
}
