//! Photos from the RyNAS app (D-PHOTOS, `docs/phase2/photos.md`).
//!
//! The phone app's photo backup is the single way photos come in: each NAS user has one
//! folder, `Photos` in their home (`<home>/Photos/YYYY/MM`, the files API path
//! [`APP_DIR`]). The catalogue's photo apps read those folders read-only and rynas wires them:
//!
//! - **Mounts**: an entry with `[user_photos]` gets one read-only bind per user's folder at
//!   `<target>/<user>` (never a whole home). [`prepare_folders`] makes the missing folders
//!   (owner the user, 0700), refuses a `Photos` that is a link or belongs to someone else, and
//!   lets the apps' account (`docker-apps`) read them through ACLs (only bits added, default
//!   ACLs on folders, no link followed). The install mounts the folders of that moment;
//!   "Sync" (`POST /v1/apps/installed/{stack}/photos/sync`) adds new users later.
//! - **Immich**: with an API key the administrator gives rynas
//!   (`POST /v1/apps/installed/{stack}/photos/key`, kept in the stack's `.env` like every app
//!   secret, never logged nor returned), "Sync" creates for each user with an Immich account
//!   an external library owned by that account whose only import path is the user's folder,
//!   then queues its scan. Accounts match by email (the NAS user's, when set), else by name
//!   or the email's local part; several matches choose none. Libraries are never deleted
//!   nor changed: deleting one deletes its assets in Immich.
//! - **Rescan**: a finished upload into [`APP_DIR`] ([`uploaded`], from the files gateway)
//!   queues that user; after [`QUIET`] without another upload (at most [`MAX_WAIT`] after the
//!   first) rynas asks Immich to scan the user's library. Immich's own daily scan catches
//!   anything missed; its experimental file watcher stays off.
//! - **PiGallery2** has no API key: the folders are mounted; per-user directory allow lists
//!   are set in the gallery by hand (the page shows the values, [`ALLOW_LIST`]).

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::apps::{PhotosApi, PhotosKeyRequest, PhotosLibrary, PhotosState, PhotosUser};
use rynas_apps::CatalogApp;
use rynas_apps::render::ShareSetItem;
use rynas_docker::Layout;
use rynas_docker::stacks::{COMPOSE_FILE, ComposeAction};
use rynas_tasks::TaskCtx;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::apps::{AppsState, AppsSvc, OnDisk, admin, asvc, blocking, compose_run, file_text, render_err, set_env_line, write_stack};
use crate::docker::{accepted, conflict, mark_busy};
use crate::ops::{Error, Res};

/// The folder of a home the RyNAS app backs up into.
pub const FOLDER: &str = "Photos";
/// That folder in the files API (the user's home is the share `home`).
pub const APP_DIR: &str = "/home/Photos";
/// A user's library is scanned once no upload came for this long…
pub const QUIET: Duration = Duration::from_secs(45);
/// …and at the latest this long after the first upload of a batch.
pub const MAX_WAIT: Duration = Duration::from_secs(600);
/// How often due scans are looked for.
const TICK: Duration = Duration::from_secs(10);
/// Name of the libraries rynas creates.
pub const LIBRARY_NAME: &str = "Photos from the RyNAS app";
/// PiGallery2's directory allow list of a user (`{user}` replaced by the UI).
pub const ALLOW_LIST: &str = "/{user}/*";
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

fn audit(who: &str, action: &str, target: &str) {
    tracing::info!(target: "rynas::audit", "apps {action} {target} by {who}");
}

// ---------------------------------------------------------------------------------------------
// folders
// ---------------------------------------------------------------------------------------------

/// A NAS user with a home (`/etc/passwd`): regular uids only, never root, a system account or
/// the apps' account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NasUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
}

