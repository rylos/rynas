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
//! - Web access (W1, [`crate::web_access`]): while it is active an install binds proxied
//!   ports to loopback and adds a route; otherwise it publishes them on the LAN
//!   (`docs/phase2/docker-apps.md` §7). The proxy's ports (80/443) count as used unless
//!   external proxy mode is on. "Move behind the proxy" (`POST .../proxy`) re-binds an
//!   installed app to loopback and adds its route.
//! - Signed online catalogue updates ([`online`], §6.12): the shipped catalogue is merged with
//!   the last verified download (`<state>/catalog/current`), the higher runnable version of
//!   each entry wins.

pub mod online;

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
use axum::routing::{get, post, put};
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
use crate::web_access::{AppRouteSpec, WebAccessSvc};

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
    /// Last request that read the catalogue ([`AppsSvc::trim_idle`]).
    used: Instant,
    catalog: Arc<Catalog>,
    issues: Arc<Vec<CatalogIssue>>,
}

/// What the app endpoints work on.
pub struct AppsSvc {
    pub docker: Arc<DockerSvc>,
    /// The catalogue folder (`apps/<id>/...`).
    pub dir: PathBuf,
    cache: Mutex<Option<Loaded>>,
    /// Web access, when this rynasd runs it (routes for apps, the proxy's ports).
    pub web: std::sync::OnceLock<Arc<WebAccessSvc>>,
    /// Signed online catalogue updates (`<state>/catalog`), when this rynasd runs them.
    pub online: std::sync::OnceLock<Arc<online::Online>>,
}

pub type AppsState = Option<Arc<AppsSvc>>;

pub(crate) fn asvc(st: &AppsState) -> Res<Arc<AppsSvc>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "the app catalog is not enabled in this rynasd".into(),
        )
    })
}

/// The user, who must be an administrator (reads included).
pub(crate) fn admin(s: &AppsSvc, hd: &HeaderMap) -> Res<String> {
    let who = user(hd)?;
    s.docker
        .ops
        .admins
        .require_admin(&who, "the app catalog")?;
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
                message: format!("cannot read the catalog: {e}"),
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
    (
        Catalog {
            source: CATALOG_SOURCE.into(),
            apps,
        },
        issues,
    )
}

/// The running rynas version (entries' `min_rynas`).
fn running_version() -> Version {
    env!("CARGO_PKG_VERSION")
        .parse()
        .unwrap_or(Version::new(0, 0, 0))
}

/// [`stamp_of`] the shipped catalogue plus the downloaded catalogue in use.
fn sources_stamp(dir: &Path, online: Option<&online::Active>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    stamp_of(dir).hash(&mut h);
    online.map(|a| (a.serial, &a.dir)).hash(&mut h);
    h.finish()
}

/// The shipped catalogue merged with the downloaded one ([`rynas_apps::catalog::merge`]).
/// A download whose serial is not above the shipped `catalog.toml` is ignored; its entries
/// that need a newer rynas (and may not load in this one) are left out without an issue;
/// a broken shipped entry that the download replaces is no issue either.
fn load_sources(dir: &Path, online: Option<&online::Active>) -> (Catalog, Vec<CatalogIssue>) {
    let (shipped, mut issues) = load(dir);
    let shipped_serial = rynas_apps::catalog::read_serial(dir).ok().flatten();
    let cat = match online.filter(|a| shipped_serial.is_none_or(|s| a.serial > s)) {
        None => shipped,
        Some(a) => {
            let running = running_version();
            let (dl, dl_issues) = load(&a.dir);
            let newer: BTreeSet<&str> = a
                .index
                .apps
                .iter()
                .filter(|x| x.min_rynas > running)
                .map(|x| x.id.as_str())
                .collect();
            issues.retain(|i| !dl.apps.contains_key(&i.app));
            issues.extend(
                dl_issues
                    .into_iter()
                    .filter(|i| !newer.contains(i.app.as_str())),
            );
            rynas_apps::catalog::merge(shipped, dl, running).0
        }
    };
    for i in &issues {
        tracing::warn!("apps: catalog entry {}/{}: {}", i.app, i.file, i.message);
    }
    (cat, issues)
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

/// `/<label>/...` below a mounted rynas volume (`mounted`: [`DockerSvc::volume_mounted`],
/// the mount table on the real system).
fn on_volume(mounted: &dyn Fn(&str) -> bool, p: &str) -> bool {
    volume_of(mounted, p).is_some()
}

/// The label of the mounted volume `p` is below ([`on_volume`]); the system volume's
/// folder counts (D-ROOTVOL).
fn volume_of(mounted: &dyn Fn(&str) -> bool, p: &str) -> Option<String> {
    let (label, rest) = rynas_api_types::storage::split_volume_path(p)?;
    (!rest.is_empty()
        && !rest.split('/').any(|s| s.is_empty() || s == ".." || s == ".")
        && !p.contains(['\n', '\0', '\'', ':'])
        && mounted(&label))
    .then_some(label)
}

/// The folders an install (or an update) made, kept by rynasd in its own state
/// (`<state>/docker/app-folders/<stack>.json`, root only), never read back from the stack's
/// files: uninstall with `delete_data` deletes only these (review 2026-11-07, H2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
struct MadeFolders {
    /// Folders of the render made by rynas (the whole tree goes with the data).
    #[serde(default)]
    folders: Vec<String>,
    /// Parents made on the way (deleted only when empty).
    #[serde(default)]
    parents: Vec<String>,
    /// Share permission entries the app's installs and share set changes added or raised
    /// (an uninstall with `remove_grants` takes them back).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    grants: Vec<rynas_api_types::apps::AppAddedGrant>,
}

impl MadeFolders {
    fn add(&mut self, other: MadeFolders) {
        for g in other.grants {
            match self.grants.iter_mut().find(|x| x.share == g.share && x.kind == g.kind && x.name == g.name) {
                // the first change keeps what the entry was before the app
                Some(x) => x.access = g.access,
                None => self.grants.push(g),
            }
        }
        for f in other.folders {
            if !self.folders.contains(&f) {
                self.folders.push(f);
            }
        }
        for f in other.parents {
            if !self.parents.contains(&f) {
                self.parents.push(f);
            }
        }
    }
}

fn made_path(s: &DockerSvc, stack: &str) -> PathBuf {
    s.state_dir.join("app-folders").join(format!("{stack}.json"))
}

fn read_made(s: &DockerSvc, stack: &str) -> MadeFolders {
    std::fs::read_to_string(made_path(s, stack))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Adds folders to the record of a stack.
fn record_made(s: &DockerSvc, stack: &str, made: MadeFolders) -> Result<(), String> {
    if made == MadeFolders::default() {
        return Ok(());
    }
    let mut all = read_made(s, stack);
    all.add(made);
    let p = made_path(s, stack);
    let dir = p.parent().expect("has a parent");
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let text = serde_json::to_vec_pretty(&all).map_err(|e| e.to_string())?;
    crate::tls::write_atomic(&p, &text, 0o600, None).map_err(|e| format!("{}: {e}", p.display()))
}

/// Paths no uninstall ever deletes, whatever a record says.
const SYSTEM_TOPS: &[&str] = &[
    "bin", "boot", "dev", "etc", "home", "lib", "lib32", "lib64", "libx32", "media", "mnt", "nix",
    "opt", "proc", "root", "run", "sbin", "snap", "srv", "sys", "tmp", "usr", "var",
];

/// What an uninstall with `delete_data` deletes outside the stack folder: the recorded
/// folders that are below a mounted volume (`mounted`), deeper than its root, not a system
/// path, not inside the stack folder (it goes as a whole), and with no mount point at or
/// below them (`mounts`). Returns (trees to delete, empty parents to remove, kept with why).
fn uninstall_targets(
    made: &MadeFolders,
    stack_dir: &str,
    mounted: &dyn Fn(&str) -> bool,
    mounts: &[String],
) -> (Vec<String>, Vec<String>, Vec<(String, &'static str)>) {
    let check = |p: &str| -> Result<(), &'static str> {
        let top = p.trim_start_matches('/').split('/').next().unwrap_or("");
        // the system volume's folder is below /srv but is a volume (D-ROOTVOL)
        if SYSTEM_TOPS.contains(&top) && !crate::places::in_system_dir(p) {
            return Err("a system folder");
        }
        if !on_volume(mounted, p) {
            return Err("not below a mounted volume");
        }
        if mounts
            .iter()
            .any(|m| rynas_docker::stacks::is_below(m, p))
        {
            return Err("holds a mount point");
        }
        Ok(())
    };
    let (mut trees, mut empties, mut kept) = (Vec::new(), Vec::new(), Vec::new());
    for p in &made.folders {
        if rynas_docker::stacks::is_below(p, stack_dir) {
            continue;
        }
        match check(p) {
            Ok(()) => trees.push(p.clone()),
            Err(why) => kept.push((p.clone(), why)),
        }
    }
    for p in &made.parents {
        if rynas_docker::stacks::is_below(p, stack_dir) || trees.contains(p) {
            continue;
        }
        match check(p) {
            Ok(()) => empties.push(p.clone()),
            Err(why) => kept.push((p.clone(), why)),
        }
    }
    // deepest first, so that parents are empty when their turn comes
    empties.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
    (trees, empties, kept)
}

/// The uid range of NAS users (Debian's `UID_MIN`..`UID_MAX`; system accounts sit below it,
/// `nobody` above).
const NAS_UIDS: std::ops::RangeInclusive<u32> = 1000..=59999;

/// The NAS users of `passwd` an app may run as: `(name, uid, primary gid)`, regular uids
/// only, never root, a system account or the apps' account; sorted by name.
pub(crate) fn nas_users(passwd: &str) -> Vec<(String, u32, u32)> {
    let mut v: Vec<(String, u32, u32)> = passwd
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            let (uid, gid) = (f.get(2)?.parse::<u32>().ok()?, f.get(3)?.parse::<u32>().ok()?);
            let ok = NAS_UIDS.contains(&uid) && gid != 0 && f[0] != rynas_docker::APPS_USER && rynas_apps::manifest::is_user_name(f[0]);
            ok.then(|| (f[0].to_string(), uid, gid))
        })
        .collect();
    v.sort();
    v
}

