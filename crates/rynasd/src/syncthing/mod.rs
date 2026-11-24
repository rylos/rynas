//! Syncthing integrated (`/v1/syncthing/...`, public under `/api/v1/syncthing/...`),
//! `docs/phase2/syncthing.md`. Phase S1a: adoption of a Syncthing that already runs (or ran)
//! somewhere into the catalogue app, without pairing the devices again.
//!
//! - `POST /v1/syncthing/adopt/preview`: the instances found (the catalogue app with the older
//!   layout, other containers, host services) and, for the chosen one (or a home folder
//!   given), its files, folders mapped to `/shares/<share>/...`, `.stfolder` markers,
//!   `.stignore` proposals and owners. No side effects.
//! - `POST /v1/syncthing/adopt`: task `syncthing.adopt`, one at a time. Every check that can
//!   fail runs before the old instance is touched; then its restart policy or unit is turned
//!   off for good (two instances with one device id fight), the home copied into the app's
//!   config folder with folder paths rewritten in the copy (folders without a verified marker,
//!   or outside every share, paused there). A later failure puts the app's previous home
//!   back and turns the old instance on again as it was. The old home is never changed. The
//!   older catalogue layout is updated in place: the update itself moves its folders
//!   ([`move_data_folders`], also run by a plain update from 1.x).
//! - Secrets: the app's API key lives in its stack's `.env` (0600, root, an app secret like
//!   every other); the old home's API key, GUI password and keys are copied as files, never
//!   read into a response or a log.

pub mod config;

use std::collections::BTreeSet;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use rynas_api_types::apps::{AppInstallRequest, AppUpdateRequest, ShareSetChoice};
use rynas_api_types::docker::ContainerState;
use rynas_api_types::syncthing::*;
use rynas_apps::render::ShareSetItem;
use rynas_docker::Layout;
use rynas_docker::stacks::ComposeAction;
use rynas_tasks::{Cmd, TaskCtx};

use crate::apps::{AppsState, AppsSvc, blocking, compose_run};
use crate::docker::{DockerSvc, accepted, conflict, mark_busy};
use crate::ops::{Error, Res, user};
use config::{FolderEdit, Mount, StConfig};

/// The catalogue entry adoption installs.
pub const APP_ID: &str = "syncthing";
/// The container folder of the entry's share set.
const TARGET: &str = "/shares";
/// The single share of the older layout inside the container.
const OLD_DATA: &str = "/data";
/// The home inside the official image (`STHOMEDIR`), below the app's config folder.
const HOME_IN_CONFIG: &str = "config";
/// The first entry version with the share set.
const SHARE_SET_VERSION: &str = "2.0.0";
/// Files of a home copied one by one (the database folders apart).
const HOME_FILES: &[&str] = &["cert.pem", "key.pem", "https-cert.pem", "https-key.pem"];
/// Files without which the devices would see a new device id.
const IDENTITY: &[&str] = &["cert.pem", "key.pem"];
const DB_V2: &str = "index-v2";
const DB_V1: &str = "index-v0.14.0.db";
/// How long a task waits for the app's REST interface after a start.
const HEALTH_WAIT: Duration = Duration::from_secs(120);
/// The prefix of preview lines that do not block the adoption.
const INFO: &str = "info: ";

fn audit(who: &str, action: &str, target: &str) {
    tracing::info!(target: "rynas::audit", "syncthing {action} {target} by {who}");
}

fn svc(st: &AppsState) -> Res<Arc<AppsSvc>> {
    st.clone().ok_or_else(|| {
        Error::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            rynas_api_types::error_codes::BACKEND_UNAVAILABLE,
            "the app catalog is not enabled in this rynasd".into(),
        )
    })
}

fn admin(a: &AppsSvc, hd: &HeaderMap) -> Res<String> {
    let who = user(hd)?;
    a.docker.ops.admins.require_admin(&who, "Syncthing")?;
    Ok(who)
}

/// A host path below the configuration root (the real `/`, or the dry root in tests).
fn real_in(root: &Path, p: &str) -> PathBuf {
    rynas_docker::under(root, p)
}

fn real(a: &AppsSvc, p: &str) -> PathBuf {
    real_in(&a.docker.root, p)
}

fn version_lt(v: &rynas_apps::Version, s: &str) -> bool {
    s.parse::<rynas_apps::Version>().is_ok_and(|x| *v < x)
}

fn major(v: &str) -> Option<u64> {
    v.split('.').next()?.parse().ok()
}