/// The NAS users of `passwd` whose home field is an absolute path, sorted by name.
pub fn nas_users(passwd: &str) -> Vec<NasUser> {
    let mut v: Vec<NasUser> = passwd
        .lines()
        .filter_map(|l| {
            let (name, uid, gid) = crate::apps::nas_users(l).into_iter().next()?;
            let home = l.split(':').nth(5)?.trim_end_matches('/').to_string();
            (home.starts_with('/') && home.len() > 1).then_some(NasUser { name, uid, gid, home })
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// The user's real home below `root`: the passwd home with its links resolved (they are
/// root's: `/srv/rynas/homes/<user>` points at the folder on a volume), a folder of the user.
fn real_home(root: &Path, u: &NasUser) -> Result<std::path::PathBuf, String> {
    let home = rynas_docker::under(root, &u.home)
        .canonicalize()
        .map_err(|_| format!("{}: the home {} does not exist", u.name, u.home))?;
    let m = std::fs::metadata(&home).map_err(|e| format!("{}: {e}", u.name))?;
    if !m.is_dir() || m.uid() != u.uid {
        return Err(format!("{}: the home {} is not a folder of the user", u.name, u.home));
    }
    Ok(home)
}

/// A host path below `root` as the host sees it.
fn on_host(root: &Path, p: &Path) -> String {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    match p.strip_prefix(&root) {
        Ok(rel) if root != Path::new("/") => format!("/{}", rel.display()),
        _ => p.display().to_string(),
    }
}

/// Where a user's folder is (`<real home>/Photos`), made when missing and `create`: owner
/// the user and their primary group, 0700 like the home. A `Photos` that is a link, not a
/// folder, or not the user's is refused: the bind would expose something else.
pub fn ensure_folder(root: &Path, u: &NasUser, create: bool) -> Result<String, String> {
    let home = real_home(root, u)?;
    let dir = home.join(FOLDER);
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if m.file_type().is_symlink() => return Err(format!("{}: {FOLDER} is a link, not a folder", u.name)),
        Ok(m) if !m.is_dir() => return Err(format!("{}: {FOLDER} is not a folder", u.name)),
        Ok(m) if m.uid() != u.uid => return Err(format!("{}: {FOLDER} does not belong to the user", u.name)),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
            std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| format!("{}: {FOLDER}: {e}", u.name))?;
            std::os::unix::fs::lchown(&dir, Some(u.uid), Some(u.gid)).map_err(|e| format!("{}: {FOLDER}: {e}", u.name))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {FOLDER}: {e}", u.name)),
    }
    let path = on_host(root, &dir);
    // the .env list cannot hold every character (rynas_apps::render::check_user_photos)
    rynas_apps::render::check_user_photos("folder", &[ShareSetItem { name: u.name.clone(), path: path.clone(), follow: false }])
        .map_err(|_| format!("{}: {path} cannot be mounted (unusual characters)", u.name))?;
    Ok(path)
}

/// The users' folders an app mounts, and what kept the others out.
pub fn folder_items(root: &Path, users: &[NasUser], create: bool) -> (Vec<ShareSetItem>, Vec<String>) {
    let (mut items, mut problems) = (Vec::new(), Vec::new());
    for u in users {
        match ensure_folder(root, u, create) {
            Ok(path) if create || rynas_docker::under(root, &path).is_dir() => items.push(ShareSetItem { name: u.name.clone(), path, follow: false }),
            Ok(_) => {}
            Err(e) => problems.push(e),
        }
    }
    (items, problems)
}

/// Makes the users' folders and lets the apps' account read them: what an install and a
/// sync mount. A user whose folder cannot be used is left out (logged).
pub(crate) async fn prepare_folders(a: &Arc<AppsSvc>, ctx: &TaskCtx) -> Vec<ShareSetItem> {
    let (root, dry, gid) = (a.docker.root.clone(), a.docker.dry, a.apps_ids().map(|i| i.1));
    let log = ctx.clone();
    let cancel = ctx.clone();
    blocking(move || {
        let passwd = std::fs::read_to_string(rynas_docker::under(&root, "/etc/passwd")).unwrap_or_default();
        let (items, problems) = folder_items(&root, &nas_users(&passwd), !dry);
        for p in &problems {
            log.log(&format!("warning: {p}: not mounted"));
        }
        if let Some(gid) = gid.filter(|_| !dry) {
            use rynas_services::idremap::{self, GroupGrant, HostRemap};
            for it in &items {
                let c = idremap::grant(&HostRemap, &[rynas_docker::under(&root, &it.path)], GroupGrant { gid, user: false, write: false }, &|| cancel.is_cancelled());
                if c.errors > 0 {
                    log.log(&format!("warning: {}: {} entries could not be opened to {}: {}", it.path, c.errors, rynas_docker::APPS_USER, c.error_samples.join("; ")));
                }
            }
        }
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        log.log(&format!("Photos folders: {}", if names.is_empty() { "none".to_string() } else { names.join(", ") }));
        Ok::<_, String>(items)
    })
    .await
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Immich
// ---------------------------------------------------------------------------------------------

/// An Immich account (`GET /api/admin/users`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Account {
    pub id: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "deletedAt")]
    pub deleted_at: Option<String>,
}

/// An Immich library (`GET /api/libraries`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Library {
    pub id: String,
    #[serde(rename = "ownerId")]
    pub owner_id: String,
    #[serde(default, rename = "importPaths")]
    pub import_paths: Vec<String>,
}

/// The import path of a user's folder: `<target>/<user>`.
pub fn import_path(target: &str, user: &str) -> String {
    format!("{}/{user}", target.trim_end_matches('/'))
}

/// The account of a NAS user: the one with the user's email when it is set, else the one
/// named like the user or whose email starts with `<user>@` (case does not matter).
pub fn account_of<'a>(user: &str, email: &str, accounts: &'a [Account]) -> Result<&'a Account, PhotosLibrary> {
    let live = || accounts.iter().filter(|a| a.deleted_at.is_none());
    let by_email: Vec<&Account> = if email.trim().is_empty() { Vec::new() } else { live().filter(|a| a.email.eq_ignore_ascii_case(email.trim())).collect() };
    let found: Vec<&Account> = if by_email.is_empty() {
        live()
            .filter(|a| a.name.eq_ignore_ascii_case(user) || a.email.split_once('@').is_some_and(|(local, _)| local.eq_ignore_ascii_case(user)))
            .collect()
    } else {
        by_email
    };
    match found.as_slice() {
        [one] => Ok(one),
        [] => Err(PhotosLibrary::NoAccount),
        _ => Err(PhotosLibrary::Ambiguous),
    }
}