/// `(uid, gid)` of the NAS user `name` an app may run as ([`nas_users`]), or why not.
pub(crate) fn nas_user_ids(passwd: &str, name: &str) -> Result<(u32, u32), String> {
    if name == "root" {
        return Err("an app never runs as root".into());
    }
    if name == rynas_docker::APPS_USER {
        return Err(format!("{name} is the apps' own account: choose a NAS user, or none"));
    }
    let line = passwd.lines().find(|l| l.split(':').next() == Some(name)).ok_or_else(|| format!("no user {name}"))?;
    match nas_users(line).into_iter().next() {
        Some((_, uid, gid)) => Ok((uid, gid)),
        None => Err(format!("{name} is a system account, not a NAS user")),
    }
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
pub(crate) fn render_err(e: RenderError) -> Error {
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

pub(crate) fn file_text(r: &RenderedStack, path: &str) -> String {
    r.file(path)
        .map(|f| String::from_utf8_lossy(&f.contents).into_owned())
        .unwrap_or_default()
}

/// An installed app stack read from disk.
pub(crate) struct OnDisk {
    pub(crate) stack: String,
    pub(crate) meta: XRynas,
    pub(crate) compose: String,
    pub(crate) env: String,
}

/// What an install needs after the plan accepted it.
struct Prepared {
    plan: AppInstallPlan,
    layout: Option<Layout>,
    request: rynas_apps::InstallRequest,
    rendered: Option<RenderedStack>,
    bind: String,
    docker_installed: bool,
    /// The web access route the install adds (loopback binding, built-in proxy mode).
    route: Option<AppRouteSpec>,
}

impl AppsSvc {
    pub fn new(docker: Arc<DockerSvc>, dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            docker,
            dir,
            cache: Mutex::new(None),
            web: std::sync::OnceLock::new(),
            online: std::sync::OnceLock::new(),
        })
    }

    /// Drops the parsed catalogue when no request read it for `idle` (it is loaded again,
    /// lazily, by the next one): an idle daemon keeps no catalogue in memory.
    pub fn trim_idle(&self, idle: Duration) -> bool {
        let mut g = self.cache.lock().unwrap();
        if g.as_ref().is_some_and(|l| l.used.elapsed() >= idle) {
            *g = None;
            return true;
        }
        false
    }

    /// Web access serves apps (built-in mode, the engine installed).
    pub fn proxy_active(&self) -> bool {
        self.web.get().is_some_and(|w| w.proxy_active())
    }

    /// The catalogue, reloaded when a file changed.
    pub async fn catalog(&self) -> (Arc<Catalog>, Arc<Vec<CatalogIssue>>) {
        if let Some(l) = self.cache.lock().unwrap().as_mut()
            && l.checked.elapsed() < RECHECK
        {
            l.used = Instant::now();
            return (l.catalog.clone(), l.issues.clone());
        }
        let dir = self.dir.clone();
        let online = self.online_use().online;
        let old = self.cache.lock().unwrap().as_ref().map(|l| l.stamp);
        let (stamp, loaded) = tokio::task::spawn_blocking(move || {
            let active = online.as_ref().and_then(|o| o.active());
            let stamp = sources_stamp(&dir, active.as_ref());
            if old == Some(stamp) {
                return (stamp, None);
            }
            (stamp, Some(load_sources(&dir, active.as_ref())))
        })
        .await
        .unwrap_or((0, None));
        let mut g = self.cache.lock().unwrap();
        match (loaded, g.as_mut()) {
            (None, Some(l)) => {
                l.checked = Instant::now();
                l.used = l.checked;
                (l.catalog.clone(), l.issues.clone())
            }
            (loaded, _) => {
                let (c, i) = loaded.unwrap_or_else(|| {
                    // the cache was dropped meanwhile (idle trim): read again, with the
                    // downloaded catalogue the stamp stands for
                    let active = self.online_use().online.and_then(|o| o.active());
                    load_sources(&self.dir, active.as_ref())
                });
                let l = Loaded {
                    stamp,
                    checked: Instant::now(),
                    used: Instant::now(),
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
    pub(crate) fn on_disk(&self) -> Vec<OnDisk> {
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

    pub(crate) fn one_on_disk(&self, stack: &str) -> Res<(Layout, OnDisk)> {
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
                "stack {stack} is not an app of the catalog (no x-rynas block)"
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
            reverse_proxy: m.reverse_proxy,
            featured: m.featured,
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
            .ok_or_else(|| Error::not_found(format!("no app {id} in the catalog")))?;
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
            share_set: m.share_set.as_ref().map(conv),
            run_as: m.run_as.is_some(),
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
    pub(crate) fn provided(
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
    /// `(uid, gid)` of the NAS user an app with `run_as` runs as ([`nas_user_ids`]).
    pub(crate) fn run_as_ids(&self, name: &str) -> Result<(u32, u32), String> {
        nas_user_ids(&read_text(&self.docker.root, "/etc/passwd"), name)
    }

    /// The NAS users an app may run as, sorted by name.
    pub(crate) fn run_as_choices(&self) -> Vec<String> {
        nas_users(&read_text(&self.docker.root, "/etc/passwd")).into_iter().map(|u| u.0).collect()
    }

    /// The ids an installed stack runs as: its `run_as` user's (looked up again, so the files
    /// keep belonging to that user), else the `PUID`/`PGID` stored, else the apps' account.
    pub(crate) fn stack_ids(&self, env: &BTreeMap<String, String>) -> Res<(u32, u32)> {
        if let Some(u) = env.get(rynas_apps::manifest::RUN_AS_ENV).filter(|u| !u.is_empty()) {
            return self.run_as_ids(u).map_err(|e| Error::invalid("run_as", e));
        }
        Ok(match (env.get("PUID").and_then(|v| v.parse().ok()), env.get("PGID").and_then(|v| v.parse().ok())) {
            (Some(u), Some(g)) => (u, g),
            _ => self.apps_ids().unwrap_or((PLACEHOLDER_ID, PLACEHOLDER_ID)),
        })
    }

    pub(crate) fn apps_ids(&self) -> Option<(u32, u32)> {
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
        // the proxy owns its ports from install, running or not (unless external mode)
        if let Some(w) = self.web.get() {
            for (port, udp) in w.reserved_ports() {
                used.push(UsedPort {
                    port,
                    protocol: if udp {
                        rynas_apps::manifest::Protocol::Udp
                    } else {
                        rynas_apps::manifest::Protocol::Tcp
                    },
                });
            }
        }
        used.sort();
        used.dedup();
        (used, ok)
    }

    /// Checks an install request and renders it: the plan, and what the install needs.
    async fn prepare(&self, id: &str, req: &AppInstallRequest) -> Res<Prepared> {
        let (cat, _) = self.catalog().await;
        let app = cat
            .get(id)
            .ok_or_else(|| Error::not_found(format!("no app {id} in the catalog")))?;
        let m = &app.manifest;
        let root = self.docker.root.clone();
        let docker_installed = self.docker.facts(false).await.installed;
        let layout = self.docker.layout();
        let stack = req.stack.clone().unwrap_or_else(|| m.id.clone());
        // §7: loopback plus a route while web access is active, the LAN otherwise
        let proxy_active = self.proxy_active();
        let publish_lan = req.publish_lan.unwrap_or(!proxy_active);
        let bind = if publish_lan { "0.0.0.0" } else { "127.0.0.1" }.to_string();
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
            } else if !on_volume(&|l| self.docker.volume_mounted(l), p) {
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
            } else if !on_volume(&|l| self.docker.volume_mounted(l), p) {
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
        if publish_lan && m.ports.iter().any(|p| p.proxied) {
            warnings.push(PlanWarning::PublishedOnLan);
        }
        if !publish_lan && !proxy_active && m.ports.iter().any(|p| p.proxied) {
            warnings.push(PlanWarning::ProxyInactive);
        }
        let route = (!publish_lan)
            .then(|| route_spec(m, &stack, &req.ports))
            .flatten()
            .filter(|_| self.web.get().is_some_and(|w| !w.reserved_ports().is_empty()));
        let web_url = route.as_ref().and_then(|r| {
            self.web
                .get()
                .and_then(|w| w.preview_url(&r.stack, &r.path))
        });
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
        let share_set = match (&m.share_set, req.share_set.is_empty()) {
            (None, true) => Vec::new(),
            (None, false) => {
                errors.push(err("share_set", FieldErrorKind::UnknownValue, format!("{} has no share set", m.name)));
                Vec::new()
            }
            (Some(ss), _) => match resolve_share_set(&self.applied_shares(), &req.share_set) {
                Ok(items) => {
                    for it in &items {
                        if !it.follow && !rynas_docker::under(&root, &it.path).is_dir() {
                            errors.push(err(&ss.env, FieldErrorKind::ShareMissing, format!("{} does not exist", it.path)));
                        }
                    }
                    items
                }
                Err(e) => {
                    errors.push(err(&ss.env, FieldErrorKind::UnknownValue, e));
                    Vec::new()
                }
            },
        };
        let request = rynas_apps::InstallRequest {
            stack: stack.clone(),
            form: req.form.clone(),
            shares: req.shares.clone(),
            folders: req.folders.clone(),
            ports: req.ports.clone(),
            secrets: BTreeMap::new(),
            share_set,
            run_as: req.run_as.clone().filter(|u| !u.is_empty()),
            user_photos: None,
            user_photos_key: None,
        };
        // the NAS user the app runs as (entries with run_as)
        let mut run_as_ids = None;
        match (&request.run_as, m.run_as) {
            (Some(_), None) => errors.push(err("run_as", FieldErrorKind::UnknownValue, format!("{} always runs as the apps' account", m.name))),
            (Some(u), Some(_)) => match self.run_as_ids(u) {
                Ok(ids) => run_as_ids = Some(ids),
                Err(e) => errors.push(err("run_as", FieldErrorKind::InvalidValue, e)),
            },
            _ => {}
        }
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
            proxy_active,
            web_https: m.web.as_ref().is_some_and(|w| w.scheme == rynas_apps::manifest::Scheme::Https),
            web_url,
            ..Default::default()
        };
        plan.share_grants = plan_share_grants(self, m, &request);
        if m.run_as.is_some() {
            plan.run_as_choices = self.run_as_choices();
        }
        plan.run_as = request.run_as.clone();
        let mut rendered = None;
        if plan.errors.is_empty()
            && let Some(l) = &layout
        {
            let ids = run_as_ids.or_else(|| self.apps_ids()).unwrap_or((PLACEHOLDER_ID, PLACEHOLDER_ID));
            let provided = self.provided(l, &stack, &bind, ids);
            match rynas_apps::render::render(app, &cat.source, &provided, &request) {
                Ok(r) => {
                    let ssd = self
                        .docker
                        .settings()
                        .apps_ssd_volume
                        .filter(|v| rynas_api_types::storage::volume_dir(v) != l.volume);
                    plan.folders = r
                        .folders
                        .iter()
                        .map(|f| self.planned_folder(app, f, &stack, ssd.as_deref(), &request))
                        .collect();
                    let secrets: BTreeSet<String> =
                        m.secrets.iter().map(|s| s.env.clone()).chain(m.user_photos.iter().filter_map(|u| u.key_env.clone())).collect();
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
            route,
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
                Some(format!(
                    "{}/docker/appdata/{stack}/{rel}",
                    rynas_api_types::storage::volume_dir(v)
                ))
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
            nocow: f.nocow,
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
        let web_https = entry
            .and_then(|a| a.manifest.web.as_ref())
            .is_some_and(|w| w.scheme == rynas_apps::manifest::Scheme::Https);
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
            web_https,
            bind: env.get("RYNAS_BIND").cloned(),
            web_url: self.web.get().and_then(|w| w.stack_url(&d.stack)),
            snapshots: l.is_some_and(|l| snapshots::supported(&self.docker.root, l, &d.stack)),
            run_as: env.get(rynas_apps::manifest::RUN_AS_ENV).filter(|u| !u.is_empty()).cloned(),
            share_set: entry
                .and_then(|a| a.manifest.share_set.as_ref())
                .and_then(|ss| env.get(&ss.env))
                .and_then(|v| rynas_apps::render::decode_share_set(v).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|i| InstalledShareSetItem {
                    name: i.name,
                    path: i.path,
                    follow: i.follow,
                })
                .collect(),
            added_grants: read_made(&self.docker, &d.stack).grants,
            user_photos: entry.is_some_and(|a| a.manifest.user_photos.is_some()),
        }
    }

    async fn update_plan(&self, stack: &str) -> Res<(Arc<Catalog>, Layout, OnDisk, AppUpdatePlan)> {
        let (cat, _) = self.catalog().await;
        let (l, d) = self.one_on_disk(stack)?;
        let app = cat.get(&d.meta.app).ok_or_else(|| {
            Error::not_found(format!(
                "the app {} of stack {stack} is no longer in the catalog",
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
            notes: app
                .migrations
                .iter()
                .filter(|m| p.migrations.contains(&m.to))
                .map(|m| m.description.trim().to_string())
                .filter(|d| !d.is_empty())
                .collect(),
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

/// The web access route of an app from its `[web]` block and the chosen host ports.
fn route_spec(
    m: &rynas_apps::manifest::AppManifest,
    stack: &str,
    ports: &BTreeMap<String, u16>,
) -> Option<AppRouteSpec> {
    let w = m.web.as_ref()?;
    let p = m.port(&w.port)?;
    Some(AppRouteSpec {
        stack: stack.into(),
        app: m.id.clone(),
        port: ports.get(&p.env).copied().unwrap_or(p.default),
        scheme: conv(&w.scheme),
        auth: conv(&w.auth),
        path: w.path.clone(),
        websocket: w.websocket,
        subpath: w.subpath,
        body: w.body.clone(),
        timeout: w.timeout.clone(),
        buffering: w.buffering,
    })
}

/// `.env` text with `key` set to `value` (the line replaced, or added at the end).
pub(crate) fn set_env_line(text: &str, key: &str, value: &str) -> String {
    let mut found = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            if l.trim_start().starts_with(&format!("{key}=")) {
                found = true;
                format!("{key}={value}")
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        out.push(format!("{key}={value}"));
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Adds the route of an installed app (a task's last step): a failure is only a warning,
/// "Move behind the proxy" adds it later.
async fn add_route(a: &Arc<AppsSvc>, ctx: &TaskCtx, who: &str, spec: AppRouteSpec) -> Result<(), String> {
    let Some(w) = a.web.get().cloned() else {
        return Ok(());
    };
    let (who, log) = (who.to_string(), ctx.clone());
    match blocking(move || w.add_app_route(&who, &spec, &|m| log.log(m))).await {
        Ok(Some(url)) => ctx.log(&format!("web access: the app is at {url}")),
        Ok(None) => ctx.log("web access: route added (local names are off: no address yet)"),
        Err(e) => {
            ctx.log(&format!(
                "warning: the web access route could not be added ({e}); retry with POST /api/v1/apps/installed/<stack>/proxy"
            ));
            return Err(e);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// tasks
// ---------------------------------------------------------------------------------------------

/// Creates the folders of a render below their volume: missing parents one by one, each
/// with the folder's owner on its descriptor (not under `--dry-system`), never through a
/// symlink ([`rynas_docker::stacks::create_below`]). Shares are only checked. Returns the
/// folders made, for the record that uninstall reads.
fn make_folders(
    s: &DockerSvc,
    folders: &[FolderPlan],
    log: &dyn Fn(&str),
) -> Result<MadeFolders, String> {
    let mut out = MadeFolders::default();
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
        let Some(label) = volume_of(&|l| s.volume_mounted(l), &f.host_path) else {
            return Err(format!(
                "{} is not below a mounted volume",
                f.host_path
            ));
        };
        let base = s.ready_volume(&label, log)?;
        let owner = (!s.dry).then_some(f.owner);
        let mut made =
            rynas_docker::stacks::create_below(&s.root, &base, &f.host_path, 0o750, owner)?;
        if made.last() == Some(&f.host_path) {
            made.pop();
            out.folders.push(f.host_path.clone());
            if f.nocow {
                nocow_folder(s, &f.host_path, log);
            }
        }
        out.parents.extend(made);
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
    Ok(out)
}

/// No copy-on-write for an app folder the install just made (empty): set on its descriptor,
/// on btrfs only. A failure is logged, not fatal: the app works, only less well.
fn nocow_folder(s: &DockerSvc, host_path: &str, log: &dyn Fn(&str)) {
    if s.dry {
        log(&format!("no copy-on-write for {host_path} (dry system: not changed)"));
        return;
    }
    let r = rynas_docker::safefs::open_dir(&s.root, host_path).and_then(|d| {
        if !rynas_services::nocow::on_btrfs(&d) {
            return Ok(false);
        }
        rynas_services::nocow::set_fd(&d).map(|()| true)
    });
    match r {
        Ok(true) => log(&format!("{host_path}: no copy-on-write (new files are written in place, without checksums)")),
        Ok(false) => {}
        Err(e) => log(&format!("warning: {host_path}: no copy-on-write not set: {e}")),
    }
}

pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

/// `docker compose pull` / `up -d` of a rynas stack (logged only under `--dry-system`).
pub(crate) async fn compose_run(
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

pub(crate) struct InstallJob {
    app: String,
    name: String,
    version: String,
    pub(crate) stack: String,
    layout: Layout,
    request: rynas_apps::InstallRequest,
    bind: String,
    start: bool,
    route: Option<AppRouteSpec>,
    who: String,
    share_grants: Vec<PlannedShareGrant>,
}

pub(crate) async fn install_task(a: &Arc<AppsSvc>, ctx: &TaskCtx, j: InstallJob) -> Result<(), String> {
    let s = &a.docker;
    let dir = j.layout.stack(&j.stack);
    ctx.log(&format!(
        "installing {} {} ({}) as stack {} in {dir}",
        j.name, j.version, j.app, j.stack
    ));
    s.ensure_apps_account(ctx).await?;
    let ids = match (&j.request.run_as, a.apps_ids()) {
        (Some(u), _) => {
            let ids = a.run_as_ids(u)?;
            ctx.log(&format!("the app runs as {u} ({}:{})", ids.0, ids.1));
            ids
        }
        (None, Some(ids)) => ids,
        (None, None) if s.dry => {
            ctx.log(&format!(
                "the {} account does not exist (dry system): using {PLACEHOLDER_ID}:{PLACEHOLDER_ID}",
                rynas_docker::APPS_USER
            ));
            (PLACEHOLDER_ID, PLACEHOLDER_ID)
        }
        (None, None) => {
            return Err(format!(
                "the {} account could not be created",
                rynas_docker::APPS_USER
            ));
        }
    };
    let (cat, _) = a.catalog().await;
    let app = cat
        .get(&j.app)
        .ok_or_else(|| format!("{} left the catalog", j.app))?;
    let provided = a.provided(&j.layout, &j.stack, &j.bind, ids);
    let mut request = j.request.clone();
    if app.manifest.user_photos.is_some() {
        // the staging folder where rynasd mounts the chosen users' Photos folders (none yet:
        // an administrator chooses them on the app's page)
        request.user_photos = Some(crate::photos::prepare_staging(s, app, &j.stack)?);
        ctx.log("Photos folders: none yet (choose the users on the app's page)");
    }
    let r = rynas_apps::render::render(app, &cat.source, &provided, &request)
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
    let (s2, folders, log, n2) = (s.clone(), r.folders.clone(), ctx.clone(), j.stack.clone());
    blocking(move || {
        // a stale record of an earlier stack with the same name never applies
        let _ = std::fs::remove_file(made_path(&s2, &n2));
        let made = make_folders(&s2, &folders, &|m| log.log(m));
        // what was made before an error is recorded too
        let rec = made.clone().unwrap_or_default();
        record_made(&s2, &n2, rec)?;
        made.map(|_| ())
    })
    .await
    .map_err(keep)?;
    // the apps' account into the chosen shared folders, before the app starts reading them
    grant_shares(a, ctx, &j.who, &j.stack, &j.name, ids, &j.share_grants)
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
    if let Some(spec) = j.route {
        // the install itself succeeded: a missing route is only a warning in the log
        let _ = add_route(a, ctx, &j.who, spec).await;
    }
    ctx.progress(100);
    ctx.log(&format!("{} installed", j.name));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// shared folders: the apps' account gets in
// ---------------------------------------------------------------------------------------------

/// Who gets into the shares of an app: the apps' group (`docker-apps`), or the NAS user an
/// app with `run_as` runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GrantSubject {
    Group(String),
    User(String),
}

impl GrantSubject {
    /// The subject of an install request: its `run_as` user, else the apps' group.
    pub(crate) fn of(run_as: Option<&str>) -> Self {
        match run_as {
            Some(u) => Self::User(u.to_string()),
            None => Self::Group(rynas_docker::APPS_USER.to_string()),
        }
    }

    fn kind_name(&self) -> (rynas_api_types::PrincipalKind, &str) {
        match self {
            Self::Group(n) => (rynas_api_types::PrincipalKind::Group, n),
            Self::User(n) => (rynas_api_types::PrincipalKind::User, n),
        }
    }

    fn name(&self) -> &str {
        self.kind_name().1
    }
}

/// What `subject` needs on `path`, a folder an app is installed with: from the rynas share
/// with that folder and its permission matrix. A matrix that names nobody leaves the
/// folder's permissions alone (adding an entry would shut everybody else out), so only the
/// files get it then; an explicit deny is respected (for a group, also a deny of the user
/// with the same name).
pub(crate) fn share_grant_action(shares: &[rynas_api_types::Share], path: &str, subject: &GrantSubject, write: bool) -> (Option<String>, ShareGrantAction) {
    use rynas_api_types::{Access, PrincipalKind};
    let norm = |p: &str| p.trim_end_matches('/').to_string();
    let Some(s) = shares.iter().find(|s| norm(&s.path) == norm(path)) else {
        return (None, ShareGrantAction::Files);
    };
    let name = Some(s.name.clone());
    if s.permissions.is_empty() {
        return (name, ShareGrantAction::Files);
    }
    let (kind, who) = subject.kind_name();
    let entry = s.permissions.iter().find(|p| p.kind == kind && p.name == who);
    let user_deny = s.permissions.iter().any(|p| p.kind == PrincipalKind::User && p.name == who && p.access == Access::Deny);
    let action = match entry.map(|p| p.access) {
        _ if user_deny => ShareGrantAction::Denied,
        Some(Access::Deny) => ShareGrantAction::Denied,
        Some(Access::Rw) => ShareGrantAction::Ok,
        Some(Access::Ro) if !write => ShareGrantAction::Ok,
        Some(Access::Ro) => ShareGrantAction::Raise,
        None => ShareGrantAction::Add,
    };
    (name, action)
}

/// The share document with `subject` let into the share at `path` (add or raise, as
/// [`share_grant_action`] says now); `None` when nothing changes.
pub(crate) fn with_share_grant(cfg: &rynas_api_types::SharesConfig, path: &str, subject: &GrantSubject, write: bool) -> Option<rynas_api_types::SharesConfig> {
    use rynas_api_types::{Access, Permission};
    let (Some(share), action) = share_grant_action(&cfg.shares, path, subject, write) else {
        return None;
    };
    if !matches!(action, ShareGrantAction::Add | ShareGrantAction::Raise) {
        return None;
    }
    let access = if write { Access::Rw } else { Access::Ro };
    let (kind, who) = subject.kind_name();
    let mut out = cfg.clone();
    let s = out.shares.iter_mut().find(|s| s.name == share)?;
    match s.permissions.iter_mut().find(|p| p.kind == kind && p.name == who) {
        Some(p) => p.access = access,
        None => s.permissions.push(Permission { kind, name: who.to_string(), access }),
    }
    Some(out)
}

/// The entry [`with_share_grant`] adds or raises on `path` for `subject`, as the stack's
/// record keeps it; `None` when nothing changes.
fn added_grant(shares: &[rynas_api_types::Share], path: &str, subject: &GrantSubject, write: bool) -> Option<rynas_api_types::apps::AppAddedGrant> {
    use rynas_api_types::Access;
    let (Some(share), action) = share_grant_action(shares, path, subject, write) else { return None };
    let before = match action {
        ShareGrantAction::Add => None,
        ShareGrantAction::Raise => Some(Access::Ro),
        _ => return None,
    };
    let (kind, name) = subject.kind_name();
    Some(rynas_api_types::apps::AppAddedGrant { share, kind, name: name.to_string(), access: if write { Access::Rw } else { Access::Ro }, before })
}

/// The share document with a recorded entry taken back: removed (the app added it) or set to
/// what it was; `None` when the share or the entry is gone, or the entry changed since.
fn without_grant(cfg: &rynas_api_types::SharesConfig, g: &rynas_api_types::apps::AppAddedGrant) -> Option<rynas_api_types::SharesConfig> {
    let mut out = cfg.clone();
    let s = out.shares.iter_mut().find(|s| s.name == g.share)?;
    let i = s.permissions.iter().position(|p| p.kind == g.kind && p.name == g.name && p.access == g.access)?;
    match g.before {
        None => {
            s.permissions.remove(i);
        }
        Some(b) => s.permissions[i].access = b,
    }
    Some(out)
}

impl AppsSvc {
    /// The shares of the applied configuration (not the staged one).
    pub(crate) fn applied_shares(&self) -> Vec<rynas_api_types::Share> {
        self.docker
            .ops
            .config
            .applied(rynas_services::shares::SHARES_KEY)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_value::<rynas_api_types::SharesConfig>(v).ok())
            .map(|c| c.shares)
            .unwrap_or_default()
    }
}

/// Share set choices → items: each must be a share of `shares` (by name); the item is named
/// after the share and mounted from its path; `follow` defaults to the share being encrypted
/// (its plaintext mount comes and goes while the container runs).
pub(crate) fn resolve_share_set(
    shares: &[rynas_api_types::Share],
    choices: &[ShareSetChoice],
) -> Result<Vec<rynas_apps::render::ShareSetItem>, String> {
    let mut out: Vec<rynas_apps::render::ShareSetItem> = Vec::new();
    for c in choices {
        let s = shares
            .iter()
            .find(|s| s.name == c.share)
            .ok_or_else(|| format!("no share {}", c.share))?;
        if s.path.is_empty() {
            return Err(format!("share {} has no folder yet", s.name));
        }
        if out.iter().any(|i| i.name == s.name) {
            return Err(format!("share {} is chosen twice", s.name));
        }
        out.push(rynas_apps::render::ShareSetItem {
            name: s.name.clone(),
            path: s.path.trim_end_matches('/').to_string(),
            follow: c.follow.unwrap_or(s.encrypted),
        });
    }
    rynas_apps::render::check_share_set("share_set", &out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// The plan's lines for the shared folders of `request` (fixed shares and share set items).
fn plan_share_grants(a: &AppsSvc, m: &rynas_apps::manifest::AppManifest, request: &rynas_apps::InstallRequest) -> Vec<PlannedShareGrant> {
    let shares = a.applied_shares();
    let subject = GrantSubject::of(request.run_as.as_deref());
    let mut out: Vec<PlannedShareGrant> = m
        .shares
        .iter()
        .filter_map(|s| {
            let path = request.shares.get(&s.env)?;
            let write = s.access == rynas_apps::manifest::Access::Rw;
            let (share, action) = share_grant_action(&shares, path, &subject, write);
            Some(PlannedShareGrant { env: s.env.clone(), path: path.clone(), share, write, action, user: request.run_as.clone() })
        })
        .collect();
    out.extend(share_set_grants(&shares, m, &request.share_set, &subject));
    out
}

/// The share grants of share set items (folders of the stack excluded) for `subject`.
fn share_set_grants(
    shares: &[rynas_api_types::Share],
    m: &rynas_apps::manifest::AppManifest,
    items: &[rynas_apps::render::ShareSetItem],
    subject: &GrantSubject,
) -> Vec<PlannedShareGrant> {
    let Some(ss) = &m.share_set else { return Vec::new() };
    let write = ss.access == rynas_apps::manifest::Access::Rw;
    let user = match subject {
        GrantSubject::User(u) => Some(u.clone()),
        GrantSubject::Group(_) => None,
    };
    items
        .iter()
        .filter(|it| it.path.starts_with('/'))
        .map(|it| {
            let (share, action) = share_grant_action(shares, &it.path, subject, write);
            PlannedShareGrant { env: format!("{}/{}", ss.env, it.name), path: it.path.clone(), share, write, action, user: user.clone() }
        })
        .collect()
}

/// Lets the apps' account into the shared folders of the install: the share's permissions
/// first (an apply of `shares`, which also writes the folder's own ACL), then the files and
/// folders already there ([`rynas_services::idremap::grant`]). Trouble with single files is
/// logged; the app is installed anyway.
/// `ids` are the app's (uid, gid): the gid is the subject of the file ACLs for the apps'
/// group, the uid for a grant to the NAS user the app runs as ([`PlannedShareGrant::user`]).
/// Every entry added or raised is recorded for the stack (`MadeFolders::grants`).
async fn grant_shares(a: &Arc<AppsSvc>, ctx: &TaskCtx, who: &str, stack: &str, app: &str, ids: (u32, u32), grants: &[PlannedShareGrant]) -> Result<(), String> {
    for g in grants {
        let subject = GrantSubject::of(g.user.as_deref());
        let sname = subject.name().to_string();
        if g.action == ShareGrantAction::Denied {
            ctx.log(&format!("warning: {} denies {sname}: the app cannot use {}", g.share.as_deref().unwrap_or(&g.path), g.path));
            continue;
        }
        if let Some(added) = added_grant(&a.applied_shares(), &g.path, &subject, g.write) {
            ctx.log(&format!(
                "the share {} gets a permission entry for {sname}: it also opens the share to {sname} over SMB and NFS",
                added.share
            ));
            if let Err(e) = record_made(&a.docker, stack, MadeFolders { grants: vec![added], ..Default::default() }) {
                ctx.log(&format!("warning: the permission entry is not recorded: {e}"));
            }
        }
        let (cfg, log, path, write, who2, sub2) = (a.docker.ops.config.clone(), ctx.clone(), g.path.clone(), g.write, who.to_string(), subject.clone());
        let what = format!(
            "{sname} gets {} access to {} for {app}",
            if g.write { "read and write" } else { "read" },
            g.share.as_deref().unwrap_or(&g.path)
        );
        blocking(move || {
            cfg.apply_update(&who2, rynas_services::shares::SHARES_KEY, &what, &|l| log.log(l), |cur| {
                let Some(cur) = cur else { return Ok(None) };
                let doc: rynas_api_types::SharesConfig = serde_json::from_value(cur).map_err(|e| e.to_string())?;
                with_share_grant(&doc, &path, &sub2, write)
                    .map(|d| serde_json::to_value(d).map_err(|e| e.to_string()))
                    .transpose()
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .await?;
        let (dry, root, path, log, write) = (a.docker.dry, a.docker.root.clone(), g.path.clone(), ctx.clone(), g.write);
        let (id, user) = match subject {
            GrantSubject::User(_) => (ids.0, true),
            GrantSubject::Group(_) => (ids.1, false),
        };
        let cancel = ctx.clone();
        blocking(move || {
            use rynas_services::idremap::{self, DryRemap, GroupGrant, HostRemap, RemapFs};
            let fs: &dyn RemapFs = if dry { &DryRemap } else { &HostRemap };
            let real = rynas_docker::under(&root, &path);
            let c = idremap::grant(fs, &[real], GroupGrant { gid: id, user, write, owner: None }, &|| cancel.is_cancelled());
            log.log(&format!(
                "{path}: {} access for {sname} on {} of {} entries",
                if write { "read and write" } else { "read" },
                c.acl_entries,
                c.scanned
            ));
            if !c.readonly.is_empty() {
                log.log(&format!("read-only snapshots left as they are: {}", c.readonly.join(", ")));
            }
            if !c.other_fs.is_empty() {
                log.log(&format!("other file systems inside, not entered: {}", c.other_fs.join(", ")));
            }
            if c.errors > 0 {
                log.log(&format!("warning: {} entries could not be changed: {}", c.errors, c.error_samples.join("; ")));
            }
            Ok::<(), String>(())
        })
        .await?;
    }
    Ok(())
}

pub(crate) struct UpdateJob {
    /// The entry id (`x-rynas.app`).
    pub(crate) app: String,
    pub(crate) stack: String,
    pub(crate) layout: Layout,
    pub(crate) old: (String, String),
    new: (String, String),
    folders: Vec<FolderPlan>,
    snapshot: bool,
    stop_first: bool,
    from: String,
    to: String,
}

pub(crate) async fn write_stack(
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
        let owner = s2.ensure_apps_account_blocking(&|m| tracing::info!("apps: {m}"))?;
        s2.fs(owner)
            .write_files(&l2, &n2, &c, &e)
            .map_err(|e| e.to_string())
    })
    .await
}

/// Runs an app update; `Ok` carries the snapshot taken first (the automatic update rolls
/// back to it when the new version is not healthy).
pub(crate) async fn update_task(
    s: &Arc<DockerSvc>,
    ctx: &TaskCtx,
    j: UpdateJob,
) -> Result<Option<String>, String> {
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
        let (s2, folders, log, n2) = (s.clone(), j.folders.clone(), ctx.clone(), j.stack.clone());
        blocking(move || {
            let made = make_folders(&s2, &folders, &|m| log.log(m))?;
            record_made(&s2, &n2, made)
        })
        .await?;
        compose_run(s, ctx, &j.stack, &j.layout, ComposeAction::Pull).await?;
        ctx.progress(70);
        compose_run(s, ctx, &j.stack, &j.layout, ComposeAction::Up).await
    }
    .await;
    match (r, snap) {
        (Ok(()), sn) => {
            // the one per-app step of an update: Syncthing 1.x → 2.x moves its folders from
            // /data to /shares (warnings only, never fails the update)
            if crate::syncthing::needs_folder_move(&j.app, &j.from, &j.to) {
                crate::syncthing::move_data_folders(s, ctx, &j.stack, &j.layout, &j.new.1).await;
            }
            ctx.progress(100);
            ctx.log(&format!("{} updated to {}", j.stack, j.to));
            Ok(sn.map(|s| s.id))
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
            HeaderValue::from_static("default-src 'none'; img-src data:; style-src 'unsafe-inline'; sandbox"),
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
        .ok_or_else(|| Error::not_found(format!("no app {id} in the catalog")))?;
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
        .ok_or_else(|| Error::not_found(format!("no app {id} in the catalog")))?;
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
    let (job, mut started) = install_job(&a, &id, &req, &who).await?;
    let stack = job.stack.clone();
    let guard: OwnedBusy = mark_busy(&a.docker, &stack)?;
    audit(&who, "install", &format!("{id} as {stack}"));
    let a2 = a.clone();
    started.task_id = a
        .docker
        .ops
        .tasks
        .spawn("apps.install", &who, move |ctx| async move {
            let _guard = guard;
            install_task(&a2, &ctx, job).await
        })?;
    Ok((StatusCode::ACCEPTED, Json(started)).into_response())
}

/// An install checked and rendered, with the plan's refusals as API errors: the job of the
/// `apps.install` task and its answer (without the task id).
pub(crate) async fn install_job(a: &AppsSvc, id: &str, req: &AppInstallRequest, who: &str) -> Res<(InstallJob, AppInstallStarted)> {
    install_job_freeing(a, id, req, who, &[]).await
}

/// [`install_job`] where the host ports in `freed` do not count as conflicts: an instance
/// the caller turns off before the app starts holds them (an adoption).
pub(crate) async fn install_job_freeing(
    a: &AppsSvc,
    id: &str,
    req: &AppInstallRequest,
    who: &str,
    freed: &[(u16, rynas_api_types::apps::PortProtocol)],
) -> Res<(InstallJob, AppInstallStarted)> {
    let id = id.to_string();
    let who = who.to_string();
    let p = a.prepare(&id, req).await?;
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
    if let Some(c) = p.plan.port_conflicts.iter().find(|c| !freed.contains(&(c.port, c.protocol))) {
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
        .ok_or_else(|| Error::not_found(format!("no app {id} in the catalog")))?;
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
    let job = InstallJob {
        app: id.clone(),
        name: m.name.clone(),
        version: m.version.to_string(),
        stack: stack.clone(),
        layout: layout.clone(),
        request,
        bind: p.bind,
        start: req.start,
        route: p.route,
        who: who.clone(),
        share_grants: p.plan.share_grants.clone(),
    };
    let started = AppInstallStarted {
        task_id: String::new(),
        stack_dir: layout.stack(&stack),
        stack,
        secrets: shown,
    };
    Ok((job, started))
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
    let (job, plan) = a.prepare_update_job(&stack, &req).await?;
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
            update_task(&s2, &ctx, job).await?;
            // the previous images go once nothing needs them (the snapshot taken before the
            // update keeps the old ones for a rollback)
            drop(_guard);
            if let Err(e) = crate::docker::prune_images(&s2, &ctx).await {
                ctx.log(&format!("unused images not removed: {e}"));
            }
            Ok(())
        })?;
    Ok(accepted(id))
}

/// Most apps one "Update selected" may name.
const UPDATE_MANY_MAX: usize = 100;

/// `POST /v1/apps/update-many` (body [`AppUpdateManyRequest`]): one task `apps.update-many`
/// updates the named apps one after the other, each like `apps.update` with the default
/// snapshot settings, on the server: closing the page changes nothing, the outcome comes
/// as one task. An app whose new version needs answers, or that is up to date by then, is
/// skipped (the task ends partial); a failed one fails the task at the end (the others are
/// still updated). Unused images are removed once, after the last.
async fn update_many_post(
    State(st): State<AppsState>,
    hd: HeaderMap,
    body: axum::body::Bytes,
) -> Res<Response> {
    let a = asvc(&st)?;
    let who = admin(&a, &hd)?;
    let req: AppUpdateManyRequest = optional_body(&body)?;
    let mut stacks = req.stacks;
    stacks.sort();
    stacks.dedup();
    if stacks.is_empty() || stacks.len() > UPDATE_MANY_MAX {
        return Err(Error::invalid("stacks", format!("1 to {UPDATE_MANY_MAX} apps")));
    }
    let stack_list = stacks.join(" ");
    let a2 = a.clone();
    // one at a time: the check and the start are one step (two clicks never start two)
    let spawned = a.docker.ops.tasks.spawn_once("apps.update-many", UPDATE_MANY_SUBJECT, &who, move |ctx| async move {
        let (mut done, mut skipped, mut failed) = (Vec::new(), Vec::new(), Vec::new());
        let n = stacks.len();
        for (i, stack) in stacks.iter().enumerate() {
            if ctx.is_cancelled() {
                return Err("cancelled".into());
            }
            ctx.progress((i * 100 / n) as u8);
            ctx.log(&format!("== {stack} ({} of {n})", i + 1));
            match a2.update_plan(stack).await.map(|(.., p)| update_many_skip(&p)) {
                // updated meanwhile (its own dialog, the automatic update): nothing left to do
                Ok(Some(ManySkip::UpToDate)) => {
                    ctx.log(&format!("{stack}: already up to date"));
                    continue;
                }
                Ok(Some(why)) => {
                    ctx.log(&format!("{stack}: {}", why.text()));
                    skipped.push(stack.clone());
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    ctx.log(&format!("{stack}: {}", e.2));
                    failed.push(stack.clone());
                    continue;
                }
            }
            let r = async {
                let (job, plan) = a2
                    .prepare_update_job(stack, &AppUpdateRequest::default())
                    .await
                    .map_err(|e| e.2)?;
                let _guard = mark_busy(&a2.docker, stack).map_err(|e| e.2)?;
                ctx.log(&format!("{stack}: {} -> {}", plan.from_version, plan.to_version));
                update_task(&a2.docker, &ctx, job).await.map(|_| ())
            }
            .await;
            match r {
                Ok(()) => done.push(stack.clone()),
                Err(e) => {
                    ctx.log(&format!("{stack}: {e}"));
                    failed.push(stack.clone());
                }
            }
        }
        ctx.progress(100);
        if !done.is_empty()
            && let Err(e) = crate::docker::prune_images(&a2.docker, &ctx).await
        {
            ctx.log(&format!("unused images not removed: {e}"));
        }
        ctx.log(&format!(
            "updated: {}; skipped: {}; failed: {}",
            list_or_none(&done),
            list_or_none(&skipped),
            list_or_none(&failed)
        ));
        if let Some(why) = update_many_outcome(&skipped, &failed)? {
            ctx.partial(why);
        }
        Ok(())
    })?;
    match spawned {
        rynas_tasks::Spawned::New(id) => {
            audit(&who, "update-many", &stack_list);
            Ok(accepted(id))
        }
        rynas_tasks::Spawned::Running(id) => Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::BUSY,
            format!("an update of several apps is running ({id})"),
        )),
    }
}

/// The subject of every `apps.update-many`: one at a time on the NAS.
const UPDATE_MANY_SUBJECT: &str = "installed";

/// Why "Update selected" leaves an app alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManySkip {
    /// Nothing to do (updated meanwhile): not a skip, the task stays a full success.
    UpToDate,
    /// The catalogue has an older version.
    Downgrade,
    /// The new version asks for values: its own dialog.
    NeedsAnswers,
}

impl ManySkip {
    fn text(self) -> &'static str {
        match self {
            ManySkip::UpToDate => "already up to date",
            ManySkip::Downgrade => "the catalog has an older version: not updated",
            ManySkip::NeedsAnswers => "the new version needs answers: update it on its own",
        }
    }
}

fn update_many_skip(p: &AppUpdatePlan) -> Option<ManySkip> {
    if p.up_to_date {
        Some(ManySkip::UpToDate)
    } else if p.downgrade {
        Some(ManySkip::Downgrade)
    } else if !p.missing_values.is_empty() {
        Some(ManySkip::NeedsAnswers)
    } else {
        None
    }
}

/// The end of an `apps.update-many`: failed apps fail it, skipped ones make it partial
/// (`Ok(Some(why))`); apps that were already up to date count as done.
fn update_many_outcome(skipped: &[String], failed: &[String]) -> Result<Option<String>, String> {
    if !failed.is_empty() {
        return Err(format!("not updated: {}", failed.join(", ")));
    }
    Ok((!skipped.is_empty()).then(|| format!("skipped: {}", skipped.join(", "))))
}

fn list_or_none(v: &[String]) -> String {
    if v.is_empty() { "none".into() } else { v.join(", ") }
}

impl AppsSvc {
    /// The update of an installed app, checked and rendered (values migrated, secrets kept):
    /// what `apps.update` runs. 409 when up to date or for a downgrade; 400 for a missing
    /// answer.
    pub(crate) async fn prepare_update_job(
        &self,
        stack: &str,
        req: &AppUpdateRequest,
    ) -> Res<(UpdateJob, AppUpdatePlan)> {
        // the account before the plan: its ids go into the rendered files (never `nobody`)
        let d2 = self.docker.clone();
        blocking(move || {
            d2.ensure_apps_account_blocking(&|m| tracing::info!("apps: {m}"))
                .map(|_| ())
        })
        .await
        .map_err(Error::internal)?;
        let (cat, l, d, plan) = self.update_plan(stack).await?;
        if plan.downgrade {
            return Err(conflict(format!(
                "the catalog has {} {}, older than the installed {}",
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
        let ids = self.stack_ids(&env)?;
        let bind = env
            .get("RYNAS_BIND")
            .cloned()
            .unwrap_or_else(|| "127.0.0.1".into());
        let provided = self.provided(&l, stack, &bind, ids);
        let r = rynas_apps::plan::prepare_update(
            app,
            &cat.source,
            &provided,
            stack,
            &d.compose,
            &d.env,
            &req.answers,
        )
        .map_err(render_err)?;
        let set = self.docker.settings();
        let job = UpdateJob {
            app: d.meta.app.clone(),
            stack: stack.to_string(),
            layout: l,
            old: (d.compose.clone(), d.env.clone()),
            new: (file_text(&r, COMPOSE_FILE), file_text(&r, ".env")),
            folders: r.folders.clone(),
            snapshot: req.snapshot.unwrap_or(set.snapshot_before_update),
            stop_first: req.stop_first.unwrap_or(set.stop_for_snapshot),
            from: plan.from_version.clone(),
            to: plan.to_version.clone(),
        };
        Ok((job, plan))
    }

    /// Installed apps whose catalogue entry is newer, with what the automatic update needs
    /// to judge them: the plan, the entry's `update_policy` and its main service.
    pub(crate) async fn auto_candidates(&self) -> Vec<AppCandidate> {
        let (cat, _) = self.catalog().await;
        let mut out = Vec::new();
        for d in self.on_disk() {
            let Some(app) = cat.get(&d.meta.app) else {
                continue;
            };
            let (policy, main_service) = (app.manifest.update_policy, app.manifest.main_service.clone());
            let Ok(p) = self.update_plan(&d.stack).await.map(|r| r.3) else {
                continue;
            };
            if p.up_to_date || p.downgrade {
                continue;
            }
            out.push(AppCandidate {
                stack: d.stack.clone(),
                policy,
                main_service,
                plan: p,
            });
        }
        out
    }
}

/// An installed app with a newer catalogue entry ([`AppsSvc::auto_candidates`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct AppCandidate {
    pub stack: String,
    pub policy: Option<rynas_api_types::docker::UpdatePolicy>,
    pub main_service: String,
    pub plan: AppUpdatePlan,
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
    let (l, _) = a.one_on_disk(&stack)?;
    // folders the install made outside the stack folder (an SSD volume) go with the data:
    // only those rynasd recorded itself, never paths read from the stack's .env
    let (outside, empties, kept) = uninstall_targets(
        &read_made(&a.docker, &stack),
        &l.stack(&stack),
        &|lb| a.docker.volume_mounted(lb),
        &a.docker.mounts(),
    );
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
    let (web, who2, who3) = (a.web.get().cloned(), who.clone(), who.clone());
    let id = a.docker.ops.tasks.spawn("apps.uninstall", &who, move |ctx| async move {
        let _guard = guard;
        let s = &s2;
        ctx.log(&format!("stopping and removing the containers of {n2}"));
        compose_run(s, &ctx, &n2, &l, ComposeAction::Down).await?;
        // the users' Photos folders mounted for the app (never into a folder being deleted)
        crate::photos::release(s, &ctx, &n2).await;
        if let Some(w) = web {
            let (n3, log) = (n2.clone(), ctx.clone());
            match blocking(move || w.remove_stack_routes(&who2, &n3, &|m| log.log(m))).await {
                Ok(true) => ctx.log("web access: the route of the app was removed"),
                Ok(false) => {}
                Err(e) => ctx.log(&format!("warning: the web access route was not removed: {e}")),
            }
        }
        if req.remove_grants {
            remove_grants(s, &ctx, &who3, &n2).await;
        }
        if !req.delete_data {
            ctx.log(&format!(
                "the stack folder {} stays (compose.yaml, .env, data): start it again with POST /api/v1/docker/stacks/{n2}/up, or delete it with POST /api/v1/apps/installed/{n2}/uninstall {{\"delete_data\": true, \"confirm\": \"{n2}\"}}",
                l.stack(&n2)
            ));
            return Ok(());
        }
        for (p, why) in &kept {
            ctx.log(&format!("kept {p}: {why}"));
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
                // never through a symlink: a link in its place is removed, not followed
                match std::fs::symlink_metadata(&real) {
                    // made without a link on the way; one put there since would lead the
                    // removal elsewhere
                    Ok(m) if m.is_dir() && rynas_docker::safefs::open_dir(&s3.root, p).is_err() => {
                        log.log(&format!("kept {p}: a symbolic link on the way to it"));
                    }
                    Ok(m) if m.is_dir() => {
                        log.log(&format!("removing {p}"));
                        std::fs::remove_dir_all(&real).map_err(|e| format!("rm {p}: {e}"))?;
                    }
                    Ok(_) => log.log(&format!("kept {p}: not a folder any more")),
                    Err(_) => {}
                }
            }
            for p in &empties {
                // only when empty (other apps may have put folders there since)
                let _ = std::fs::remove_dir(rynas_docker::under(&s3.root, p));
            }
            s3.fs(None).remove_stack_dir(&l3, &n3, &|m| log.log(m))?;
            let _ = std::fs::remove_file(made_path(&s3, &n3));
            Ok(())
        })
        .await?;
        ctx.log(&format!("{n2} and its data deleted"));
        Ok(())
    })?;
    Ok(accepted(id))
}

/// Takes back the share permission entries the stack's record lists (an uninstall with
/// `remove_grants`): each one still as the app left it is removed or set back; the record
/// keeps none of them afterwards.
async fn remove_grants(s: &Arc<DockerSvc>, ctx: &TaskCtx, who: &str, stack: &str) {
    let mut made = read_made(s, stack);
    if made.grants.is_empty() {
        ctx.log("no share permission entry was added by the app");
        return;
    }
    for g in std::mem::take(&mut made.grants) {
        let (cfg, log, who2, g2) = (s.ops.config.clone(), ctx.clone(), who.to_string(), g.clone());
        let what = format!("the permission entry of {} on {} added for the app {stack} is removed", g.name, g.share);
        let r = blocking(move || {
            cfg.apply_update(&who2, rynas_services::shares::SHARES_KEY, &what, &|l| log.log(l), |cur| {
                let Some(cur) = cur else { return Ok(None) };
                let doc: rynas_api_types::SharesConfig = serde_json::from_value(cur).map_err(|e| e.to_string())?;
                without_grant(&doc, &g2).map(|d| serde_json::to_value(d).map_err(|e| e.to_string())).transpose()
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .await;
        match r {
            Ok(()) => ctx.log(&format!("{}: the entry of {} is back as before the app (or changed since: left as it is)", g.share, g.name)),
            Err(e) => ctx.log(&format!("warning: {}: the entry of {} not taken back: {e}", g.share, g.name)),
        }
    }
    let p = made_path(s, stack);
    if let Ok(text) = serde_json::to_vec_pretty(&made) {
        let _ = crate::tls::write_atomic(&p, &text, 0o600, None);
    }
}

/// "Move behind the proxy": loopback binding (`RYNAS_BIND=127.0.0.1`, then `up`) and the
/// app's web access route. Host-network apps only get the route.
async fn proxy_post(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(stack): UrlPath<String>,
) -> Res<Response> {
    let a = asvc(&st)?;
    let who = admin(&a, &hd)?;
    let Some(web) = a.web.get().cloned().filter(|w| w.proxy_active()) else {
        return Err(conflict(
            "web access is not active (rynas-caddy not installed, or external proxy mode)",
        ));
    };
    let (cat, _) = a.catalog().await;
    let (l, d) = a.one_on_disk(&stack)?;
    let app = cat.get(&d.meta.app).ok_or_else(|| {
        Error::not_found(format!(
            "the app {} of stack {stack} is no longer in the catalog",
            d.meta.app
        ))
    })?;
    let m = &app.manifest;
    let env = rynas_apps::render::parse_env(&d.env);
    let ports: BTreeMap<String, u16> = m
        .ports
        .iter()
        .filter_map(|p| Some((p.env.clone(), env.get(&p.env)?.parse().ok()?)))
        .collect();
    let Some(spec) = route_spec(m, &stack, &ports) else {
        return Err(conflict(format!("{} has no web interface", m.name)));
    };
    let rebind = !m.host_network && env.get("RYNAS_BIND").map(String::as_str) != Some("127.0.0.1");
    if !rebind && web.stack_url(&stack).is_some() {
        return Err(conflict(format!("{stack} is already behind the proxy")));
    }
    let guard = mark_busy(&a.docker, &stack)?;
    audit(&who, "proxy", &stack);
    let (a2, n2) = (a.clone(), stack.clone());
    let old = (d.compose.clone(), d.env.clone());
    let new = (d.compose, set_env_line(&d.env, "RYNAS_BIND", "127.0.0.1"));
    let id = a.docker.ops.tasks.spawn("apps.proxy", &who.clone(), move |ctx| async move {
        let _guard = guard;
        let s = &a2.docker;
        if rebind {
            ctx.log("binding the web port to 127.0.0.1 (only the proxy reaches it)");
            write_stack(s, &l, &n2, &new).await?;
            if let Err(e) = compose_run(s, &ctx, &n2, &l, ComposeAction::Up).await {
                ctx.log(&format!("{e}: putting the previous .env back"));
                write_stack(s, &l, &n2, &old).await?;
                compose_run(s, &ctx, &n2, &l, ComposeAction::Up).await?;
                return Err(e);
            }
            s.changed();
        }
        add_route(&a2, &ctx, &who, spec).await
    })?;
    Ok(accepted(id))
}

/// A new share set of an installed app, rendered: what `apps.share-set` writes.
pub(crate) struct ShareSetJob {
    pub(crate) stack: String,
    pub(crate) layout: Layout,
    name: String,
    old: (String, String),
    new: (String, String),
    folders: Vec<FolderPlan>,
    grants: Vec<PlannedShareGrant>,
    /// The app's (uid, gid).
    ids: (u32, u32),
    /// The items now mounted.
    pub(crate) items: Vec<rynas_apps::render::ShareSetItem>,
    /// Items mounted before that the change removes (names).
    removed: Vec<String>,
}

impl AppsSvc {
    /// Renders an installed app with another share set (everything else as stored). 409 for
    /// an app without a share set, or whose installed version differs from the catalogue's
    /// (update it first: the render uses the catalogue entry).
    pub(crate) async fn prepare_share_set(&self, stack: &str, items: Vec<rynas_apps::render::ShareSetItem>) -> Res<ShareSetJob> {
        let (cat, _) = self.catalog().await;
        let (l, d) = self.one_on_disk(stack)?;
        let app = cat.get(&d.meta.app).ok_or_else(|| {
            Error::not_found(format!("the app {} of stack {stack} is no longer in the catalog", d.meta.app))
        })?;
        let m = &app.manifest;
        let Some(ss) = &m.share_set else {
            return Err(conflict(format!("{} has no share set", m.name)));
        };
        if d.meta.version != m.version {
            return Err(conflict(format!(
                "{stack} is {} {} and the catalog has {}: update it first",
                m.name, d.meta.version, m.version
            )));
        }
        rynas_apps::render::check_share_set(&ss.env, &items).map_err(render_err)?;
        let env = rynas_apps::render::parse_env(&d.env);
        let ids = self.stack_ids(&env)?;
        let bind = env.get("RYNAS_BIND").cloned().unwrap_or_else(|| "127.0.0.1".into());
        let provided = self.provided(&l, stack, &bind, ids);
        let mut req = rynas_apps::render::request_from_env(app, stack, &env);
        req.share_set = items.clone();
        let r = rynas_apps::render::render(app, &cat.source, &provided, &req).map_err(render_err)?;
        let subject = GrantSubject::of(req.run_as.as_deref());
        let grants = share_set_grants(&self.applied_shares(), m, &items, &subject);
        let removed = env
            .get(&ss.env)
            .and_then(|v| rynas_apps::render::decode_share_set(v).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|i| !items.iter().any(|n| n.name == i.name))
            .map(|i| i.name)
            .collect();
        Ok(ShareSetJob {
            removed,
            stack: stack.to_string(),
            layout: l,
            name: m.name.clone(),
            old: (d.compose.clone(), d.env.clone()),
            new: (file_text(&r, COMPOSE_FILE), file_text(&r, ".env")),
            folders: r.folders.clone(),
            grants,
            ids,
            items,
        })
    }
}

/// Writes the new share set: files, folders of the stack, the apps' account into the shares,
/// `up` (the container is recreated with the new binds). On a failure the previous files
/// come back and the stack goes up as it was.
pub(crate) async fn share_set_task(a: &Arc<AppsSvc>, ctx: &TaskCtx, who: &str, j: ShareSetJob) -> Result<(), String> {
    let s = &a.docker;
    let names: Vec<&str> = j.items.iter().map(|i| i.name.as_str()).collect();
    ctx.log(&format!(
        "{}: shared folders {}",
        j.stack,
        if names.is_empty() { "none".to_string() } else { names.join(", ") }
    ));
    let r = async {
        write_stack(s, &j.layout, &j.stack, &j.new).await?;
        let (s2, folders, log, n2) = (s.clone(), j.folders.clone(), ctx.clone(), j.stack.clone());
        blocking(move || {
            // added to what the install made
            let made = make_folders(&s2, &folders, &|m| log.log(m))?;
            record_made(&s2, &n2, made)
        })
        .await?;
        grant_shares(a, ctx, who, &j.stack, &j.name, j.ids, &j.grants).await?;
        ctx.progress(60);
        compose_run(s, ctx, &j.stack, &j.layout, ComposeAction::Up).await
    }
    .await;
    match r {
        Ok(()) => {
            s.changed();
            ctx.progress(100);
            Ok(())
        }
        Err(e) => {
            ctx.log(&format!("{e}: putting the previous compose.yaml and .env back"));
            write_stack(s, &j.layout, &j.stack, &j.old).await?;
            compose_run(s, ctx, &j.stack, &j.layout, ComposeAction::Up).await?;
            Err(e)
        }
    }
}

/// `PUT /v1/apps/installed/{stack}/share-set`: task `apps.share-set`.
async fn share_set_put(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(stack): UrlPath<String>,
    body: Result<Json<ShareSetRequest>, JsonRejection>,
) -> Res<Response> {
    let a = asvc(&st)?;
    let who = admin(&a, &hd)?;
    let Json(req) = body?;
    let items = resolve_share_set(&a.applied_shares(), &req.items).map_err(|e| Error::invalid("items", e))?;
    let job = a.prepare_share_set(&stack, items).await?;
    let guard = mark_busy(&a.docker, &stack)?;
    let names: Vec<&str> = job.items.iter().map(|i| i.name.as_str()).collect();
    audit(&who, "share-set", &format!("{stack} [{}]", names.join(", ")));
    let a2 = a.clone();
    let id = a.docker.ops.tasks.spawn("apps.share-set", &who.clone(), move |ctx| async move {
        let _guard = guard;
        share_set_task(&a2, &ctx, &who, job).await
    })?;
    Ok(accepted(id))
}

/// `POST /v1/apps/installed/{stack}/share-set/plan`: what a `PUT` with this body does (the
/// items, the ones removed, the permission entries added or raised). No side effect.
async fn share_set_plan_post(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(stack): UrlPath<String>,
    body: Result<Json<ShareSetRequest>, JsonRejection>,
) -> Res<Json<rynas_api_types::apps::ShareSetPlan>> {
    let a = asvc(&st)?;
    admin(&a, &hd)?;
    let Json(req) = body?;
    let items = resolve_share_set(&a.applied_shares(), &req.items).map_err(|e| Error::invalid("items", e))?;
    let job = a.prepare_share_set(&stack, items).await?;
    Ok(Json(rynas_api_types::apps::ShareSetPlan {
        items: job.items.iter().map(|i| InstalledShareSetItem { name: i.name.clone(), path: i.path.clone(), follow: i.follow }).collect(),
        removed: job.removed.clone(),
        grants: job.grants.iter().filter(|g| matches!(g.action, ShareGrantAction::Add | ShareGrantAction::Raise)).cloned().collect(),
    }))
}

async fn catalog_source_get(
    State(st): State<AppsState>,
    hd: HeaderMap,
) -> Res<Json<CatalogSourceState>> {
    let a = asvc(&st)?;
    admin(&a, &hd)?;
    Ok(Json(a.catalog_source().await))
}

/// "Check now": task `apps.catalog-update`.
async fn catalog_check_post(State(st): State<AppsState>, hd: HeaderMap) -> Res<Response> {
    let a = asvc(&st)?;
    let who = admin(&a, &hd)?;
    if let Some(reason) = a.online_use().reason {
        return Err(conflict(match reason {
            "off" => "the online catalog is turned off in the Docker settings",
            _ => "this build has no catalog signing key: the online catalog is off",
        }));
    }
    audit(&who, "catalog-update", "online catalog");
    let a2 = a.clone();
    let id = a
        .docker
        .ops
        .tasks
        .spawn("apps.catalog-update", &who, move |ctx| async move {
            let log = |l: &str| ctx.log(l);
            match a2.check_catalog(&log).await {
                Ok(online::Outcome::Updated { serial, .. }) => {
                    ctx.log(&format!("the catalog is now serial {serial}"));
                    Ok(())
                }
                Ok(online::Outcome::Unchanged(s)) => {
                    ctx.log(&format!("no newer catalog (serial {s})"));
                    Ok(())
                }
                Ok(online::Outcome::NotModified) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        })?;
    Ok(accepted(id))
}

pub fn router(st: AppsState) -> Router {
    Router::new()
        .route("/v1/apps/catalog", get(catalog_get))
        .route("/v1/apps/catalog-source", get(catalog_source_get))
        .route("/v1/apps/catalog-source/check", post(catalog_check_post))
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
        .route("/v1/apps/update-many", post(update_many_post))
        .route("/v1/apps/installed/{stack}/rollback", post(rollback_post))
        .route("/v1/apps/installed/{stack}/convert", post(convert_post))
        .route("/v1/apps/installed/{stack}/uninstall", post(uninstall_post))
        .route("/v1/apps/installed/{stack}/proxy", post(proxy_post))
        .route("/v1/apps/installed/{stack}/share-set", put(share_set_put))
        .route("/v1/apps/installed/{stack}/share-set/plan", post(share_set_plan_post))
        .with_state(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_many_skips_and_outcome() {
        let plan = |up_to_date: bool, downgrade: bool, missing: &[&str]| AppUpdatePlan {
            up_to_date,
            downgrade,
            missing_values: missing.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        assert_eq!(update_many_skip(&plan(false, false, &[])), None);
        assert_eq!(update_many_skip(&plan(true, false, &[])), Some(ManySkip::UpToDate));
        assert_eq!(update_many_skip(&plan(false, true, &[])), Some(ManySkip::Downgrade));
        assert_eq!(update_many_skip(&plan(false, false, &["TZ"])), Some(ManySkip::NeedsAnswers));
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // up to date apps are neither skipped nor failed: a full success
        assert_eq!(update_many_outcome(&[], &[]), Ok(None));
        assert_eq!(update_many_outcome(&v(&["immich"]), &[]), Ok(Some("skipped: immich".into())));
        assert_eq!(update_many_outcome(&v(&["immich"]), &v(&["jellyfin"])), Err("not updated: jellyfin".into()));
    }

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
    fn the_apps_account_gets_into_a_chosen_share() {
        use rynas_api_types::{Access, Permission, PrincipalKind, Share, SharesConfig};
        let p = |kind, name: &str, access| Permission { kind, name: name.into(), access };
        let share = |name: &str, path: &str, perms: Vec<Permission>| Share { name: name.into(), path: path.into(), permissions: perms, ..Default::default() };
        let me = p(PrincipalKind::User, "alice", Access::Rw);
        let shares = vec![
            share("Photos", "/srv/v1/Photos", vec![me.clone()]),
            share("Open", "/srv/v1/Open", vec![]),
            share("Ro", "/srv/v1/Ro", vec![me.clone(), p(PrincipalKind::Group, "docker-apps", Access::Ro)]),
            share("No", "/srv/v1/No", vec![me.clone(), p(PrincipalKind::Group, "docker-apps", Access::Deny)]),
            share("NoUser", "/srv/v1/NoUser", vec![me.clone(), p(PrincipalKind::User, "docker-apps", Access::Deny)]),
        ];
        let g = GrantSubject::Group("docker-apps".into());
        let act = |path, write| share_grant_action(&shares, path, &g, write);
        assert_eq!(act("/srv/v1/Photos/", false), (Some("Photos".into()), ShareGrantAction::Add));
        // permissions left to the folder: adding the group would shut the others out
        assert_eq!(act("/srv/v1/Open", true), (Some("Open".into()), ShareGrantAction::Files));
        assert_eq!(act("/srv/v1/plain", true), (None, ShareGrantAction::Files));
        assert_eq!(act("/srv/v1/Ro", false), (Some("Ro".into()), ShareGrantAction::Ok));
        assert_eq!(act("/srv/v1/Ro", true), (Some("Ro".into()), ShareGrantAction::Raise));
        assert_eq!(act("/srv/v1/No", false).1, ShareGrantAction::Denied);
        assert_eq!(act("/srv/v1/NoUser", false).1, ShareGrantAction::Denied);
        // the document: the entry added or raised, nothing else touched; no change → None
        let cfg = SharesConfig { shares, ..Default::default() };
        let added = with_share_grant(&cfg, "/srv/v1/Photos", &g, false).unwrap();
        assert_eq!(added.shares[0].permissions, vec![me.clone(), p(PrincipalKind::Group, "docker-apps", Access::Ro)]);
        assert_eq!(added.shares[1..], cfg.shares[1..]);
        let raised = with_share_grant(&cfg, "/srv/v1/Ro", &g, true).unwrap();
        assert_eq!(raised.shares[2].permissions[1].access, Access::Rw);
        for (path, write) in [("/srv/v1/Ro", false), ("/srv/v1/No", true), ("/srv/v1/Open", true), ("/srv/v1/plain", false)] {
            assert!(with_share_grant(&cfg, path, &g, write).is_none(), "{path}");
        }
        // what is recorded, and taken back at an uninstall with remove_grants
        let rec_add = added_grant(&cfg.shares, "/srv/v1/Photos", &g, false).unwrap();
        assert_eq!((rec_add.share.as_str(), rec_add.access, rec_add.before), ("Photos", Access::Ro, None));
        let rec_raise = added_grant(&cfg.shares, "/srv/v1/Ro", &g, true).unwrap();
        assert_eq!((rec_raise.access, rec_raise.before), (Access::Rw, Some(Access::Ro)));
        assert!(added_grant(&cfg.shares, "/srv/v1/Open", &g, true).is_none());
        assert_eq!(without_grant(&added, &rec_add).unwrap(), cfg);
        assert_eq!(without_grant(&raised, &rec_raise).unwrap(), cfg);
        // changed since (or gone): left alone
        assert!(without_grant(&cfg, &rec_add).is_none());
        let mut made = MadeFolders { grants: vec![rec_raise.clone()], ..Default::default() };
        made.add(MadeFolders { grants: vec![rynas_api_types::apps::AppAddedGrant { access: Access::Rw, before: Some(Access::Rw), ..rec_raise.clone() }], ..Default::default() });
        assert_eq!(made.grants, [rec_raise], "the first change keeps the entry as it was before the app");
    }

    #[test]
    fn a_run_as_user_gets_into_a_chosen_share() {
        use rynas_api_types::{Access, Permission, PrincipalKind, Share, SharesConfig};
        let p = |kind, name: &str, access| Permission { kind, name: name.into(), access };
        let share = |name: &str, path: &str, perms: Vec<Permission>| Share { name: name.into(), path: path.into(), permissions: perms, ..Default::default() };
        let shares = vec![
            share("Mine", "/srv/v1/Mine", vec![p(PrincipalKind::User, "ann", Access::Rw)]),
            share("Ro", "/srv/v1/Ro", vec![p(PrincipalKind::User, "ann", Access::Ro)]),
            share("Other", "/srv/v1/Other", vec![p(PrincipalKind::User, "bob", Access::Rw), p(PrincipalKind::Group, "ann", Access::Rw)]),
            share("No", "/srv/v1/No", vec![p(PrincipalKind::User, "ann", Access::Deny)]),
        ];
        let ann = GrantSubject::User("ann".into());
        let act = |path| share_grant_action(&shares, path, &ann, true).1;
        assert_eq!(act("/srv/v1/Mine"), ShareGrantAction::Ok);
        assert_eq!(act("/srv/v1/Ro"), ShareGrantAction::Raise);
        // a group named like the user is not the user
        assert_eq!(act("/srv/v1/Other"), ShareGrantAction::Add);
        assert_eq!(act("/srv/v1/No"), ShareGrantAction::Denied);
        let cfg = SharesConfig { shares, ..Default::default() };
        let added = with_share_grant(&cfg, "/srv/v1/Other", &ann, true).unwrap();
        assert_eq!(added.shares[2].permissions.last(), Some(&p(PrincipalKind::User, "ann", Access::Rw)));
        assert_eq!(with_share_grant(&cfg, "/srv/v1/Ro", &ann, true).unwrap().shares[1].permissions[0].access, Access::Rw);
        assert!(with_share_grant(&cfg, "/srv/v1/Mine", &ann, true).is_none());
        assert_eq!(GrantSubject::of(None), GrantSubject::Group(rynas_docker::APPS_USER.into()));
    }

    #[test]
    fn run_as_only_nas_users() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
            daemon:x:1:1::/usr/sbin:/usr/sbin/nologin\n\
            docker-apps:x:1500:1500::/nonexistent:/usr/sbin/nologin\n\
            ann:x:1026:100::/home/ann:/bin/bash\n\
            bob:x:1027:1027::/home/bob:/usr/sbin/nologin\n\
            nobody:x:65534:65534::/nonexistent:/usr/sbin/nologin\n\
            sys:x:999:999::/:/usr/sbin/nologin\n";
        assert_eq!(nas_user_ids(passwd, "ann"), Ok((1026, 100)));
        assert_eq!(nas_user_ids(passwd, "bob"), Ok((1027, 1027)));
        for (u, why) in [("root", "root"), ("docker-apps", "apps' own account"), ("daemon", "system account"), ("sys", "system account"), ("nobody", "system account"), ("zed", "no user")] {
            let e = nas_user_ids(passwd, u).unwrap_err();
            assert!(e.contains(why), "{u}: {e}");
        }
        assert_eq!(nas_users(passwd).into_iter().map(|u| u.0).collect::<Vec<_>>(), ["ann", "bob"]);
    }

    #[test]
    fn masks_secrets_and_checks_volumes() {
        let s: BTreeSet<String> = ["DB_PASSWORD".to_string()].into();
        assert_eq!(
            masked_env("# x\nDB_PASSWORD='a b'\nPUID=1\n", &s),
            format!("# x\nDB_PASSWORD={MASK}\nPUID=1\n")
        );
        let m = |l: &str| l == "vol1";
        assert!(on_volume(&m, "/vol1/media"));
        assert!(!on_volume(&m, "/vol1"));
        assert!(!on_volume(&m, "/vol2/media"));
        assert!(!on_volume(&m, "/vol1/../etc"));
        assert!(!on_volume(&m, "relative/x"));
        // `lib` would pass a plain is_dir test; it is reserved and never mounted
        assert!(!on_volume(&|_| true, "/lib/x"));
        // the system volume's folder is a volume (D-ROOTVOL); /srv itself is not
        let sys = |l: &str| l == "system";
        assert!(on_volume(&sys, "/srv/rynas/system/media"));
        assert!(!on_volume(&sys, "/srv/rynas/system"));
        assert!(!on_volume(&|_| true, "/srv/x"));
        assert!(!on_volume(&sys, "/srv/rynas/systemx/a"));
    }

    /// Review 2026-11-07, H2: uninstall deletes only the folders rynasd recorded as made,
    /// below a mounted volume, never system paths or mount points; paths in the stack's
    /// .env play no part (a tampered `.env` with `/lib/x` or a folder that existed before
    /// the install is never deleted).
    #[test]
    fn uninstall_deletes_only_recorded_folders() {
        let made = MadeFolders {
            folders: vec![
                "/ssd/docker/appdata/app/db".into(),
                "/vol1/docker/stacks/app/config".into(),
                "/lib/x".into(),
                "/vol9/data".into(),
                "/vol1/media".into(),
            ],
            parents: vec![
                "/ssd/docker/appdata".into(),
                "/ssd/docker/appdata/app".into(),
            ],
            ..Default::default()
        };
        let mounted = |l: &str| l == "ssd" || l == "vol1";
        let (trees, empties, kept) = uninstall_targets(
            &made,
            "/vol1/docker/stacks/app",
            &mounted,
            &["/".into(), "/vol1".into(), "/ssd".into(), "/vol1/media/usb".into()],
        );
        assert_eq!(trees, ["/ssd/docker/appdata/app/db"]);
        assert_eq!(empties, ["/ssd/docker/appdata/app", "/ssd/docker/appdata"]);
        let why: BTreeMap<&str, &str> = kept.iter().map(|(p, w)| (p.as_str(), *w)).collect();
        assert_eq!(why["/lib/x"], "a system folder");
        assert_eq!(why["/vol9/data"], "not below a mounted volume");
        assert_eq!(why["/vol1/media"], "holds a mount point");
        // nothing recorded (a pre-existing folder, or an install before the record): nothing
        let (t, e, k) =
            uninstall_targets(&MadeFolders::default(), "/vol1/docker/stacks/app", &mounted, &[]);
        assert!(t.is_empty() && e.is_empty() && k.is_empty());
    }
}