/// `./rel` of a stack's `.env` → absolute host path.
fn stack_path(stack_dir: &str, v: &str) -> String {
    match v.strip_prefix("./") {
        Some(rel) => format!("{}/{rel}", stack_dir.trim_end_matches('/')),
        None => v.to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// one adoption at a time
// ---------------------------------------------------------------------------------------------

static ADOPTING: AtomicBool = AtomicBool::new(false);

/// The one adoption slot; freed when dropped (task end, panic, or a refusal).
struct AdoptSlot;

impl Drop for AdoptSlot {
    fn drop(&mut self) {
        ADOPTING.store(false, Ordering::Release);
    }
}

fn take_slot() -> Res<AdoptSlot> {
    if ADOPTING.swap(true, Ordering::AcqRel) {
        return Err(Error::new(
            axum::http::StatusCode::CONFLICT,
            rynas_api_types::error_codes::BUSY,
            "a Syncthing adoption is running".into(),
        ));
    }
    Ok(AdoptSlot)
}

// ---------------------------------------------------------------------------------------------
// sources
// ---------------------------------------------------------------------------------------------

/// A source found or named, with what the preview and the task need about it.
#[derive(Debug, Clone, Default)]
struct Resolved {
    candidate: AdoptCandidate,
    /// Home folder on this host.
    home: Option<String>,
    /// container path → host path (empty: the folder paths are host paths).
    table: Vec<AdoptVolume>,
    /// The old user's home, for `~/` folder paths.
    user_home: Option<String>,
    /// Older catalogue layout: the share set item its single share becomes (name, host).
    app_item: Option<(String, String)>,
    /// systemd unit (`syncthing@ann.service`), and whether it is a user unit.
    unit: Option<(String, bool)>,
    /// A container's restart policy (`no`, `always`, `unless-stopped`, `on-failure`).
    restart_policy: Option<String>,
}

/// The older catalogue layout: installed stacks of the entry below 2.0.0.
fn app_sources(a: &AppsSvc) -> Vec<Resolved> {
    let Some(l) = a.docker.layout() else { return Vec::new() };
    a.on_disk()
        .into_iter()
        .filter(|d| d.meta.app == APP_ID && version_lt(&d.meta.version, SHARE_SET_VERSION))
        .map(|d| {
            let env = rynas_apps::render::parse_env(&d.env);
            let dir = l.stack(&d.stack);
            let mut table = Vec::new();
            let config = env.get("CONFIG_FOLDER").map(|v| stack_path(&dir, v));
            if let Some(c) = &config {
                table.push(AdoptVolume { container: "/var/syncthing".into(), host: c.clone() });
            }
            // the item the migration makes of SYNC_SHARE (its folder's name)
            let app_item = env.get("SYNC_SHARE").and_then(|v| {
                let item = rynas_apps::render::decode_share_set(v).ok()?.into_iter().next()?;
                Some((item.name, stack_path(&dir, &item.path)))
            });
            if let Some((_, host)) = &app_item {
                table.push(AdoptVolume { container: OLD_DATA.into(), host: host.clone() });
            }
            let home = config.map(|c| {
                let inner = format!("{c}/{HOME_IN_CONFIG}");
                if real(a, &format!("{inner}/config.xml")).exists() { inner } else { c }
            });
            Resolved {
                candidate: AdoptCandidate {
                    source: AdoptSourceRef { kind: AdoptSourceKind::App, id: Some(d.stack.clone()), path: None },
                    name: d.stack.clone(),
                    home: home.clone(),
                    running: false,
                    detail: format!("{APP_ID} {}", d.meta.version),
                },
                home,
                table,
                app_item,
                ..Default::default()
            }
        })
        .collect()
}

/// Other containers running Syncthing (the catalogue app's own excluded).
async fn container_sources(a: &AppsSvc) -> Vec<Resolved> {
    let Ok(cs) = a.docker.containers().await else { return Vec::new() };
    let ours: BTreeSet<String> = a.on_disk().into_iter().filter(|d| d.meta.app == APP_ID).map(|d| d.stack).collect();
    let mut out = Vec::new();
    for c in cs {
        if c.c.stack.as_ref().is_some_and(|s| ours.contains(s)) {
            continue;
        }
        let Ok(js) = a.docker.inspect_json(&c.c.id).await else { continue };
        let (image, cmd) = rynas_docker::model::inspect_image_and_command(&js);
        if !config::is_syncthing(&image, &cmd) && !config::is_syncthing(&c.c.image, "") {
            continue;
        }
        let mounts: Vec<Mount> = rynas_docker::model::inspect_mounts(&js)
            .into_iter()
            .map(|m| Mount { source: m.source, destination: m.destination })
            .collect();
        let home = config::home_from_mounts(&mounts, &|p| real(a, p).exists());
        out.push(Resolved {
            candidate: AdoptCandidate {
                source: AdoptSourceRef { kind: AdoptSourceKind::Container, id: Some(c.c.name.clone()), path: None },
                name: c.c.name.clone(),
                home: home.clone(),
                running: c.c.state == ContainerState::Running,
                detail: c.c.image.clone(),
            },
            home,
            table: config::table_from_mounts(&mounts),
            restart_policy: Some(config::restart_policy(&js)),
            ..Default::default()
        });
    }
    out
}

/// A Syncthing process of the host: pid, uid, and the homes it names (empty: the default).
struct Proc {
    pid: String,
    uid: u32,
    homes: Vec<String>,
}

/// The running Syncthing processes (`/proc/<pid>/comm` = `syncthing`).
fn processes(a: &AppsSvc) -> Vec<Proc> {
    let proc = a.docker.host_root.join("proc");
    let nul = |p: PathBuf| -> Vec<String> {
        std::fs::read(p)
            .map(|b| b.split(|c| *c == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect())
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for e in std::fs::read_dir(&proc).into_iter().flatten().flatten() {
        let p = e.path();
        if !std::fs::read_to_string(p.join("comm")).is_ok_and(|c| c.trim() == "syncthing") {
            continue;
        }
        let Ok(m) = std::fs::metadata(&p) else { continue };
        out.push(Proc {
            pid: e.file_name().to_string_lossy().into_owned(),
            uid: m.uid(),
            homes: config::process_homes(&nul(p.join("cmdline")), &nul(p.join("environ"))),
        });
    }
    out
}

/// `(name, uid, home)` of the host's accounts.
fn accounts(a: &AppsSvc) -> Vec<(String, u32, String)> {
    std::fs::read_to_string(real(a, "/etc/passwd"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.len() >= 7 && !f[5].is_empty() && f[5] != "/").then(|| (f[0].to_string(), f[2].parse().unwrap_or(u32::MAX), f[5].trim_end_matches('/').to_string()))
        })
        .collect()
}

const DEFAULT_HOMES: &[&str] = &[".local/state/syncthing", ".config/syncthing"];

/// The homes a process uses: the ones it names, else the defaults of its user.
fn homes_of(p: &Proc, accounts: &[(String, u32, String)]) -> Vec<String> {
    if !p.homes.is_empty() {
        return p.homes.clone();
    }
    accounts
        .iter()
        .filter(|(_, u, _)| *u == p.uid)
        .flat_map(|(_, _, h)| DEFAULT_HOMES.iter().map(move |d| format!("{h}/{d}")))
        .collect()
}

/// Syncthing homes of the host's users, with their systemd unit when there is one.
fn service_sources(a: &AppsSvc) -> Vec<Resolved> {
    let accs = accounts(a);
    let procs = processes(a);
    let mut out = Vec::new();
    for (name, _uid, home) in &accs {
        let Some(st_home) = DEFAULT_HOMES.iter().map(|rel| format!("{home}/{rel}")).find(|h| real(a, &format!("{h}/config.xml")).exists()) else {
            continue;
        };
        let system = format!("syncthing@{name}.service");
        let unit = if real(a, &format!("/etc/systemd/system/multi-user.target.wants/{system}")).exists() {
            Some((system, false))
        } else if real(a, &format!("{home}/.config/systemd/user/default.target.wants/syncthing.service")).exists() {
            Some(("syncthing.service".to_string(), true))
        } else {
            None
        };
        let running = procs.iter().any(|p| homes_of(p, &accs).contains(&st_home));
        out.push(Resolved {
            candidate: AdoptCandidate {
                source: AdoptSourceRef { kind: AdoptSourceKind::Service, id: Some(name.clone()), path: None },
                name: unit
                    .as_ref()
                    .map(|(u, user)| if *user { format!("{u} ({name})") } else { u.clone() })
                    .unwrap_or_else(|| format!("{name} (no unit)")),
                home: Some(st_home.clone()),
                running,
                detail: st_home.clone(),
            },
            home: Some(st_home),
            user_home: Some(home.clone()),
            unit,
            ..Default::default()
        });
    }
    out
}

/// The chosen source among those found, or a home folder given by path.
fn resolve(found: &[Resolved], s: &AdoptSourceRef) -> Res<Resolved> {
    if s.kind == AdoptSourceKind::Path {
        let p = s.path.clone().filter(|p| p.starts_with('/') && !p.split('/').any(|x| x == ".."));
        let Some(p) = p else { return Err(Error::invalid("source.path", "an absolute path to the folder holding config.xml")) };
        let p = p.trim_end_matches('/').to_string();
        return Ok(Resolved {
            candidate: AdoptCandidate { source: s.clone(), name: p.clone(), home: Some(p.clone()), running: false, detail: String::new() },
            home: Some(p),
            ..Default::default()
        });
    }
    found
        .iter()
        .find(|r| r.candidate.source.kind == s.kind && r.candidate.source.id == s.id)
        .cloned()
        .ok_or_else(|| Error::not_found(format!("no such Syncthing source ({:?} {})", s.kind, s.id.as_deref().unwrap_or("-"))))
}

// ---------------------------------------------------------------------------------------------
// preview
// ---------------------------------------------------------------------------------------------

/// The preview and what the task needs beyond it.
struct Planned {
    preview: SyncthingAdoptPreview,
    resolved: Option<Resolved>,
    config_xml: Option<String>,
}

fn marker_state(root: &Path, host: &str, marker: &str, id: &str) -> MarkerState {
    let dir = real_in(root, &format!("{}/{marker}", host.trim_end_matches('/')));
    if dir.join(config::marker_file_name(id)).is_file() {
        MarkerState::Verified
    } else if dir.is_dir() {
        MarkerState::Unverified
    } else {
        MarkerState::Missing
    }
}

/// Owners of the folder roots and their top-level entries (at most 1000 each).
fn owners_of(a: &AppsSvc, roots: &[String]) -> Vec<(u32, u32)> {
    let mut v = Vec::new();
    for r in roots {
        let p = real(a, r);
        if let Ok(m) = std::fs::symlink_metadata(&p) {
            v.push((m.uid(), m.gid()));
        }
        for e in std::fs::read_dir(&p).into_iter().flatten().flatten().take(1000) {
            if let Ok(m) = e.metadata() {
                v.push((m.uid(), m.gid()));
            }
        }
    }
    v
}

async fn plan(a: &Arc<AppsSvc>, req: &SyncthingAdoptPreviewRequest) -> Res<Planned> {
    config::check_volumes(&req.volumes).map_err(|e| Error::invalid("volumes", e))?;
    let containers = container_sources(a).await;
    let (a2, req2) = (a.clone(), req.clone());
    tokio::task::spawn_blocking(move || plan_sync(&a2, &req2, containers)).await.map_err(Error::internal)?
}

/// The file reading part of the preview (blocking).
fn plan_sync(a: &AppsSvc, req: &SyncthingAdoptPreviewRequest, containers: Vec<Resolved>) -> Res<Planned> {
    let mut found = app_sources(a);
    found.extend(containers);
    found.extend(service_sources(a));
    let mut p = SyncthingAdoptPreview { candidates: found.iter().map(|r| r.candidate.clone()).collect(), ..Default::default() };
    let disk = a.on_disk();
    let installed = disk.iter().find(|d| d.meta.app == APP_ID);
    p.installed = installed.map(|d| d.stack.clone());
    let Some(src) = &req.source else {
        return Ok(Planned { preview: p, resolved: None, config_xml: None });
    };
    let r = resolve(&found, src)?;
    p.source = Some(r.candidate.clone());
    p.home = r.home.clone();
    let mut problems = Vec::new();
    if src.kind != AdoptSourceKind::App
        && let Some(d) = installed
        && version_lt(&d.meta.version, SHARE_SET_VERSION)
    {
        problems.push(format!("the installed app {} is the older layout: adopt it first, or update it", d.stack));
    }
    let Some(home) = r.home.clone() else {
        problems.push("no Syncthing home folder found for this source".into());
        p.problems = problems;
        return Ok(Planned { preview: p, resolved: Some(r), config_xml: None });
    };
    if src.kind == AdoptSourceKind::Path {
        problems.push(format!(
            "{INFO}rynas cannot stop the instance this home comes from: it must be stopped for good, and never started again, before the adoption"
        ));
        let accs = accounts(a);
        for pr in processes(a) {
            if homes_of(&pr, &accs).iter().any(|h| h.trim_end_matches('/') == home) {
                problems.push(format!("a Syncthing process (pid {}) runs with this home: stop it for good first", pr.pid));
            }
        }
    }
    let hp = real(a, &home);
    let canon_home = std::fs::canonicalize(&hp).ok();
    let has = |f: &str| canon_home.as_deref().and_then(|c| config::home_file(c, f)).is_some();
    p.files = AdoptHomeFiles {
        cert: has("cert.pem"),
        key: has("key.pem"),
        config: hp.join("config.xml").is_file(),
        https_cert: has("https-cert.pem"),
        database: if hp.join(DB_V2).exists() {
            AdoptDatabase::V2
        } else if hp.join(DB_V1).exists() {
            AdoptDatabase::V1
        } else {
            AdoptDatabase::None
        },
    };
    if !p.files.cert || !p.files.key {
        problems.push("cert.pem or key.pem is missing, or a link out of the home: the devices would see a new device id".into());
    }
    if p.files.database == AdoptDatabase::V1 {
        problems.push(format!("{INFO}a Syncthing 1 database: it is migrated on the first start, which can take long"));
    }
    if p.files.database == AdoptDatabase::None {
        problems.push(format!("{INFO}no database: every folder is scanned again"));
    }
    let xml = std::fs::read_to_string(hp.join("config.xml")).ok();
    let cfg: StConfig = match xml.as_deref().map(config::parse_config) {
        Some(Ok(c)) => c,
        Some(Err(e)) => {
            problems.push(e);
            StConfig::default()
        }
        None => {
            problems.push("config.xml is missing or unreadable".into());
            StConfig::default()
        }
    };
    let dup = config::duplicate_ids(&cfg);
    if !dup.is_empty() {
        problems.push(format!("config.xml names these folder ids twice: {}", dup.join(", ")));
    }
    p.gui_address = cfg.gui_address.clone();
    p.devices = cfg.devices.iter().map(|d| AdoptDevice { id_short: d.id.chars().take(7).collect(), name: d.name.clone() }).collect();
    p.volumes = config::merge_tables(r.table.clone(), &req.volumes);
    // where folders may land: the older layout's single item, or the applied shares
    let shares: Vec<(String, String)> = match &r.app_item {
        Some(item) => vec![item.clone()],
        None => a.applied_shares().into_iter().filter(|s| !s.path.is_empty()).map(|s| (s.name, s.path)).collect(),
    };
    let mut roots = Vec::new();
    let mut used = BTreeSet::new();
    for f in &cfg.folders {
        let mut af = AdoptFolder {
            id: f.id.clone(),
            label: f.label.clone(),
            path: f.path.clone(),
            folder_type: f.folder_type.clone(),
            devices: f.devices.iter().map(|d| d.chars().take(7).collect()).collect(),
            paused: f.paused,
            marker_name: f.marker_name.clone(),
            marker: MarkerState::Unknown,
            ..Default::default()
        };
        match config::host_path(&f.path, &p.volumes, r.user_home.as_deref()) {
            Err(why) => af.unmapped = Some(why),
            Ok(host) => {
                af.marker = marker_state(&a.docker.root, &host, &f.marker_name, &f.id);
                af.stignore = std::fs::read_to_string(real(a, &format!("{host}/.stignore"))).ok().and_then(|t| config::clean_stignore(&t));
                match config::share_path(&host, &shares, TARGET) {
                    Ok((share, new_path)) => {
                        used.insert(share.clone());
                        af.share = Some(share);
                        af.new_path = Some(new_path);
                        roots.push(host.clone());
                    }
                    Err(why) => af.unmapped = Some(why),
                }
                af.host_path = Some(host);
            }
        }
        af.will_pause = !f.paused && (af.unmapped.is_some() || af.marker != MarkerState::Verified);
        p.folders.push(af);
    }
    p.shares = used.into_iter().collect();
    let app = a.apps_ids().unwrap_or((65534, 65534));
    p.owners = config::owner_tally(&owners_of(a, &roots)).map(|((uid, gid), others)| AdoptOwners { uid, gid, others, app_uid: app.0, app_gid: app.1 });
    if r.candidate.running {
        problems.push(format!("{INFO}the source runs now: the adoption stops it first (stop_source)"));
    }
    if r.candidate.running && r.unit.is_none() && src.kind == AdoptSourceKind::Service {
        problems.push("the source runs without a systemd unit: stop it by hand first".into());
    }
    p.ok = p.files.config && p.files.cert && p.files.key && !problems.iter().any(|m| !m.starts_with(INFO));
    p.problems = problems;
    Ok(Planned { preview: p, resolved: Some(r), config_xml: xml })
}

async fn preview_post(State(st): State<AppsState>, hd: HeaderMap, body: axum::body::Bytes) -> Res<Json<SyncthingAdoptPreview>> {
    let a = svc(&st)?;
    admin(&a, &hd)?;
    let req: SyncthingAdoptPreviewRequest = crate::docker::optional_body(&body)?;
    Ok(Json(plan(&a, &req).await?.preview))
}

// ---------------------------------------------------------------------------------------------
// adoption
// ---------------------------------------------------------------------------------------------

/// The source of a plan, when it may be adopted with this request.
fn check_ready(p: &Planned, req: &SyncthingAdoptRequest) -> Result<Resolved, String> {
    if !p.preview.ok {
        return Err(p.preview.problems.iter().find(|m| !m.starts_with(INFO)).cloned().unwrap_or_else(|| "the source cannot be adopted".into()));
    }
    let r = p.resolved.clone().ok_or("no source")?;
    if r.candidate.running && !req.stop_source {
        return Err("the source runs: stop it first, or ask the adoption to (stop_source)".into());
    }
    Ok(r)
}

/// The stack an adoption lands in: the older layout's own, the installed 2.x app, or a new
/// one.
fn target_stack(req: &SyncthingAdoptRequest, p: &SyncthingAdoptPreview) -> String {
    match (&req.source.kind, &p.installed) {
        (AdoptSourceKind::App, _) => req.source.id.clone().unwrap_or_default(),
        (_, Some(s)) => s.clone(),
        (_, None) => req.stack.clone().unwrap_or_else(|| APP_ID.to_string()),
    }
}

/// Installed stacks of the entry other than `stack` (an adoption refuses them: one device id
/// must run in one place).
fn other_stacks(a: &AppsSvc, stack: &str) -> Vec<String> {
    a.on_disk().into_iter().filter(|d| d.meta.app == APP_ID && d.stack != stack).map(|d| d.stack).collect()
}

fn preview_request(req: &SyncthingAdoptRequest) -> SyncthingAdoptPreviewRequest {
    SyncthingAdoptPreviewRequest { source: Some(req.source.clone()), volumes: req.volumes.clone() }
}

async fn adopt_post(State(st): State<AppsState>, hd: HeaderMap, body: Result<Json<SyncthingAdoptRequest>, JsonRejection>) -> Res<Response> {
    let a = svc(&st)?;
    let who = admin(&a, &hd)?;
    let Json(req) = body?;
    let slot = take_slot()?;
    let planned = plan(&a, &preview_request(&req)).await?;
    let r = check_ready(&planned, &req).map_err(|e| if e.contains("stop_source") { conflict(e).field("stop_source") } else { conflict(e) })?;
    let stack = target_stack(&req, &planned.preview);
    if !rynas_apps::render::valid_stack_name(&stack) {
        return Err(Error::invalid("stack", format!("`{stack}` is not a valid stack name")));
    }
    let others = other_stacks(&a, &stack);
    if !others.is_empty() {
        return Err(conflict(format!("other Syncthing stacks are installed ({}): one device id must run in one place", others.join(", "))));
    }
    let guard = mark_busy(&a.docker, &stack)?;
    audit(&who, "adopt", &format!("{} into {stack}", r.candidate.name));
    let a2 = a.clone();
    let id = a.docker.ops.tasks.spawn("syncthing.adopt", &who.clone(), move |ctx| async move {
        let _slot = slot;
        let _guard = guard;
        adopt_task(&a2, &ctx, &who, &stack, req).await
    })?;
    Ok(accepted(id))
}

async fn run(a: &AppsSvc, ctx: &TaskCtx, cmd: Cmd) -> Result<(), String> {
    let out = a.docker.run(ctx, &cmd).await?;
    if !out.success() {
        return Err(format!("{} failed: {}", cmd.display(), out.stderr.trim()));
    }
    Ok(())
}

/// What turning the old instance off changed, to turn it on again after a failure.
#[derive(Debug, Default)]
struct Undo {
    /// Container name, the restart policy it had (other than `no`), and whether it ran.
    container: Option<(String, Option<String>, bool)>,
    /// Unit, its user (user units), and whether it was active.
    unit: Option<(String, Option<String>, bool)>,
}

fn systemctl(user: Option<&str>) -> Cmd {
    match user {
        Some(u) => Cmd::new("systemctl").args(["--user".to_string(), format!("--machine={u}@.host")]),
        None => Cmd::new("systemctl"),
    }
}

/// Turns the old instance off for good, running or not: a restart policy goes to `no`, a
/// unit is disabled (and stopped when active). `undo` records each step done.
async fn neutralize(a: &AppsSvc, ctx: &TaskCtx, r: &Resolved, undo: &mut Undo) -> Result<(), String> {
    match r.candidate.source.kind {
        AdoptSourceKind::Container => {
            let name = r.candidate.source.id.clone().unwrap_or_default();
            let policy = r.restart_policy.clone().filter(|p| p != "no");
            if let Some(pol) = &policy {
                ctx.log(&format!("container {name}: restart policy {pol} → no"));
                run(a, ctx, Cmd::new("docker").args(["update", "--restart=no", &name])).await?;
                undo.container = Some((name.clone(), policy.clone(), false));
            }
            if r.candidate.running {
                ctx.log(&format!("stopping container {name}"));
                run(a, ctx, Cmd::new("docker").args(["stop", &name])).await?;
                undo.container = Some((name, policy, true));
            }
            Ok(())
        }
        AdoptSourceKind::Service => match &r.unit {
            Some((unit, is_user)) => {
                let user = is_user.then(|| r.candidate.source.id.clone().unwrap_or_default());
                let active = r.candidate.running;
                ctx.log(&format!("disabling {unit}{}", if active { " and stopping it" } else { "" }));
                let mut args = vec!["disable"];
                if active {
                    args.push("--now");
                }
                args.push(unit);
                run(a, ctx, systemctl(user.as_deref()).args(args)).await?;
                undo.unit = Some((unit.clone(), user, active));
                Ok(())
            }
            None if r.candidate.running => Err("the source runs without a systemd unit: stop it by hand".into()),
            None => Ok(()),
        },
        _ => Ok(()),
    }
}

/// Turns the old instance on again as it was.
async fn restore_source(a: &AppsSvc, ctx: &TaskCtx, undo: &Undo) {
    if let Some((name, policy, ran)) = &undo.container {
        if let Some(pol) = policy {
            match run(a, ctx, Cmd::new("docker").args(["update", &format!("--restart={pol}"), name])).await {
                Ok(()) => ctx.log(&format!("restored: container {name} has restart policy {pol} again")),
                Err(e) => ctx.log(&format!("warning: restart policy of {name} not restored: {e}")),
            }
        }
        if *ran {
            match run(a, ctx, Cmd::new("docker").args(["start", name])).await {
                Ok(()) => ctx.log(&format!("restored: container {name} started again")),
                Err(e) => ctx.log(&format!("warning: container {name} not started again: {e}")),
            }
        }
    }
    if let Some((unit, user, active)) = &undo.unit {
        let mut args = vec!["enable"];
        if *active {
            args.push("--now");
        }
        args.push(unit);
        match run(a, ctx, systemctl(user.as_deref()).args(args)).await {
            Ok(()) => ctx.log(&format!("restored: {unit} enabled{} again", if *active { " and started" } else { "" })),
            Err(e) => ctx.log(&format!("warning: {unit} not enabled again: {e}")),
        }
    }
}

/// The REST interface of an installed app: its base URL and key, from the stack's `.env`.
fn api_of_env(env_text: &str) -> Result<(String, String), String> {
    let env = rynas_apps::render::parse_env(env_text);
    let port = env.get("WEB_PORT").cloned().ok_or("WEB_PORT missing in .env")?;
    let key = env.get("ST_API_KEY").cloned().filter(|k| !k.is_empty()).ok_or("ST_API_KEY missing in .env")?;
    Ok((format!("http://127.0.0.1:{port}"), key))
}

fn api_of(a: &AppsSvc, stack: &str) -> Result<(String, String), String> {
    let d = a.on_disk().into_iter().find(|d| d.stack == stack).ok_or_else(|| format!("no stack {stack}"))?;
    api_of_env(&d.env)
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// A small client of the app's REST interface (`X-API-Key`).
struct Rest {
    http: reqwest::Client,
    base: String,
    key: String,
}

impl Rest {
    fn new((base, key): (String, String)) -> Result<Self, String> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).no_proxy().build().map_err(|e| e.to_string())?;
        Ok(Self { http, base, key })
    }

    async fn healthy(&self) -> bool {
        match self.http.get(format!("{}/rest/noauth/health", self.base)).send().await {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    }

    async fn wait(&self, ctx: &TaskCtx) -> Result<(), String> {
        let start = std::time::Instant::now();
        while start.elapsed() < HEALTH_WAIT {
            if ctx.is_cancelled() {
                return Err("cancelled".into());
            }
            if self.healthy().await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err(format!("Syncthing did not answer within {} s", HEALTH_WAIT.as_secs()))
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> Result<String, String> {
        let r = rb.header("X-API-Key", &self.key).send().await.map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("Syncthing answered {}", r.status()));
        }
        r.text().await.map_err(|e| e.to_string())
    }

    async fn folders(&self) -> Result<Vec<serde_json::Value>, String> {
        let t = self.send(self.http.get(format!("{}/rest/config/folders", self.base))).await?;
        serde_json::from_str(&t).map_err(|e| e.to_string())
    }

    async fn patch_folder(&self, id: &str, body: &serde_json::Value) -> Result<(), String> {
        let rb = self
            .http
            .patch(format!("{}/rest/config/folders/{}", self.base, enc(id)))
            .header("Content-Type", "application/json")
            .body(body.to_string());
        self.send(rb).await.map(|_| ())
    }

    async fn scan(&self, id: &str) -> Result<(), String> {
        self.send(self.http.post(format!("{}/rest/db/scan?folder={}", self.base, enc(id)))).await.map(|_| ())
    }
}

/// Rescans every mapped folder the adoption leaves running.
async fn rescan(a: &AppsSvc, ctx: &TaskCtx, stack: &str, folders: &[AdoptFolder]) {
    if a.docker.dry {
        ctx.log("rescanning the folders (dry system: not done)");
        return;
    }
    let rest = match api_of(a, stack).and_then(Rest::new) {
        Ok(r) => r,
        Err(e) => return ctx.log(&format!("warning: no rescan: {e}")),
    };
    if let Err(e) = rest.wait(ctx).await {
        return ctx.log(&format!("warning: no rescan: {e} (Syncthing scans its folders at start anyway)"));
    }
    for f in folders.iter().filter(|f| f.new_path.is_some() && !f.paused && !f.will_pause) {
        match rest.scan(&f.id).await {
            Ok(()) => ctx.log(&format!("rescan of {} ({}) asked", f.label, f.id)),
            Err(e) => ctx.log(&format!("warning: rescan of {}: {e}", f.id)),
        }
    }
}

/// Writes `.stignore` of the folder `host` in place of the existing file: never through a
/// symlink (on the way or the file itself), a temporary file in the same folder renamed
/// over it, with the old file's owner and mode.
fn write_stignore(root: &Path, host: &str, text: &str) -> std::io::Result<()> {
    use rynas_docker::safefs;
    let d = safefs::open_dir(root, host)?;
    let (mode, uid, gid) = safefs::stat_of(&safefs::open_file_at(&d, ".stignore")?)?;
    safefs::write_at(&d, ".stignore", text.as_bytes(), mode, Some((uid, gid)))
}

/// Writes the `.stignore` proposals of mapped folders whose marker is verified.
fn apply_stignore(a: &AppsSvc, ctx: &TaskCtx, folders: &[AdoptFolder]) {
    for f in folders.iter().filter(|f| f.new_path.is_some() && f.marker == MarkerState::Verified) {
        let (Some(host), Some(s)) = (&f.host_path, &f.stignore) else { continue };
        if a.docker.dry {
            ctx.log(&format!("{host}/.stignore: {} line(s) out, {} in (dry system: not written)", s.removed.len(), s.added.len()));
            continue;
        }
        match write_stignore(&a.docker.root, host, &s.after) {
            Ok(()) => ctx.log(&format!("{host}/.stignore: {} line(s) out, {} in", s.removed.len(), s.added.len())),
            Err(e) => ctx.log(&format!("warning: {host}/.stignore not written: {e}")),
        }
    }
}

fn copy_tree(src: &Path, dst: &Path, owner: (u32, u32)) -> Result<u64, String> {
    let m = std::fs::symlink_metadata(src).map_err(|e| format!("{}: {e}", src.display()))?;
    if m.is_dir() {
        std::fs::create_dir_all(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        let mut n = 0;
        for e in std::fs::read_dir(src).map_err(|e| format!("{}: {e}", src.display()))?.flatten() {
            n += copy_tree(&e.path(), &dst.join(e.file_name()), owner)?;
        }
        std::os::unix::fs::chown(dst, Some(owner.0), Some(owner.1)).map_err(|e| format!("chown {}: {e}", dst.display()))?;
        Ok(n)
    } else if m.is_file() {
        std::fs::copy(src, dst).map_err(|e| format!("{} → {}: {e}", src.display(), dst.display()))?;
        std::os::unix::fs::chown(dst, Some(owner.0), Some(owner.1)).map_err(|e| format!("chown {}: {e}", dst.display()))?;
        Ok(1)
    } else {
        // links and special files stay behind
        Ok(0)
    }
}

/// The real path of `p` (canonical when it exists, else as given).
fn canon(root: &Path, p: &str) -> String {
    let r = real_in(root, p);
    std::fs::canonicalize(&r).unwrap_or(r).to_string_lossy().into_owned()
}

/// Why the home `home` cannot be copied to `dest`: the same folder, or inside it (the copy
/// would move the source aside).
fn copy_conflict(root: &Path, home: &str, dest: &str) -> Option<String> {
    let (h, d) = (canon(root, home), canon(root, dest));
    config::is_within(&h, &d).then(|| format!("the home {home} is the app's home {dest} or inside it"))
}

/// Copies the old home into `dest` (`<config folder>/config` of the app; a home already
/// there is kept aside as `<dest>.before-adopt-<ms>`, returned), with `config_xml` written
/// instead of the original. cert.pem and key.pem must be regular files (or links to files
/// inside the home); owned by the app's account, keys and config 0600.
fn copy_home(root: &Path, dry: bool, log: &dyn Fn(&str), home: &str, dest: &str, config_xml: &str, owner: (u32, u32)) -> Result<Option<String>, String> {
    if let Some(e) = copy_conflict(root, home, dest) {
        return Err(e);
    }
    let src = std::fs::canonicalize(real_in(root, home)).map_err(|e| format!("{home}: {e}"))?;
    let files: Vec<(&str, PathBuf)> = HOME_FILES.iter().filter_map(|f| config::home_file(&src, f).map(|p| (*f, p))).collect();
    for f in IDENTITY {
        if !files.iter().any(|(n, _)| n == f) {
            return Err(format!("{home}/{f} is not a regular file of the home: the devices would see a new device id"));
        }
    }
    if dry {
        log(&format!("copying {home} to {dest}, owner {}:{} (dry system: not done)", owner.0, owner.1));
        return Ok(None);
    }
    let dst = real_in(root, dest);
    let mut aside = None;
    if std::fs::symlink_metadata(&dst).is_ok() {
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let name = format!("{dest}.before-adopt-{ms}");
        std::fs::rename(&dst, real_in(root, &name)).map_err(|e| format!("{dest}: {e}"))?;
        log(&format!("the app's previous home is kept as {name}"));
        aside = Some(name);
    }
    let r = (|| {
        std::fs::create_dir_all(&dst).map_err(|e| format!("{dest}: {e}"))?;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("{dest}: {e}"))?;
        let mut n = 0;
        for (name, from) in &files {
            let to = dst.join(name);
            std::fs::copy(from, &to).map_err(|e| format!("{home}/{name}: {e}"))?;
            std::os::unix::fs::chown(&to, Some(owner.0), Some(owner.1)).map_err(|e| format!("chown {name}: {e}"))?;
            if name.ends_with("key.pem") {
                std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("{name}: {e}"))?;
            }
            n += 1;
        }
        for db in [DB_V2, DB_V1] {
            let s = src.join(db);
            if s.exists() {
                n += copy_tree(&s, &dst.join(db), owner)?;
            }
        }
        let cfg = dst.join("config.xml");
        std::fs::write(&cfg, config_xml).map_err(|e| format!("{}: {e}", cfg.display()))?;
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("{}: {e}", cfg.display()))?;
        std::os::unix::fs::chown(&cfg, Some(owner.0), Some(owner.1)).map_err(|e| format!("chown config.xml: {e}"))?;
        std::os::unix::fs::chown(&dst, Some(owner.0), Some(owner.1)).map_err(|e| format!("chown {dest}: {e}"))?;
        Ok::<u64, String>(n + 1)
    })();
    match r {
        Ok(n) => {
            log(&format!("copied {home} to {dest} ({n} files, owner {}:{}); the old home is left as it was", owner.0, owner.1));
            Ok(aside)
        }
        Err(e) => {
            put_back(root, log, dest, aside.as_deref());
            Err(e)
        }
    }
}

/// Removes a copied home and puts the one kept aside back.
fn put_back(root: &Path, log: &dyn Fn(&str), dest: &str, aside: Option<&str>) {
    let dst = real_in(root, dest);
    if let Err(e) = std::fs::remove_dir_all(&dst)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log(&format!("warning: {dest} not removed: {e}"));
        return;
    }
    if let Some(a) = aside {
        match std::fs::rename(real_in(root, a), &dst) {
            Ok(()) => log(&format!("restored: the app's previous home {a} is back at {dest}")),
            Err(e) => log(&format!("warning: {a} not put back at {dest}: {e}")),
        }
    }
}