/// What a sync does for one user: the state, and the account a new library goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wiring {
    pub user: String,
    pub state: PhotosLibrary,
    pub owner: Option<String>,
    pub library: Option<String>,
}

/// The wiring of mounted users (`(name, email)`) against Immich's accounts and libraries.
pub fn wiring(users: &[(String, String)], accounts: &[Account], libraries: &[Library], target: &str) -> Vec<Wiring> {
    users
        .iter()
        .map(|(user, email)| {
            let path = import_path(target, user);
            let lib = libraries.iter().find(|l| l.import_paths.iter().any(|p| p.trim_end_matches('/') == path));
            let account = account_of(user, email, accounts);
            let (state, owner) = match (lib, &account) {
                (Some(l), Ok(a)) if l.owner_id == a.id => (PhotosLibrary::Linked, Some(a.id.clone())),
                (Some(_), _) => (PhotosLibrary::OtherOwner, None),
                (None, Ok(a)) => (PhotosLibrary::Missing, Some(a.id.clone())),
                (None, Err(s)) => (*s, None),
            };
            Wiring { user: user.clone(), state, owner, library: lib.map(|l| l.id.clone()) }
        })
        .collect()
}

/// Immich's API with an API key (`x-api-key`). Errors never carry the key.
#[derive(Clone)]
pub struct Immich {
    http: reqwest::Client,
    base: String,
    key: String,
}

impl Immich {
    pub fn new(base: &str, key: &str) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(CALL_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .no_proxy()
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { http, base: base.trim_end_matches('/').to_string(), key: key.to_string() })
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> Result<String, String> {
        let r = rb.header("x-api-key", &self.key).header("Accept", "application/json").send().await.map_err(|e| {
            if e.is_timeout() {
                "Immich did not answer in time".to_string()
            } else if e.is_connect() {
                "Immich does not accept connections (is it running?)".to_string()
            } else {
                "Immich could not be reached".to_string()
            }
        })?;
        let status = r.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Immich refused the API key".into());
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err("the API key is not an administrator's, or lacks a permission (adminUser.read, library.read, library.create, library.update)".into());
        }
        if !status.is_success() {
            return Err(format!("Immich answered {status}"));
        }
        r.text().await.map_err(|_| "Immich's answer was cut short".to_string())
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let t = self.send(self.http.get(format!("{}{path}", self.base))).await?;
        serde_json::from_str(&t).map_err(|e| format!("Immich answered something unexpected: {e}"))
    }

    async fn post(&self, path: &str, body: Option<Value>) -> Result<String, String> {
        let mut rb = self.http.post(format!("{}{path}", self.base));
        if let Some(b) = body {
            rb = rb.header("Content-Type", "application/json").body(b.to_string());
        }
        self.send(rb).await
    }

    pub async fn accounts(&self) -> Result<Vec<Account>, String> {
        self.get("/api/admin/users").await
    }

    pub async fn libraries(&self) -> Result<Vec<Library>, String> {
        self.get("/api/libraries").await
    }

    /// A new external library of `owner` reading only `path`.
    pub async fn create_library(&self, owner: &str, path: &str) -> Result<Library, String> {
        let t = self.post("/api/libraries", Some(json!({ "ownerId": owner, "name": LIBRARY_NAME, "importPaths": [path], "exclusionPatterns": [] }))).await?;
        serde_json::from_str(&t).map_err(|e| format!("Immich answered something unexpected: {e}"))
    }

    /// Queues a scan of a library (new files are imported).
    pub async fn scan(&self, id: &str) -> Result<(), String> {
        if !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return Err("Immich returned an unexpected library id".into());
        }
        self.post(&format!("/api/libraries/{id}/scan"), None).await.map(|_| ())
    }
}

/// The API client of an installed photo app with a key: the entry's web port on loopback.
fn client_of(app: &CatalogApp, env: &BTreeMap<String, String>) -> Option<Result<Immich, String>> {
    let m = &app.manifest;
    let key = env.get(m.user_photos.as_ref()?.key_env.as_ref()?).filter(|k| !k.is_empty())?;
    let port = m.web.as_ref().and_then(|w| m.port(&w.port)).and_then(|p| env.get(&p.env)).and_then(|v| v.parse::<u16>().ok())?;
    Some(Immich::new(&format!("http://127.0.0.1:{port}"), key))
}

// ---------------------------------------------------------------------------------------------
// rescan after uploads
// ---------------------------------------------------------------------------------------------

/// Users with uploads waiting for a scan: `(first, last)` upload of the batch.
#[derive(Debug, Default)]
pub struct Debounce {
    pending: BTreeMap<String, (Instant, Instant)>,
}

impl Debounce {
    pub fn poke(&mut self, user: &str, now: Instant) {
        self.pending.entry(user.to_string()).and_modify(|e| e.1 = now).or_insert((now, now));
    }

    /// The users whose batch is over (quiet for [`QUIET`], or [`MAX_WAIT`] since its first
    /// upload); they leave the queue.
    pub fn due(&mut self, now: Instant) -> Vec<String> {
        let due: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, (first, last))| now.duration_since(*last) >= QUIET || now.duration_since(*first) >= MAX_WAIT)
            .map(|(u, _)| u.clone())
            .collect();
        for u in &due {
            self.pending.remove(u);
        }
        due
    }
}

struct Photos {
    apps: Arc<AppsSvc>,
    queue: Mutex<Debounce>,
}

static PHOTOS: OnceLock<Arc<Photos>> = OnceLock::new();

/// Starts the rescan queue (once, with the app catalogue).
pub fn spawn(apps: Arc<AppsSvc>) {
    let p = Arc::new(Photos { apps, queue: Mutex::new(Debounce::default()) });
    if PHOTOS.set(p.clone()).is_err() {
        return;
    }
    tokio::spawn(async move {
        let mut t = tokio::time::interval(TICK);
        loop {
            t.tick().await;
            let due = p.queue.lock().unwrap().due(Instant::now());
            for user in due {
                rescan(&p.apps, &user).await;
            }
        }
    });
}

/// The files gateway saw `user` upload into `dir` (files API path): an upload into the
/// RyNAS app's folder queues a scan of that user's library.
pub fn uploaded(user: &str, dir: &str) {
    let dir = dir.trim_end_matches('/');
    if dir != APP_DIR && !dir.starts_with(&format!("{APP_DIR}/")) {
        return;
    }
    if let Some(p) = PHOTOS.get() {
        p.queue.lock().unwrap().poke(user, Instant::now());
    }
}