/// The edits of the copied `config.xml`: new paths for mapped folders, `paused` for the
/// folders that must not run yet.
fn edits_of(folders: &[AdoptFolder]) -> Vec<FolderEdit> {
    folders
        .iter()
        .filter(|f| f.new_path.is_some() || f.will_pause)
        .map(|f| FolderEdit { id: f.id.clone(), path: f.new_path.clone(), pause: f.will_pause })
        .collect()
}

/// The installed app's items kept as they are (names, paths, `./` folders of the stack) plus
/// the new ones that name another folder and path.
fn merge_share_items(existing: Vec<ShareSetItem>, new: Vec<ShareSetItem>) -> Vec<ShareSetItem> {
    let mut out = existing;
    for n in new {
        let same = |i: &ShareSetItem| i.name == n.name || i.path.trim_end_matches('/') == n.path.trim_end_matches('/');
        if !out.iter().any(same) {
            out.push(n);
        }
    }
    out
}

/// `(PUID, PGID)` and the app's home from an installed stack's `.env`.
fn owner_and_home(layout: &Layout, stack: &str, env_text: &str) -> ((u32, u32), String) {
    let env = rynas_apps::render::parse_env(env_text);
    let id = |k: &str| env.get(k).and_then(|v| v.parse().ok()).unwrap_or(65534);
    let config = stack_path(&layout.stack(stack), env.get("CONFIG_FOLDER").map(String::as_str).unwrap_or("./state"));
    ((id("PUID"), id("PGID")), format!("{config}/{HOME_IN_CONFIG}"))
}

/// Where the adopted home goes, prepared before the old instance is touched.
enum Target {
    /// The 2.x app installed: its share set job, owner and home.
    Existing { job: Box<crate::apps::ShareSetJob>, owner: (u32, u32), dest: String },
    /// A new install (not started), its home in the default config folder.
    New { job: Box<crate::apps::InstallJob>, dest: String },
}

async fn adopt_task(a: &Arc<AppsSvc>, ctx: &TaskCtx, who: &str, stack: &str, req: SyncthingAdoptRequest) -> Result<(), String> {
    // checked again in the task: the world may have changed since the request
    let planned = plan(a, &preview_request(&req)).await.map_err(|e| e.2)?;
    let r = check_ready(&planned, &req)?;
    let others = other_stacks(a, stack);
    if !others.is_empty() {
        return Err(format!("other Syncthing stacks are installed: {}", others.join(", ")));
    }
    let p = planned.preview;
    ctx.log(&format!(
        "adopting {} ({} folders, {} paused after adoption, shares: {})",
        r.candidate.name,
        p.folders.len(),
        p.folders.iter().filter(|f| f.will_pause).count(),
        if p.shares.is_empty() { "none".into() } else { p.shares.join(", ") }
    ));
    if req.source.kind == AdoptSourceKind::App {
        return adopt_app_layout(a, ctx, stack, &p, req.apply_stignore).await;
    }

    // every check that can fail, before the old instance is touched
    let xml = planned.config_xml.ok_or("config.xml is unreadable")?;
    let new_xml = config::rewrite_config(&xml, &edits_of(&p.folders))?;
    let home = r.home.clone().ok_or("no home")?;
    let layout = a.docker.layout().ok_or("Docker has no volume")?;
    let choices: Vec<ShareSetChoice> = p.shares.iter().map(|s| ShareSetChoice { share: s.clone(), follow: None }).collect();
    let target = match a.on_disk().into_iter().find(|d| d.stack == stack) {
        Some(d) => {
            let (owner, dest) = owner_and_home(&layout, stack, &d.env);
            let env = rynas_apps::render::parse_env(&d.env);
            let existing = match env.get("SYNC_SHARES").map(String::as_str).unwrap_or("") {
                "" => Vec::new(),
                v => rynas_apps::render::decode_share_set(v).map_err(|e| format!("the installed share set: {e}"))?,
            };
            let fresh: Vec<ShareSetChoice> = choices.into_iter().filter(|c| !existing.iter().any(|i| i.name == c.share)).collect();
            let new = crate::apps::resolve_share_set(&a.applied_shares(), &fresh)?;
            let job = a.prepare_share_set(stack, merge_share_items(existing, new)).await.map_err(|e| e.2)?;
            Target::Existing { job: Box::new(job), owner, dest }
        }
        None => {
            let ireq = AppInstallRequest { stack: Some(stack.to_string()), share_set: choices, start: false, ..Default::default() };
            let (job, _) = crate::apps::install_job(a, APP_ID, &ireq, who).await.map_err(|e| e.2)?;
            Target::New { job: Box::new(job), dest: format!("{}/state/{HOME_IN_CONFIG}", layout.stack(stack)) }
        }
    };
    let dest = match &target {
        Target::Existing { dest, .. } | Target::New { dest, .. } => dest.clone(),
    };
    if let Some(e) = copy_conflict(&a.docker.root, &home, &dest) {
        return Err(e);
    }
    ctx.progress(10);

    // the old instance off for good, then the files
    let mut undo = Undo::default();
    if let Err(e) = neutralize(a, ctx, &r, &mut undo).await {
        ctx.log(&format!("{e}: turning the source on again"));
        restore_source(a, ctx, &undo).await;
        return Err(e);
    }
    if req.apply_stignore {
        apply_stignore(a, ctx, &p.folders);
    }
    let mut copied: Option<Option<String>> = None;
    if let Err(e) = place(a, ctx, who, stack, &layout, target, &home, &new_xml, &mut copied).await {
        ctx.log(&format!("adoption failed ({e}): rolling back"));
        if let Some(aside) = copied {
            if let Err(e2) = compose_run(&a.docker, ctx, stack, &layout, ComposeAction::Stop).await {
                ctx.log(&format!("warning: {stack} not stopped: {e2}"));
            }
            if !a.docker.dry {
                let (root, log, d2) = (a.docker.root.clone(), ctx.clone(), dest.clone());
                let _ = blocking(move || {
                    put_back(&root, &|m| log.log(m), &d2, aside.as_deref());
                    Ok(())
                })
                .await;
            }
        }
        restore_source(a, ctx, &undo).await;
        return Err(e);
    }
    ctx.progress(80);
    rescan(a, ctx, stack, &p.folders).await;
    ctx.progress(100);
    ctx.log(&format!("{} adopted as {stack}", r.candidate.name));
    Ok(())
}