/// Scans the user's library in every photo app with an API key that mounts the user's folder.
async fn rescan(a: &Arc<AppsSvc>, user: &str) {
    let (cat, _) = a.catalog().await;
    for d in a.on_disk() {
        let Some(app) = cat.get(&d.meta.app) else { continue };
        let Some(up) = &app.manifest.user_photos else { continue };
        let env = rynas_apps::render::parse_env(&d.env);
        let mounted = env.get(&up.env).and_then(|v| rynas_apps::render::decode_share_set(v).ok()).unwrap_or_default();
        if !mounted.iter().any(|i| i.name == user) {
            continue;
        }
        let Some(Ok(c)) = client_of(app, &env) else { continue };
        let path = import_path(&up.target, user);
        let r = async {
            let libs = c.libraries().await?;
            match libs.iter().find(|l| l.import_paths.iter().any(|p| p.trim_end_matches('/') == path)) {
                Some(l) => c.scan(&l.id).await.map(|_| true),
                None => Ok(false),
            }
        }
        .await;
        match r {
            Ok(true) => tracing::info!("photos: {}: scan of {user}'s library queued after an upload", d.stack),
            Ok(false) => tracing::debug!("photos: {}: {user} has no library yet", d.stack),
            Err(e) => tracing::warn!("photos: {}: scan of {user}'s library: {e}", d.stack),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------------------------

/// The installed stack and its entry, which must read the users' folders.
async fn photo_app(a: &AppsSvc, stack: &str) -> Res<(Arc<rynas_apps::Catalog>, Layout, OnDisk)> {
    let (cat, _) = a.catalog().await;
    let (l, d) = a.one_on_disk(stack)?;
    let app = cat.get(&d.meta.app).ok_or_else(|| Error::not_found(format!("the app {} of stack {stack} is no longer in the catalog", d.meta.app)))?;
    if app.manifest.user_photos.is_none() {
        return Err(conflict(format!("{} does not read the users' Photos folders", app.manifest.name)));
    }
    Ok((cat, l, d))
}

/// The email of each NAS user in the applied `users` configuration (empty: none).
fn emails(a: &AppsSvc) -> BTreeMap<String, String> {
    a.docker
        .ops
        .config
        .applied(rynas_services::USERS_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value::<rynas_api_types::UsersConfig>(v).ok())
        .map(|c| c.users.into_iter().map(|u| (u.name, u.email)).collect())
        .unwrap_or_default()
}

fn passwd(a: &AppsSvc) -> String {
    std::fs::read_to_string(rynas_docker::under(&a.docker.root, "/etc/passwd")).unwrap_or_default()
}

/// `GET /v1/apps/installed/{stack}/photos`.
async fn state_get(State(st): State<AppsState>, hd: HeaderMap, UrlPath(stack): UrlPath<String>) -> Res<Json<PhotosState>> {
    let a = asvc(&st)?;
    admin(&a, &hd)?;
    let (cat, _, d) = photo_app(&a, &stack).await?;
    let app = cat.get(&d.meta.app).ok_or_else(|| Error::not_found("no such app"))?;
    let up = app.manifest.user_photos.clone().ok_or_else(|| Error::internal("no user_photos"))?;
    let env = rynas_apps::render::parse_env(&d.env);
    let mounted = env.get(&up.env).and_then(|v| rynas_apps::render::decode_share_set(v).ok()).unwrap_or_default();
    let (root, pw) = (a.docker.root.clone(), passwd(&a));
    let users = blocking(move || {
        let mut rows: Vec<PhotosUser> = nas_users(&pw)
            .iter()
            .map(|u| {
                let folder = ensure_folder(&root, u, false).unwrap_or_else(|_| format!("{}/{FOLDER}", u.home));
                let mounted = mounted.iter().any(|i| i.name == u.name && i.path == folder);
                PhotosUser { user: u.name.clone(), folder, mounted, library: None }
            })
            .collect();
        // mounted for a user who is no longer a NAS user: shown until the next sync
        for i in &mounted {
            if !rows.iter().any(|r| r.user == i.name) {
                rows.push(PhotosUser { user: i.name.clone(), folder: i.path.clone(), mounted: true, library: None });
            }
        }
        Ok::<_, String>(rows)
    })
    .await
    .map_err(Error::internal)?;
    let mut out = PhotosState { users, api: None, allow_list: (d.meta.app == "pigallery2").then(|| ALLOW_LIST.to_string()) };
    if up.key_env.is_some() {
        let mut api = PhotosApi::default();
        if let Some(c) = client_of(app, &env) {
            api.key = true;
            let r = async {
                let c = c?;
                Ok::<_, String>((c.accounts().await?, c.libraries().await?))
            }
            .await;
            match r {
                Ok((accounts, libs)) => {
                    let mails = emails(&a);
                    let who: Vec<(String, String)> = out.users.iter().map(|u| (u.user.clone(), mails.get(&u.user).cloned().unwrap_or_default())).collect();
                    for (row, w) in out.users.iter_mut().zip(wiring(&who, &accounts, &libs, &up.target)) {
                        row.library = Some(w.state);
                    }
                }
                Err(e) => api.error = Some(e),
            }
        }
        out.api = Some(api);
    }
    Ok(Json(out))
}

/// `POST /v1/apps/installed/{stack}/photos/sync`: task `apps.photos-sync`.
async fn sync_post(State(st): State<AppsState>, hd: HeaderMap, UrlPath(stack): UrlPath<String>) -> Res<Response> {
    let a = asvc(&st)?;
    let who = admin(&a, &hd)?;
    photo_app(&a, &stack).await?;
    let guard = mark_busy(&a.docker, &stack)?;
    audit(&who, "photos-sync", &stack);
    let a2 = a.clone();
    let id = a.docker.ops.tasks.spawn("apps.photos-sync", &who, move |ctx| async move {
        let _guard = guard;
        sync_task(&a2, &ctx, &stack).await
    })?;
    Ok(accepted(id))
}

/// Mounts the users' folders of now (the stack goes up again only when the list changed),
/// then, with an API key, creates the missing libraries and scans them.
pub(crate) async fn sync_task(a: &Arc<AppsSvc>, ctx: &TaskCtx, stack: &str) -> Result<(), String> {
    let (cat, l, d) = photo_app(a, stack).await.map_err(|e| e.2)?;
    let app = cat.get(&d.meta.app).ok_or("the app left the catalog")?;
    let m = &app.manifest;
    let up = m.user_photos.clone().ok_or("no user_photos")?;
    let items = prepare_folders(a, ctx).await;
    ctx.progress(30);
    let env = rynas_apps::render::parse_env(&d.env);
    let before = env.get(&up.env).and_then(|v| rynas_apps::render::decode_share_set(v).ok()).unwrap_or_default();
    if before != items {
        if d.meta.version != m.version {
            return Err(format!("{stack} is {} {} and the catalog has {}: update it first", m.name, d.meta.version, m.version));
        }
        let ids = a.stack_ids(&env).map_err(|e| e.2)?;
        let bind = env.get("RYNAS_BIND").cloned().unwrap_or_else(|| "127.0.0.1".into());
        let provided = a.provided(&l, stack, &bind, ids);
        let mut req = rynas_apps::render::request_from_env(app, stack, &env);
        req.user_photos = items.clone();
        let r = rynas_apps::render::render(app, &cat.source, &provided, &req).map_err(|e| render_err(e).2)?;
        let (old, new) = ((d.compose.clone(), d.env.clone()), (file_text(&r, COMPOSE_FILE), file_text(&r, ".env")));
        ctx.log(&format!("{stack}: mounting the Photos folders again (the app restarts)"));
        let r = async {
            write_stack(&a.docker, &l, stack, &new).await?;
            compose_run(&a.docker, ctx, stack, &l, ComposeAction::Up).await
        }
        .await;
        if let Err(e) = r {
            ctx.log(&format!("{e}: putting the previous compose.yaml and .env back"));
            write_stack(&a.docker, &l, stack, &old).await?;
            compose_run(&a.docker, ctx, stack, &l, ComposeAction::Up).await?;
            return Err(e);
        }
        a.docker.changed();
    } else {
        ctx.log("the mounted Photos folders are up to date");
    }
    ctx.progress(70);
    let Some(client) = client_of(app, &env) else {
        if up.key_env.is_some() {
            ctx.log("no Immich API key: the folders are mounted, the libraries are not made (add a key on the app's page)");
        }
        ctx.progress(100);
        return Ok(());
    };
    let c = client?;
    // the app may still be starting after the restart
    let mut accounts = Err(String::new());
    for _ in 0..12 {
        accounts = c.accounts().await;
        if accounts.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    let accounts = accounts?;
    let libs = c.libraries().await?;
    let mails = emails(a);
    let who: Vec<(String, String)> = items.iter().map(|i| (i.name.clone(), mails.get(&i.name).cloned().unwrap_or_default())).collect();
    for w in wiring(&who, &accounts, &libs, &up.target) {
        let path = import_path(&up.target, &w.user);
        match (w.state, &w.owner) {
            (PhotosLibrary::Missing, Some(owner)) => {
                let lib = c.create_library(owner, &path).await?;
                ctx.log(&format!("{}: library created for the user's Immich account, reading {path}", w.user));
                c.scan(&lib.id).await?;
            }
            (PhotosLibrary::Linked, _) => ctx.log(&format!("{}: library already there", w.user)),
            (PhotosLibrary::NoAccount, _) => ctx.log(&format!("{}: no Immich account with the user's email or name: no library", w.user)),
            (PhotosLibrary::Ambiguous, _) => ctx.log(&format!("{}: several Immich accounts match: no library (give the user an email in rynas)", w.user)),
            (PhotosLibrary::OtherOwner, _) => ctx.log(&format!("{}: a library of another account reads {path}: left as it is", w.user)),
            (PhotosLibrary::Missing, None) => {}
        }
    }
    ctx.progress(100);
    Ok(())
}

/// `POST /v1/apps/installed/{stack}/photos/key`: checked against the app, kept in `.env`.
async fn key_post(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath(stack): UrlPath<String>,
    body: Result<Json<PhotosKeyRequest>, JsonRejection>,
) -> Res<axum::http::StatusCode> {
    let a = asvc(&st)?;
    let who = admin(&a, &hd)?;
    let Json(req) = body?;
    let key = req.key.trim().to_string();
    let (cat, l, d) = photo_app(&a, &stack).await?;
    let app = cat.get(&d.meta.app).ok_or_else(|| Error::not_found("no such app"))?;
    let Some(key_env) = app.manifest.user_photos.as_ref().and_then(|u| u.key_env.clone()) else {
        return Err(conflict(format!("{} takes no API key", app.manifest.name)));
    };
    if !key.is_empty() && !rynas_apps::render::valid_api_key(&key) {
        return Err(Error::invalid("key", "this is not an API key"));
    }
    let new_env = set_env_line(&d.env, &key_env, &key);
    if !key.is_empty() {
        let env = rynas_apps::render::parse_env(&new_env);
        let c = client_of(app, &env).ok_or_else(|| Error::invalid("key", "the app has no web port"))?.map_err(Error::internal)?;
        c.accounts().await.map_err(|e| Error::invalid("key", e))?;
        c.libraries().await.map_err(|e| Error::invalid("key", e))?;
    }
    let _guard = mark_busy(&a.docker, &stack)?;
    write_stack(&a.docker, &l, &stack, &(d.compose.clone(), new_env)).await.map_err(Error::internal)?;
    audit(&who, if key.is_empty() { "photos-key-removed" } else { "photos-key-set" }, &stack);
    Ok(axum::http::StatusCode::NO_CONTENT)
}

pub fn router(st: AppsState) -> Router {
    Router::new()
        .route("/v1/apps/installed/{stack}/photos", get(state_get))
        .route("/v1/apps/installed/{stack}/photos/sync", post(sync_post))
        .route("/v1/apps/installed/{stack}/photos/key", post(key_post))
        .with_state(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(id: &str, email: &str, name: &str) -> Account {
        Account { id: id.into(), email: email.into(), name: name.into(), deleted_at: None }
    }

    #[test]
    fn nas_users_with_a_home() {
        let pw = "root:x:0:0:root:/root:/bin/bash\n\
                  docker-apps:x:1500:1500::/nonexistent:/usr/sbin/nologin\n\
                  bob:x:1001:100:Bob:/srv/rynas/homes/bob:/bin/bash\n\
                  ann:x:1000:100:Ann:/home/ann/:/bin/bash\n\
                  daemon:x:1:1::/usr/sbin:/usr/sbin/nologin\n\
                  nohome:x:1002:100::relative:/bin/sh\n";
        let u = nas_users(pw);
        assert_eq!(u.iter().map(|u| (u.name.as_str(), u.home.as_str())).collect::<Vec<_>>(), [("ann", "/home/ann"), ("bob", "/srv/rynas/homes/bob")]);
        assert_eq!((u[1].uid, u[1].gid), (1001, 100));
    }

    #[test]
    fn folders_follow_the_homes_links_but_never_a_photos_link() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let me = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        // ann's home is on a volume, reached through rynas's link
        std::fs::create_dir_all(root.join("vol1/homes/ann")).unwrap();
        std::fs::create_dir_all(root.join("srv/rynas/homes")).unwrap();
        std::os::unix::fs::symlink(root.join("vol1/homes/ann"), root.join("srv/rynas/homes/ann")).unwrap();
        let ann = NasUser { name: "ann".into(), uid: me, gid, home: "/srv/rynas/homes/ann".into() };
        // not made without create
        assert_eq!(ensure_folder(root, &ann, false).unwrap(), "/vol1/homes/ann/Photos");
        assert!(!root.join("vol1/homes/ann/Photos").exists());
        let (items, problems) = folder_items(root, std::slice::from_ref(&ann), false);
        assert!(items.is_empty() && problems.is_empty());
        // made with create: the user's, 0700
        assert_eq!(ensure_folder(root, &ann, true).unwrap(), "/vol1/homes/ann/Photos");
        let m = std::fs::metadata(root.join("vol1/homes/ann/Photos")).unwrap();
        assert_eq!((m.uid(), m.mode() & 0o777), (me, 0o700));
        // bob made Photos a link to somewhere else: refused
        std::fs::create_dir_all(root.join("home/bob")).unwrap();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::os::unix::fs::symlink(root.join("etc"), root.join("home/bob/Photos")).unwrap();
        let bob = NasUser { name: "bob".into(), uid: me, gid, home: "/home/bob".into() };
        assert!(ensure_folder(root, &bob, true).unwrap_err().contains("is a link"));
        // a file named Photos, and a home that is someone else's
        std::fs::create_dir_all(root.join("home/cid")).unwrap();
        std::fs::write(root.join("home/cid/Photos"), "x").unwrap();
        let cid = NasUser { name: "cid".into(), uid: me, gid, home: "/home/cid".into() };
        assert!(ensure_folder(root, &cid, true).unwrap_err().contains("not a folder"));
        let dan = NasUser { name: "dan".into(), uid: me + 1, gid, home: "/home/cid".into() };
        assert!(ensure_folder(root, &dan, true).unwrap_err().contains("not a folder of the user"));
        let eve = NasUser { name: "eve".into(), uid: me, gid, home: "/home/eve".into() };
        assert!(ensure_folder(root, &eve, true).unwrap_err().contains("does not exist"));
        let (items, problems) = folder_items(root, &[ann, bob, cid, dan, eve], true);
        assert_eq!(items, [ShareSetItem { name: "ann".into(), path: "/vol1/homes/ann/Photos".into(), follow: false }]);
        assert_eq!(problems.len(), 4);
    }

    #[test]
    fn accounts_match_by_email_then_name() {
        let accounts = [acc("1", "ann@example.com", "Ann"), acc("2", "bob@example.com", "Robert"), acc("3", "x@example.com", "cid"), acc("4", "cid@example.org", "C"), Account { deleted_at: Some("2026-01-01".into()), ..acc("5", "dan@example.com", "dan") }];
        assert_eq!(account_of("ann", "", &accounts).unwrap().id, "1");
        assert_eq!(account_of("robert", "BOB@example.com", &accounts).unwrap().id, "2");
        // the email decides when it is set, even against a name
        assert_eq!(account_of("ann", "bob@example.com", &accounts).unwrap().id, "2");
        // an email nobody has: the name still counts
        assert_eq!(account_of("ann", "new@example.com", &accounts).unwrap().id, "1");
        assert_eq!(account_of("cid", "", &accounts), Err(PhotosLibrary::Ambiguous));
        assert_eq!(account_of("dan", "", &accounts), Err(PhotosLibrary::NoAccount), "deleted accounts never");
        assert_eq!(account_of("eve", "", &accounts), Err(PhotosLibrary::NoAccount));
    }

    #[test]
    fn wiring_states() {
        let accounts = [acc("a1", "ann@example.com", "ann"), acc("b1", "bob@example.com", "bob"), acc("c1", "cid@example.com", "cid")];
        let libs = [
            Library { id: "L1".into(), owner_id: "a1".into(), import_paths: vec!["/mnt/photos/ann/".into()] },
            Library { id: "L2".into(), owner_id: "a1".into(), import_paths: vec!["/mnt/photos/cid".into(), "/mnt/external".into()] },
        ];
        let users: Vec<(String, String)> = ["ann", "bob", "cid", "eve"].iter().map(|u| (u.to_string(), String::new())).collect();
        let w = wiring(&users, &accounts, &libs, "/mnt/photos/");
        let got: Vec<(&str, PhotosLibrary, Option<&str>, Option<&str>)> = w.iter().map(|w| (w.user.as_str(), w.state, w.owner.as_deref(), w.library.as_deref())).collect();
        assert_eq!(
            got,
            [
                ("ann", PhotosLibrary::Linked, Some("a1"), Some("L1")),
                ("bob", PhotosLibrary::Missing, Some("b1"), None),
                ("cid", PhotosLibrary::OtherOwner, None, Some("L2")),
                ("eve", PhotosLibrary::NoAccount, None, None),
            ]
        );
    }

    #[test]
    fn debounce_waits_for_quiet_or_the_longest_wait() {
        let t0 = Instant::now();
        let mut d = Debounce::default();
        d.poke("ann", t0);
        d.poke("ann", t0 + Duration::from_secs(30));
        assert!(d.due(t0 + Duration::from_secs(60)).is_empty(), "the batch goes on");
        assert_eq!(d.due(t0 + Duration::from_secs(30) + QUIET), ["ann"]);
        assert!(d.due(t0 + Duration::from_secs(1000)).is_empty(), "left the queue");
        // a steady stream: scanned at the longest wait anyway
        let mut d = Debounce::default();
        let mut t = t0;
        while t < t0 + MAX_WAIT {
            d.poke("bob", t);
            assert!(d.due(t).is_empty());
            t += Duration::from_secs(20);
        }
        d.poke("bob", t);
        assert_eq!(d.due(t), ["bob"]);
    }

    #[test]
    fn only_uploads_into_the_app_folder_count() {
        for (dir, yes) in [("/home/Photos", true), ("/home/Photos/2026/11", true), ("/home/Photos/", true), ("/home/Photoshop", false), ("/home", false), ("/Photos/2026", false)] {
            let d = dir.trim_end_matches('/');
            assert_eq!(d == APP_DIR || d.starts_with(&format!("{APP_DIR}/")), yes, "{dir}");
        }
    }

    // -- the API client against a fake Immich ---------------------------------------------

    #[derive(Default)]
    struct Fake {
        libraries: Vec<Value>,
        log: Vec<String>,
    }
    type FakeIm = Arc<Mutex<Fake>>;

    fn key_ok(hd: &HeaderMap) -> bool {
        hd.get("x-api-key").and_then(|v| v.to_str().ok()) == Some("k3y")
    }

    async fn serve(f: FakeIm) -> String {
        use axum::http::StatusCode;
        async fn users(hd: HeaderMap) -> Result<Json<Value>, StatusCode> {
            if !key_ok(&hd) {
                return Err(StatusCode::UNAUTHORIZED);
            }
            Ok(Json(json!([
                {"id": "u-ann", "email": "ann@example.com", "name": "Ann", "deletedAt": null, "isAdmin": true},
                {"id": "u-bob", "email": "robert@example.com", "name": "bob", "deletedAt": null, "isAdmin": false}
            ])))
        }
        async fn libs(State(f): State<FakeIm>, hd: HeaderMap) -> Result<Json<Value>, StatusCode> {
            if !key_ok(&hd) {
                return Err(StatusCode::UNAUTHORIZED);
            }
            Ok(Json(Value::Array(f.lock().unwrap().libraries.clone())))
        }
        async fn create(State(f): State<FakeIm>, hd: HeaderMap, Json(b): Json<Value>) -> Result<(StatusCode, Json<Value>), StatusCode> {
            if !key_ok(&hd) {
                return Err(StatusCode::UNAUTHORIZED);
            }
            let mut g = f.lock().unwrap();
            let id = format!("lib-{}", g.libraries.len() + 1);
            let lib = json!({"id": id, "ownerId": b["ownerId"], "name": b["name"], "importPaths": b["importPaths"], "exclusionPatterns": [], "assetCount": 0});
            g.log.push(format!("create {} {}", b["ownerId"].as_str().unwrap(), b["importPaths"][0].as_str().unwrap()));
            g.libraries.push(lib.clone());
            Ok((StatusCode::CREATED, Json(lib)))
        }
        async fn scan(State(f): State<FakeIm>, hd: HeaderMap, UrlPath(id): UrlPath<String>) -> StatusCode {
            if !key_ok(&hd) {
                return StatusCode::UNAUTHORIZED;
            }
            f.lock().unwrap().log.push(format!("scan {id}"));
            StatusCode::NO_CONTENT
        }
        let app = Router::new()
            .route("/api/admin/users", get(users))
            .route("/api/libraries", get(libs).post(create))
            .route("/api/libraries/{id}/scan", post(scan))
            .with_state(f);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn client_creates_and_scans_libraries() {
        let f: FakeIm = Arc::default();
        let base = serve(f.clone()).await;
        let c = Immich::new(&base, "k3y").unwrap();
        let accounts = c.accounts().await.unwrap();
        assert_eq!(accounts.len(), 2);
        let users = vec![("ann".to_string(), String::new()), ("bob".to_string(), String::new())];
        for w in wiring(&users, &accounts, &c.libraries().await.unwrap(), "/mnt/photos") {
            assert_eq!(w.state, PhotosLibrary::Missing);
            let lib = c.create_library(w.owner.as_deref().unwrap(), &import_path("/mnt/photos", &w.user)).await.unwrap();
            c.scan(&lib.id).await.unwrap();
        }
        assert_eq!(f.lock().unwrap().log, ["create u-ann /mnt/photos/ann", "scan lib-1", "create u-bob /mnt/photos/bob", "scan lib-2"]);
        // a second sync finds them
        let w = wiring(&users, &accounts, &c.libraries().await.unwrap(), "/mnt/photos");
        assert!(w.iter().all(|w| w.state == PhotosLibrary::Linked), "{w:?}");
        // a wrong key: a plain message, never the key
        let bad = Immich::new(&base, "s3cret-wrong").unwrap();
        let e = bad.accounts().await.unwrap_err();
        assert_eq!(e, "Immich refused the API key");
        // nothing listening
        let gone = Immich::new("http://127.0.0.1:9", "k3y").unwrap();
        assert!(!gone.libraries().await.unwrap_err().contains("k3y"));
        // a library id that could change the path is refused before any request
        assert!(c.scan("../admin").await.is_err());
    }
}