/// Installs or stops the app, copies the home in, starts it. `copied` is set once the home
/// is in place (with the previous home kept aside, if any).
#[allow(clippy::too_many_arguments)]
async fn place(
    a: &Arc<AppsSvc>,
    ctx: &TaskCtx,
    who: &str,
    stack: &str,
    layout: &Layout,
    target: Target,
    home: &str,
    xml: &str,
    copied: &mut Option<Option<String>>,
) -> Result<(), String> {
    let copy = |owner: (u32, u32), dest: String| {
        let (root, dry, log, h, x) = (a.docker.root.clone(), a.docker.dry, ctx.clone(), home.to_string(), xml.to_string());
        blocking(move || copy_home(&root, dry, &|m| log.log(m), &h, &dest, &x, owner))
    };
    match target {
        Target::Existing { job, owner, dest } => {
            ctx.log(&format!("{stack} is installed: stopping it to put the adopted home in"));
            compose_run(&a.docker, ctx, stack, layout, ComposeAction::Stop).await?;
            *copied = Some(copy(owner, dest).await?);
            crate::apps::share_set_task(a, ctx, who, *job).await
        }
        Target::New { job, dest } => {
            crate::apps::install_task(a, ctx, *job).await?;
            let d = a.on_disk().into_iter().find(|d| d.stack == stack).ok_or("the installed stack is missing")?;
            let (owner, _) = owner_and_home(layout, stack, &d.env);
            *copied = Some(copy(owner, dest).await?);
            ctx.log("starting the app");
            compose_run(&a.docker, ctx, stack, layout, ComposeAction::Up).await?;
            a.docker.changed();
            Ok(())
        }
    }
}

/// The older catalogue layout: update to the share set in place; the update moves each
/// folder from `/data/...` to `/shares/<share>/...` ([`move_data_folders`]).
async fn adopt_app_layout(a: &Arc<AppsSvc>, ctx: &TaskCtx, stack: &str, p: &SyncthingAdoptPreview, stignore: bool) -> Result<(), String> {
    let (job, plan) = a.prepare_update_job(stack, &AppUpdateRequest::default()).await.map_err(|e| e.2)?;
    if stignore {
        apply_stignore(a, ctx, &p.folders);
    }
    ctx.log(&format!("updating {stack} from {} to {} (the share moves to /shares)", plan.from_version, plan.to_version));
    crate::apps::update_task(&a.docker, ctx, job).await?;
    ctx.progress(100);
    ctx.log(&format!("{stack} uses /shares now"));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// the update from the older layout
// ---------------------------------------------------------------------------------------------

/// An update of the entry from 1.x to 2.x or later: its folders move from `/data`.
pub(crate) fn needs_folder_move(app: &str, from: &str, to: &str) -> bool {
    app == APP_ID && major(from) == Some(1) && major(to).is_some_and(|m| m >= 2)
}

/// A folder path of the older layout → its path below the share set item `name`, and the
/// part below `/data`.
fn data_move(path: &str, name: &str) -> Option<(String, String)> {
    let rel = config::under(path.trim_end_matches('/'), OLD_DATA)?;
    Some((config::join(&format!("{TARGET}/{name}"), &rel), rel))
}

/// After an update from 1.x: every folder below `/data` gets its path below
/// `/shares/<name>`, paused when its marker is not verified on the host, then the others are
/// rescanned. Failures are warnings: the update stays done, the folders stay paused or in
/// error (Syncthing deletes nothing for a folder whose path or marker is missing).
pub(crate) async fn move_data_folders(s: &Arc<DockerSvc>, ctx: &TaskCtx, stack: &str, layout: &Layout, env_text: &str) {
    let env = rynas_apps::render::parse_env(env_text);
    let item = env
        .get("SYNC_SHARES")
        .and_then(|v| rynas_apps::render::decode_share_set(v).ok())
        .and_then(|v| v.into_iter().next());
    let Some(item) = item else {
        ctx.log("no shared folder in the new share set: the folders below /data stay as they are");
        return;
    };
    let host = stack_path(&layout.stack(stack), &item.path);
    if s.dry {
        ctx.log(&format!("moving the Syncthing folders from {OLD_DATA} to {TARGET}/{} (dry system: not done)", item.name));
        return;
    }
    let warn = |e: String| {
        ctx.log(&format!(
            "warning: the folders below {OLD_DATA} were not moved ({e}): they stay paused or in error until their path is changed in Syncthing"
        ))
    };
    let rest = match api_of_env(env_text).and_then(Rest::new) {
        Ok(r) => r,
        Err(e) => return warn(e),
    };
    if let Err(e) = rest.wait(ctx).await {
        return warn(e);
    }
    let folders = match rest.folders().await {
        Ok(f) => f,
        Err(e) => return warn(e),
    };
    let mut moved = Vec::new();
    for f in &folders {
        let get = |k: &str| f.get(k).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
        let (id, path) = (get("id"), get("path"));
        let Some((new_path, rel)) = data_move(&path, &item.name) else { continue };
        let marker = Some(get("markerName")).filter(|m| !m.is_empty()).unwrap_or_else(|| config::DEFAULT_MARKER.into());
        let verified = marker_state(&s.root, &config::join(&host, &rel), &marker, &id) == MarkerState::Verified;
        let mut body = serde_json::json!({ "path": new_path });
        if !verified {
            body["paused"] = true.into();
        }
        match rest.patch_folder(&id, &body).await {
            Ok(()) => {
                ctx.log(&format!("folder {id}: {path} → {new_path}{}", if verified { "" } else { " (paused: marker not verified)" }));
                let was_paused = f.get("paused").and_then(serde_json::Value::as_bool).unwrap_or(false);
                if verified && !was_paused {
                    moved.push(id);
                }
            }
            Err(e) => warn(format!("folder {id}: {e}")),
        }
    }
    for id in moved {
        match rest.scan(&id).await {
            Ok(()) => ctx.log(&format!("rescan of {id} asked")),
            Err(e) => ctx.log(&format!("warning: rescan of {id}: {e}")),
        }
    }
}

pub fn router(st: AppsState) -> Router {
    Router::new()
        .route("/v1/syncthing/adopt/preview", post(preview_post))
        .route("/v1/syncthing/adopt", post(adopt_post))
        .with_state(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_helpers() {
        assert_eq!(enc("a b/c"), "a%20b%2Fc");
        assert_eq!(stack_path("/v/docker/stacks/st/", "./state"), "/v/docker/stacks/st/state");
        assert_eq!(stack_path("/x", "/abs"), "/abs");
        let f = AdoptFolder { id: "a".into(), new_path: Some("/shares/x".into()), ..Default::default() };
        let g = AdoptFolder { id: "b".into(), will_pause: true, ..Default::default() };
        let h = AdoptFolder { id: "c".into(), ..Default::default() };
        let e = edits_of(&[f, g, h]);
        assert_eq!(e.len(), 2);
        assert!(e[1].pause && e[1].path.is_none());
    }

    #[test]
    fn preview_json_never_carries_secrets() {
        let xml = r#"<configuration><device id="AAAAAAA-BBBBBBB" name="laptop"/><gui><address>0.0.0.0:8384</address><apikey>TOPSECRETKEY</apikey><password>hash</password></gui><folder id="f" path="/x"/></configuration>"#;
        let c = config::parse_config(xml).unwrap();
        let p = SyncthingAdoptPreview {
            devices: c.devices.iter().map(|d| AdoptDevice { id_short: d.id.chars().take(7).collect(), name: d.name.clone() }).collect(),
            gui_address: c.gui_address.clone(),
            ..Default::default()
        };
        let j = serde_json::to_string(&p).unwrap();
        assert!(!j.contains("TOPSECRETKEY") && !j.contains("hash") && !j.contains("BBBBBBB"), "{j}");
        assert!(j.contains("AAAAAAA"));
    }

    #[test]
    fn one_adoption_at_a_time() {
        let s = take_slot().unwrap();
        assert!(take_slot().is_err());
        drop(s);
        drop(take_slot().unwrap());
    }

    #[test]
    fn installed_items_are_kept() {
        let it = |n: &str, p: &str| ShareSetItem { name: n.into(), path: p.into(), follow: false };
        let existing = vec![it("sync", "./sync"), it("photos", "/vol1/photos")];
        let new = vec![it("photos", "/vol1/photos"), it("pics", "/vol1/photos/"), it("docs", "/vol1/docs")];
        assert_eq!(merge_share_items(existing, new), [it("sync", "./sync"), it("photos", "/vol1/photos"), it("docs", "/vol1/docs")]);
    }

    #[test]
    fn the_folder_move_of_the_older_layout() {
        assert!(needs_folder_move("syncthing", "1.0.0", "2.0.0"));
        assert!(needs_folder_move("syncthing", "1.4.2", "3.0.0"));
        assert!(!needs_folder_move("syncthing", "2.0.0", "2.1.0"));
        assert!(!needs_folder_move("immich", "1.0.0", "2.0.0"));
        assert_eq!(data_move("/data/photos/2024", "sync"), Some(("/shares/sync/photos/2024".into(), "photos/2024".into())));
        assert_eq!(data_move("/data", "sync"), Some(("/shares/sync".into(), String::new())));
        assert_eq!(data_move("/database", "sync"), None);
        assert_eq!(data_move("/var/syncthing/Sync", "sync"), None);
    }

    fn me(p: &Path) -> (u32, u32) {
        let m = std::fs::metadata(p).unwrap();
        (m.uid(), m.gid())
    }

    #[test]
    fn stignore_is_written_in_place_and_never_through_a_link() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("vol1/photos")).unwrap();
        let f = root.join("vol1/photos/.stignore");
        std::fs::write(&f, "@eaDir\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_stignore(root, "/vol1/photos", "(?d)#recycle\n").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "(?d)#recycle\n");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o640);
        // a link as the file, and a link on the way
        std::fs::create_dir_all(root.join("vol1/other")).unwrap();
        std::fs::write(root.join("secret"), "keep").unwrap();
        std::os::unix::fs::symlink(root.join("secret"), root.join("vol1/other/.stignore")).unwrap();
        assert!(write_stignore(root, "/vol1/other", "x").is_err());
        std::os::unix::fs::symlink(root.join("vol1/photos"), root.join("vol1/via")).unwrap();
        assert!(write_stignore(root, "/vol1/via", "x").is_err());
        assert_eq!(std::fs::read_to_string(root.join("secret")).unwrap(), "keep");
    }

    #[test]
    fn home_copy_checks_identity_files_and_places() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let owner = me(root);
        let home = root.join("old");
        std::fs::create_dir_all(home.join("index-v2")).unwrap();
        std::fs::write(home.join("cert.pem"), "c").unwrap();
        std::fs::write(home.join("index-v2/main.db"), "db").unwrap();
        std::fs::write(root.join("elsewhere.pem"), "k").unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere.pem"), home.join("key.pem")).unwrap();
        let log = |_: &str| {};
        // key.pem is a link out of the home: refused before anything moves
        std::fs::create_dir_all(root.join("app/config")).unwrap();
        std::fs::write(root.join("app/config/config.xml"), "previous").unwrap();
        let e = copy_home(root, false, &log, "/old", "/app/config", "<configuration/>", owner).unwrap_err();
        assert!(e.contains("key.pem"), "{e}");
        assert_eq!(std::fs::read_to_string(root.join("app/config/config.xml")).unwrap(), "previous");
        // a link inside the home is fine; the previous home goes aside
        std::fs::remove_file(home.join("key.pem")).unwrap();
        std::fs::create_dir_all(home.join("keys")).unwrap();
        std::fs::write(home.join("keys/key.pem"), "k").unwrap();
        std::os::unix::fs::symlink("keys/key.pem", home.join("key.pem")).unwrap();
        let aside = copy_home(root, false, &log, "/old", "/app/config", "<configuration/>", owner).unwrap().expect("aside");
        let dst = root.join("app/config");
        assert!(std::fs::symlink_metadata(dst.join("key.pem")).unwrap().is_file());
        assert_eq!(std::fs::metadata(dst.join("key.pem")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(dst.join("index-v2/main.db")).unwrap(), "db");
        assert_eq!(std::fs::read_to_string(dst.join("config.xml")).unwrap(), "<configuration/>");
        assert_eq!(std::fs::read_to_string(real_in(root, &aside).join("config.xml")).unwrap(), "previous");
        // the old home is untouched
        assert!(std::fs::symlink_metadata(home.join("key.pem")).unwrap().file_type().is_symlink());
        // put back after a failure
        put_back(root, &log, "/app/config", Some(&aside));
        assert_eq!(std::fs::read_to_string(dst.join("config.xml")).unwrap(), "previous");
        // the source is the destination, or inside it
        assert!(copy_home(root, false, &log, "/app/config", "/app/config", "x", owner).unwrap_err().contains("inside it"));
        std::fs::create_dir_all(root.join("app/config/sub")).unwrap();
        assert!(copy_home(root, false, &log, "/app/config/sub", "/app/config", "x", owner).unwrap_err().contains("inside it"));
        assert!(copy_conflict(root, "/old", "/app/config").is_none());
    }
}