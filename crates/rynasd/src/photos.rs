//! Photos from the RyNAS app (D-PHOTOS, `docs/phase2/photos.md`).
//!
//! The phone app's photo backup is the single way photos come in: each NAS user has one
//! folder, `Photos` in their home (`<home>/Photos/YYYY/MM`, the files API path
//! [`APP_DIR`]). The catalogue's photo apps read those folders read-only and rynas wires them:
//!
//! - **Who**: an administrator chooses the users of each app on its page; nobody is shown at
//!   install. An app with an API (Immich) needs the user's account in the app confirmed once
//!   (rynas proposes one: same email, else same name); the choice is kept in
//!   `<state>/docker/photos/<stack>.json` and never made again by itself.
//! - **Mounts**: the stack binds one root-owned staging folder,
//!   `<state>/docker/photos/<stack>`, read-only and `rslave` at `[user_photos].target`.
//!   rynasd mounts each chosen user's folder inside it (`rynasd photos-mount` in a transient
//!   unit, [`crate::photos_mount`]: from a descriptor opened without following links, read-only,
//!   nosuid, nodev, noexec), so no path a user can change is ever bound; the mounts appear and
//!   go in the running app. They are made again when rynasd starts ([`spawn`]). A user who
//!   leaves passwd loses the mount (logged); a failed check (folder missing, a link, a volume
//!   not mounted yet) keeps what is mounted.
//! - **Immich**: with an API key the administrator gives rynas (kept in the stack's `.env`
//!   like every app secret, never logged nor returned), "Sync" creates for each confirmed user
//!   an external library owned by the confirmed account whose only import path is the user's
//!   folder, then queues its scan. A library of another account on that path keeps the user
//!   unmounted until an administrator resolves it in Immich. Libraries are never deleted nor
//!   changed.
//! - **Rescan**: a finished upload into [`APP_DIR`] ([`uploaded`], from the files gateway)
//!   queues that user; after [`QUIET`] without another upload (at most [`MAX_WAIT`] after the
//!   first) rynas asks Immich to scan the library of the confirmed account on the user's
//!   path, and no other.
//! - **PiGallery2** has no API key: the chosen users' folders are mounted; per-user directory
//!   allow lists are set in the gallery by hand (the page shows the values, [`ALLOW_LIST`]).

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::apps::{PhotosAccount, PhotosApi, PhotosKeyRequest, PhotosLibrary, PhotosState, PhotosUser, PhotosUserRequest};
use rynas_apps::CatalogApp;
use rynas_docker::Layout;
use rynas_docker::stacks::{COMPOSE_FILE, ComposeAction};
use rynas_tasks::TaskCtx;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::apps::{AppsState, AppsSvc, OnDisk, admin, asvc, blocking, compose_run, file_text, render_err, set_env_line, write_stack};
use crate::docker::{DockerSvc, accepted, conflict, mark_busy};
use crate::ops::{Error, Res};
use crate::photos_mount::FOLDER;

/// That folder in the files API (the user's home is the share `home`).
pub const APP_DIR: &str = "/home/Photos";
/// A user's library is scanned once no upload came for this long…
pub const QUIET: Duration = Duration::from_secs(45);
/// …and at the latest this long after the first upload of a batch.
pub const MAX_WAIT: Duration = Duration::from_secs(600);
/// Name of the libraries rynas creates.
pub const LIBRARY_NAME: &str = "Photos from the RyNAS app";
/// PiGallery2's directory allow list of a user (`{user}` replaced by the UI).
pub const ALLOW_LIST: &str = "/{user}/*";
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
const HELPER_TIMEOUT: Duration = Duration::from_secs(60);

fn audit(who: &str, action: &str, target: &str) {
    tracing::info!(target: "rynas::audit", "apps {action} {target} by {who}");
}

// ---------------------------------------------------------------------------------------------
// users and the stack's choices
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

/// The user's real home below `root`: the passwd home with the links of its parents resolved
/// (root's: `/srv/rynas/homes/<user>` points at the folder on a volume).
pub fn real_home(root: &Path, u: &NasUser) -> Result<PathBuf, String> {
    rynas_docker::under(root, &u.home).canonicalize().map_err(|_| format!("the home {} cannot be reached", u.home))
}

/// A chosen user of a stack.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Member {
    /// The app account an administrator confirmed (apps with an API).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Why the folder is kept unmounted (a library of another account on its path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

/// The chosen users of a stack (`<state>/docker/photos/<stack>.json`, root 0600).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub users: BTreeMap<String, Member>,
    /// The app needs a confirmed account per user (it has an API): the boot unit mounts
    /// only confirmed users then.
    #[serde(default)]
    pub api: bool,
    /// The stack folder when the choices last changed: the boot unit mounts nothing for a
    /// stack whose compose file is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_dir: Option<String>,
}

fn photos_dir(d: &DockerSvc) -> PathBuf {
    d.state_dir.join("photos")
}

/// The staging folder of a stack.
pub fn staging_root(d: &DockerSvc, stack: &str) -> PathBuf {
    photos_dir(d).join(stack)
}

fn store_path(d: &DockerSvc, stack: &str) -> PathBuf {
    photos_dir(d).join(format!("{stack}.json"))
}

pub fn load_store(path: &Path) -> Store {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_store(path: &Path, s: &Store) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Makes the staging folder (0755, rynasd's: root) with a folder per chosen user and the
/// entry's mount points; a folder that is a link is refused.
pub fn ensure_staging<'a>(root: &Path, names: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    ensure_staging_in(&mountinfo(), root, names)
}

/// [`ensure_staging`] with the mount table given: a folder that is a mount point (a user's
/// folder mounted there) is left as it is (it shows the user's folder, read-only).
pub fn ensure_staging_in<'a>(mountinfo: &str, root: &Path, names: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    for (dir, what) in std::iter::once((root.to_path_buf(), "staging")).chain(names.into_iter().map(|n| (root.join(n), n))) {
        if crate::photos_mount::is_mount_point(mountinfo, &dir) {
            continue;
        }
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            Ok(_) => return Err(format!("{} ({what}) is not a folder", dir.display())),
            Err(_) => {
                std::fs::DirBuilder::new().recursive(true).mode(0o755).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    Ok(())
}

/// The users mounted now in a staging folder (`mountinfo`: the text of
/// `/proc/self/mountinfo`).
pub fn mounted_users(mountinfo: &str, root: &Path) -> BTreeSet<String> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| rynas_apps::manifest::is_user_name(n) && crate::photos_mount::is_mount_point(mountinfo, &root.join(n)))
        .collect()
}

fn mountinfo() -> String {
    std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default()
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

impl Account {
    fn view(&self) -> PhotosAccount {
        PhotosAccount { id: self.id.clone(), email: self.email.clone(), name: self.name.clone() }
    }
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

fn reads(l: &Library, path: &str) -> bool {
    l.import_paths.iter().any(|p| p.trim_end_matches('/') == path)
}

/// The account rynas proposes for a NAS user (never used before an administrator confirms
/// it): the one with the user's email when it is set, else the one named like the user;
/// several matches propose none. Deleted accounts never.
pub fn propose<'a>(user: &str, email: &str, accounts: &'a [Account]) -> Option<&'a Account> {
    let live = || accounts.iter().filter(|a| a.deleted_at.is_none());
    let one = |v: Vec<&'a Account>| if v.len() == 1 { v.first().copied() } else { None };
    if !email.trim().is_empty()
        && let Some(a) = one(live().filter(|a| a.email.eq_ignore_ascii_case(email.trim())).collect())
    {
        return Some(a);
    }
    one(live().filter(|a| a.name.eq_ignore_ascii_case(user)).collect())
}

/// Where a confirmed user's folder stands in Immich.
pub fn library_state(user: &str, account: Option<&str>, accounts: &[Account], libraries: &[Library], target: &str) -> PhotosLibrary {
    let Some(account) = account else { return PhotosLibrary::Unconfirmed };
    let path = import_path(target, user);
    let on_path: Vec<&Library> = libraries.iter().filter(|l| reads(l, &path)).collect();
    if on_path.iter().any(|l| l.owner_id != account) {
        // any other account on the path, even next to the user's own library
        return PhotosLibrary::OtherOwner;
    }
    match on_path.first() {
        Some(_) => PhotosLibrary::Linked,
        None if !accounts.iter().any(|a| a.id == account && a.deleted_at.is_none()) => PhotosLibrary::NoAccount,
        None => PhotosLibrary::Missing,
    }
}

/// The library a rescan asks for: the one on the user's path owned by the confirmed account.
pub fn scan_target<'a>(libraries: &'a [Library], path: &str, account: &str) -> Option<&'a Library> {
    libraries.iter().find(|l| l.owner_id == account && reads(l, path))
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

    /// Accounts and libraries together.
    pub async fn view(&self) -> Result<(Vec<Account>, Vec<Library>), String> {
        Ok((self.accounts().await?, self.libraries().await?))
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
// what a sync does
// ---------------------------------------------------------------------------------------------

/// One user's step of a sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Mount (or keep) the user's folder.
    Mount,
    /// Take the mount away, with the reason (left out by an administrator, blocked).
    Unmount(String),
    /// No longer a NAS user: unmounted and forgotten.
    Forget,
    /// Not mounted, with the reason (no account confirmed).
    Hold(String),
}

/// The steps of a sync and the choices kept afterwards. `immich`: the app's accounts and
/// libraries when it has an API and answered (`None`: no API, or it did not answer: then the
/// blocks found before stay as they are).
pub fn plan(store: &Store, users: &[NasUser], mounted: &BTreeSet<String>, api: bool, immich: Option<(&[Account], &[Library])>, target: &str) -> (Vec<(String, Step)>, Store) {
    let mut kept = Store { api, ..Default::default() };
    let mut steps = Vec::new();
    for (name, m) in &store.users {
        if !users.iter().any(|u| &u.name == name) {
            steps.push((name.clone(), Step::Forget));
            continue;
        }
        let mut m = m.clone();
        let step = match (api, &m.account, immich) {
            (true, None, _) => Step::Hold("no account confirmed".into()),
            (true, Some(acc), Some((accounts, libs))) => match library_state(name, Some(acc), accounts, libs, target) {
                PhotosLibrary::OtherOwner => {
                    let why = format!("a library of another account reads {}: resolve it in the app", import_path(target, name));
                    m.blocked = Some(why.clone());
                    Step::Unmount(why)
                }
                _ => {
                    m.blocked = None;
                    Step::Mount
                }
            },
            (true, Some(_), None) => match &m.blocked {
                Some(why) => Step::Unmount(why.clone()),
                None => Step::Mount,
            },
            (false, _, _) => Step::Mount,
        };
        steps.push((name.clone(), step));
        kept.users.insert(name.clone(), m);
    }
    for name in mounted.iter().filter(|n| !store.users.contains_key(*n)) {
        steps.push((name.clone(), Step::Unmount("left out by an administrator".into())));
    }
    (steps, kept)
}

/// `rynasd photos-mount|photos-unmount` in a transient unit, outside rynasd's sandbox.
async fn helper(d: &Arc<DockerSvc>, args: Vec<String>) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let argv: Vec<String> = std::iter::once(exe.to_string_lossy().into_owned()).chain(args).collect();
    let cmd = rynas_tasks::Outside::new("rynas-photos", "rynas: Photos folder of a user (apps)", HELPER_TIMEOUT).cmd(argv);
    let runner = d.runner.clone();
    blocking(move || {
        let out = runner.run(&cmd)?;
        if out.success() { Ok(out.stdout.trim().to_string()) } else { Err(out.stderr.lines().last().unwrap_or("failed").trim().to_string()) }
    })
    .await
}

/// Mounts one user's folder (and lets the apps' account read it); `Err`: why it is not
/// mounted, or why the mount there stays as it was.
async fn mount_user(d: &Arc<DockerSvc>, root: &Path, u: &NasUser, gid: Option<u32>) -> Result<String, String> {
    let home = real_home(&d.root, u)?;
    let target = root.join(&u.name);
    ensure_staging(root, [u.name.as_str()])?;
    if d.dry {
        return Ok("dry system: not mounted".into());
    }
    let out = helper(d, vec!["photos-mount".into(), "--home".into(), home.to_string_lossy().into_owned(), "--uid".into(), u.uid.to_string(), "--target".into(), target.to_string_lossy().into_owned()]).await?;
    // the apps' account reads the folder (apps that run as docker-apps): only the user's
    // files, never a hard-linked one, from the descriptor checked like the mount's
    if let Some(gid) = gid {
        let uid = u.uid;
        let r = blocking(move || open_to_apps(&rynas_services::idremap::HostRemap, &home, uid, gid)).await;
        match r {
            Ok(c) if c.errors > 0 => tracing::warn!("photos: {}: {} entries not opened to the apps' account: {}", u.name, c.errors, c.error_samples.join("; ")),
            Ok(_) => {}
            Err(e) => tracing::warn!("photos: {}: not opened to the apps' account: {e}", u.name),
        }
    }
    Ok(out)
}

/// Lets the apps' group read the user's `Photos` folder: only the user's files and folders,
/// never a hard-linked file, walked from the descriptor checked like the mount's.
pub fn open_to_apps(fs: &dyn rynas_services::idremap::RemapFs, home: &Path, uid: u32, gid: u32) -> Result<rynas_services::idremap::RemapCounts, String> {
    use rynas_services::idremap::{GroupGrant, grant_fd};
    let fd = crate::photos_mount::open_photos(home, uid)?;
    let label = home.join(FOLDER);
    Ok(grant_fd(fs, std::os::fd::AsFd::as_fd(&fd), &label, GroupGrant { gid, user: false, write: false, owner: Some(uid) }, &|| false))
}

async fn unmount_user(d: &Arc<DockerSvc>, root: &Path, user: &str) -> Result<(), String> {
    let target = root.join(user);
    if !d.dry {
        helper(d, vec!["photos-unmount".into(), "--target".into(), target.to_string_lossy().into_owned()]).await?;
    }
    let _ = std::fs::remove_dir(&target);
    Ok(())
}

/// Unmounts every user of a stack's staging folder (uninstall); the folder and the choices go
/// too, so nothing is ever mounted again for a stack that is not installed.
pub(crate) async fn release(d: &Arc<DockerSvc>, ctx: &TaskCtx, stack: &str) {
    let _ = std::fs::remove_file(store_path(d, stack));
    let root = staging_root(d, stack);
    if !root.exists() {
        return;
    }
    for user in mounted_users(&mountinfo(), &root) {
        match unmount_user(d, &root, &user).await {
            Ok(()) => ctx.log(&format!("{user}'s Photos folder unmounted from {stack}")),
            Err(e) => ctx.log(&format!("warning: {user}'s Photos folder: {e}")),
        }
    }
    // only empty folders: never into a mount that is still there
    for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        let _ = std::fs::remove_dir(e.path());
    }
    let _ = std::fs::remove_dir(&root);
}

/// Whether the stack of a choices file is installed: its compose file is where the last
/// change saw it (below `root`).
pub fn installed(store: &Store, root: &Path) -> bool {
    store.stack_dir.as_deref().is_some_and(|d| rynas_docker::under(root, d).join(COMPOSE_FILE).is_file())
}

/// The staging folder an install binds (made now, with the entry's mount points).
pub(crate) fn prepare_staging(d: &DockerSvc, app: &CatalogApp, stack: &str) -> Result<String, String> {
    let root = staging_root(d, stack);
    let mps = app.manifest.user_photos.as_ref().map(|u| u.mountpoints.clone()).unwrap_or_default();
    ensure_staging(&root, mps.iter().map(String::as_str))?;
    Ok(root.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------------------------
// rescan after uploads, mounts at start
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

    /// When the next batch is due (`None`: nothing queued, nothing to wait for).
    pub fn next_due(&self) -> Option<Instant> {
        self.pending.values().map(|(first, last)| (*last + QUIET).min(*first + MAX_WAIT)).min()
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
    /// An upload was queued: the rescan worker wakes (it sleeps while the queue is empty).
    queued: tokio::sync::Notify,
}

static PHOTOS: OnceLock<Arc<Photos>> = OnceLock::new();

/// How long mount table changes are gathered before the chosen folders are checked.
const MOUNTS_SETTLE: Duration = Duration::from_secs(3);

/// Starts the rescan worker and the mount watcher, and mounts the chosen users' folders again
/// (once, with the app catalogue; the apps' binds follow the mounts made after they started).
/// Nothing runs on a timer: the worker sleeps until an upload is queued and then only until
/// the batch is due; the watcher sleeps in `poll(2)` until the mount table changes.
pub fn spawn(apps: Arc<AppsSvc>) {
    let p = Arc::new(Photos { apps, queue: Mutex::new(Debounce::default()), queued: tokio::sync::Notify::new() });
    if PHOTOS.set(p.clone()).is_err() {
        return;
    }
    let w = p.clone();
    tokio::spawn(async move {
        loop {
            let next = w.queue.lock().unwrap().next_due();
            match next {
                None => w.queued.notified().await,
                Some(at) => {
                    tokio::select! {
                        _ = tokio::time::sleep_until(at.into()) => {}
                        _ = w.queued.notified() => continue,
                    }
                }
            }
            let due = w.queue.lock().unwrap().due(Instant::now());
            for user in due {
                rescan(&w.apps, &user).await;
            }
        }
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
    std::thread::Builder::new()
        .name("photos-mounts".into())
        .spawn(move || watch_mounts(&tx))
        .map(|_| ())
        .unwrap_or_else(|e| tracing::warn!("photos: no mount watcher: {e}"));
    tokio::spawn(async move {
        restore(&p.apps, false).await;
        while rx.recv().await.is_some() {
            // a burst of changes (a container starting) is one check
            tokio::time::sleep(MOUNTS_SETTLE).await;
            while rx.try_recv().is_ok() {}
            let Ok(users) = users_checked(&passwd(&p.apps)) else { continue };
            let worth = missing_anywhere(&photos_dir(&p.apps.docker), &p.apps.docker.root, &users, &FAILURES.lock().unwrap());
            if worth {
                restore(&p.apps, true).await;
            }
        }
    });
}

/// Blocks in `poll(2)` on `/proc/self/mountinfo` and sends one message per change of the mount
/// table (the kernel signals `POLLPRI`/`POLLERR`; no wakeup otherwise).
fn watch_mounts(tx: &tokio::sync::mpsc::Sender<()>) {
    use std::io::{Read, Seek};
    use std::os::fd::AsRawFd;
    let Ok(mut f) = std::fs::File::open("/proc/self/mountinfo") else { return };
    let mut buf = Vec::new();
    loop {
        // reading the table to its end arms the next event
        buf.clear();
        if f.rewind().is_err() || f.read_to_end(&mut buf).is_err() {
            return;
        }
        let mut pfd = libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLPRI | libc::POLLERR, revents: 0 };
        // SAFETY: poll on one valid descriptor, no timeout
        let r = unsafe { libc::poll(&mut pfd, 1, -1) };
        if r < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return;
        }
        if tx.try_send(()).is_err() && tx.is_closed() {
            return;
        }
    }
}

/// Whether a chosen folder of any photo stack is not mounted (only the choices files and the
/// mount table are read: most mount changes are not about photos).
pub fn missing_anywhere(dir: &Path, root: &Path, users: &[NasUser], failures: &Failures) -> bool {
    let mi = mountinfo();
    std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
        let p = e.path();
        let Some(stack) = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".json")) else { return false };
        let store = load_store(&p);
        installed(&store, root)
            && to_restore(&store, &mounted_users(&mi, &dir.join(stack)))
                .iter()
                .filter_map(|n| users.iter().find(|u| &u.name == n))
                .any(|u| !failures.skip(stack, &u.name, &source_id(root, u)))
    })
}

/// The last failed mount of each (stack, user) with the identity its source had then: a
/// change of the mount table does not try again (and spawn a helper) until the source
/// changes. A start of rynasd or a sync tries everything again.
#[derive(Debug, Default)]
pub struct Failures(BTreeMap<(String, String), String>);

impl Failures {
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }
    /// The source is as it was when the mount failed.
    pub fn skip(&self, stack: &str, user: &str, id: &str) -> bool {
        self.0.get(&(stack.to_string(), user.to_string())).is_some_and(|f| f == id)
    }
    pub fn failed(&mut self, stack: &str, user: &str, id: String) {
        self.0.insert((stack.to_string(), user.to_string()), id);
    }
    pub fn ok(&mut self, stack: &str, user: &str) {
        self.0.remove(&(stack.to_string(), user.to_string()));
    }
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

static FAILURES: Mutex<Failures> = Mutex::new(Failures::new());

/// The identity of a user's `Photos` folder, from a cheap `lstat` (never the helper):
/// device, inode, owner and mode, or why there is none.
pub fn source_id(root: &Path, u: &NasUser) -> String {
    use std::os::unix::fs::MetadataExt;
    let Ok(home) = real_home(root, u) else { return "unreachable".into() };
    match std::fs::symlink_metadata(home.join(FOLDER)) {
        Ok(m) => format!("{}:{}:{}:{:o}", m.dev(), m.ino(), m.uid(), m.mode()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "absent".into(),
        Err(e) => format!("error {}", e.kind()),
    }
}

/// The users of a stack to mount (at start, after a change of the mount table, at boot):
/// chosen, confirmed when the app has an API, not blocked, and not mounted yet.
pub fn to_restore(store: &Store, mounted: &BTreeSet<String>) -> Vec<String> {
    store
        .users
        .iter()
        .filter(|(n, m)| !mounted.contains(*n) && m.blocked.is_none() && (!store.api || m.account.is_some()))
        .map(|(n, _)| n.clone())
        .collect()
}

/// The mounts of every installed photo app as chosen, made where missing (no call to the
/// apps: a block found by the last sync stays). A stack busy with another task is skipped:
/// its task ends with a sync or leaves the mounts as they were.
async fn restore(a: &Arc<AppsSvc>, event: bool) {
    if !event {
        // a start tries every folder again
        FAILURES.lock().unwrap().clear();
    }
    let (cat, _) = a.catalog().await;
    let Ok(users) = users_checked(&passwd(a)) else { return };
    let gid = a.apps_ids().map(|i| i.1);
    for d in a.on_disk() {
        let Some(app) = cat.get(&d.meta.app) else { continue };
        if app.manifest.user_photos.is_none() {
            continue;
        }
        let Ok(_guard) = mark_busy(&a.docker, &d.stack) else { continue };
        // the choices and the mount table read under the busy mark
        let store = load_store(&store_path(&a.docker, &d.stack));
        let root = staging_root(&a.docker, &d.stack);
        for name in to_restore(&store, &mounted_users(&mountinfo(), &root)) {
            let Some(u) = users.iter().find(|u| u.name == name) else { continue };
            let id = source_id(&a.docker.root, u);
            if event && FAILURES.lock().unwrap().skip(&d.stack, &name, &id) {
                continue;
            }
            match mount_user(&a.docker, &root, u, gid).await {
                Ok(out) => {
                    FAILURES.lock().unwrap().ok(&d.stack, &name);
                    tracing::info!("photos: {}: {name}: {out}", d.stack);
                }
                Err(e) => {
                    FAILURES.lock().unwrap().failed(&d.stack, &name, id);
                    tracing::debug!("photos: {}: {name}: {e}", d.stack);
                }
            }
        }
    }
}

/// `rynasd photos-restore` (the boot unit, before the container engine): the chosen users'
/// folders of every photo stack under `<state>/docker/photos`, mounted with the same checks.
/// Never fails: what it cannot mount, rynasd tries again when it starts and when the mount
/// table changes (a volume mounted later). Stacks that are not installed are skipped.
pub fn boot_restore(docker_state: &Path, root: &Path) -> Vec<String> {
    let mut log = Vec::new();
    let users = match users_checked(&std::fs::read_to_string(rynas_docker::under(root, "/etc/passwd")).unwrap_or_default()) {
        Ok(u) => u,
        Err(e) => return vec![e],
    };
    let dir = docker_state.join("photos");
    let mi = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let p = e.path();
        let Some(stack) = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".json")).map(str::to_string) else { continue };
        if rynas_docker::stacks::check_name(&stack).is_err() {
            continue;
        }
        let staging = dir.join(&stack);
        let store = load_store(&p);
        if !installed(&store, root) {
            log.push(format!("{stack}: not installed: nothing mounted"));
            continue;
        }
        for name in to_restore(&store, &mounted_users(&mi, &staging)) {
            let Some(u) = users.iter().find(|u| u.name == name) else { continue };
            let r = real_home(root, u).and_then(|home| {
                ensure_staging(&staging, [name.as_str()])?;
                crate::photos_mount::mount(&home, u.uid, &staging.join(&name))
            });
            log.push(match r {
                Ok(_) => format!("{stack}: {name}: mounted"),
                Err(e) => format!("{stack}: {name}: {e}"),
            });
        }
    }
    log
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
        p.queued.notify_one();
    }
}

/// Scans the user's library in every photo app with an API key where the user is confirmed:
/// only the library of the confirmed account on the user's path.
async fn rescan(a: &Arc<AppsSvc>, user: &str) {
    let (cat, _) = a.catalog().await;
    for d in a.on_disk() {
        let Some(app) = cat.get(&d.meta.app) else { continue };
        let Some(up) = &app.manifest.user_photos else { continue };
        let store = load_store(&store_path(&a.docker, &d.stack));
        let Some(account) = store.users.get(user).filter(|m| m.blocked.is_none()).and_then(|m| m.account.clone()) else { continue };
        let env = rynas_apps::render::parse_env(&d.env);
        let Some(Ok(c)) = client_of(app, &env) else { continue };
        let path = import_path(&up.target, user);
        let r = async {
            let libs = c.libraries().await?;
            match scan_target(&libs, &path, &account) {
                Some(l) => c.scan(&l.id).await.map(|_| true),
                None => Ok(false),
            }
        }
        .await;
        match r {
            Ok(true) => tracing::info!("photos: {}: scan of {user}'s library queued after an upload", d.stack),
            Ok(false) => tracing::debug!("photos: {}: {user} has no library of the confirmed account", d.stack),
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

/// The NAS users, or why nothing may be changed: an unreadable or empty user list would
/// make every chosen user look gone (and lose their folder in the apps).
pub fn users_checked(passwd: &str) -> Result<Vec<NasUser>, String> {
    let users = nas_users(passwd);
    if users.is_empty() {
        return Err("the user list (/etc/passwd) cannot be read or has no NAS user: the mounts stay as they are".into());
    }
    Ok(users)
}

/// `GET /v1/apps/installed/{stack}/photos`.
async fn state_get(State(st): State<AppsState>, hd: HeaderMap, UrlPath(stack): UrlPath<String>) -> Res<Json<PhotosState>> {
    let a = asvc(&st)?;
    admin(&a, &hd)?;
    let (cat, _, d) = photo_app(&a, &stack).await?;
    let app = cat.get(&d.meta.app).ok_or_else(|| Error::not_found("no such app"))?;
    let up = app.manifest.user_photos.clone().ok_or_else(|| Error::internal("no user_photos"))?;
    let env = rynas_apps::render::parse_env(&d.env);
    let store = load_store(&store_path(&a.docker, &stack));
    let mounted = mounted_users(&mountinfo(), &staging_root(&a.docker, &stack));
    let root = a.docker.root.clone();
    let mut out = PhotosState { users: Vec::new(), api: None, allow_list: (d.meta.app == "pigallery2").then(|| ALLOW_LIST.to_string()) };
    for u in nas_users(&passwd(&a)) {
        let m = store.users.get(&u.name);
        let folder = real_home(&root, &u).map(|h| h.join(FOLDER).to_string_lossy().into_owned()).unwrap_or_else(|_| format!("{}/{FOLDER}", u.home));
        out.users.push(PhotosUser {
            included: m.is_some(),
            mounted: mounted.contains(&u.name),
            problem: m.and_then(|m| m.blocked.clone()),
            user: u.name,
            folder,
            ..Default::default()
        });
    }
    if up.key_env.is_some() {
        let mut api = PhotosApi::default();
        if let Some(c) = client_of(app, &env) {
            api.key = true;
            match async { c?.view().await }.await {
                Ok((accounts, libs)) => {
                    let mails = emails(&a);
                    for row in &mut out.users {
                        let account = store.users.get(&row.user).and_then(|m| m.account.clone());
                        row.library = Some(library_state(&row.user, account.as_deref(), &accounts, &libs, &up.target));
                        row.account = account.as_deref().and_then(|id| accounts.iter().find(|a| a.id == id)).map(Account::view);
                        if account.is_none() {
                            row.proposed = propose(&row.user, mails.get(&row.user).map_or("", String::as_str), &accounts).map(Account::view);
                        }
                    }
                }
                Err(e) => api.error = Some(e),
            }
        }
        out.api = Some(api);
    }
    Ok(Json(out))
}

fn start_sync<G: Send + 'static>(a: &Arc<AppsSvc>, who: &str, stack: String, guard: G) -> Res<Response> {
    let a2 = a.clone();
    let id = a.docker.ops.tasks.spawn("apps.photos-sync", who, move |ctx| async move {
        let _guard = guard;
        sync_task(&a2, &ctx, &stack).await
    })?;
    Ok(accepted(id))
}

/// `POST /v1/apps/installed/{stack}/photos/sync`: task `apps.photos-sync`.
async fn sync_post(State(st): State<AppsState>, hd: HeaderMap, UrlPath(stack): UrlPath<String>) -> Res<Response> {
    let a = asvc(&st)?;
    let who = admin(&a, &hd)?;
    photo_app(&a, &stack).await?;
    let guard = mark_busy(&a.docker, &stack)?;
    audit(&who, "photos-sync", &stack);
    start_sync(&a, &who, stack, guard)
}

/// `POST /v1/apps/installed/{stack}/photos/users/{user}`: an administrator chooses a user (with
/// an API: confirms the user's account) or leaves them out; task `apps.photos-sync`.
async fn user_post(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath((stack, user)): UrlPath<(String, String)>,
    body: Result<Json<PhotosUserRequest>, JsonRejection>,
) -> Res<Response> {
    let a = asvc(&st)?;
    let who = admin(&a, &hd)?;
    let Json(req) = body?;
    // busy first: the choices are read and written under it, and the sync runs under it
    let guard = mark_busy(&a.docker, &stack)?;
    let (cat, l, d) = photo_app(&a, &stack).await?;
    let app = cat.get(&d.meta.app).ok_or_else(|| Error::not_found("no such app"))?;
    let api = app.manifest.user_photos.as_ref().is_some_and(|u| u.key_env.is_some());
    if !nas_users(&passwd(&a)).iter().any(|u| u.name == user) {
        return Err(Error::invalid("user", format!("{user} is not a NAS user with a home")));
    }
    let path = store_path(&a.docker, &stack);
    let mut store = load_store(&path);
    match (req.include, api, req.account.as_deref().filter(|s| !s.is_empty())) {
        (false, _, _) => {
            store.users.remove(&user);
        }
        (true, false, _) => {
            store.users.insert(user.clone(), Member::default());
        }
        (true, true, None) => return Err(Error::invalid("account", "confirm the user's account in the app")),
        (true, true, Some(acc)) => {
            let env = rynas_apps::render::parse_env(&d.env);
            let c = client_of(app, &env).ok_or_else(|| conflict("give rynas an API key of the app first"))?.map_err(Error::internal)?;
            let accounts = c.accounts().await.map_err(conflict)?;
            if !accounts.iter().any(|x| x.id == acc && x.deleted_at.is_none()) {
                return Err(Error::invalid("account", "no such account in the app"));
            }
            store.users.insert(user.clone(), Member { account: Some(acc.to_string()), blocked: None });
        }
    }
    store.api = api;
    store.stack_dir = Some(l.stack(&stack));
    save_store(&path, &store).map_err(Error::internal)?;
    audit(&who, if req.include { "photos-user-chosen" } else { "photos-user-left-out" }, &format!("{stack} {user}"));
    start_sync(&a, &who, stack, guard)
}

/// Brings the stack and the mounts in line with the choices, then, with an API key, creates
/// the missing libraries of confirmed accounts and scans them.
pub(crate) async fn sync_task(a: &Arc<AppsSvc>, ctx: &TaskCtx, stack: &str) -> Result<(), String> {
    let (cat, l, d) = photo_app(a, stack).await.map_err(|e| e.2)?;
    let app = cat.get(&d.meta.app).ok_or("the app left the catalog")?;
    let m = &app.manifest;
    let up = m.user_photos.clone().ok_or("no user_photos")?;
    let root = staging_root(&a.docker, stack);
    let root_s = prepare_staging(&a.docker, app, stack)?;
    let env = rynas_apps::render::parse_env(&d.env);
    // the stack binds the staging folder (installed before it, or updated from 1.x)
    if env.get(&up.env) != Some(&root_s) {
        if d.meta.version != m.version {
            return Err(format!("{stack} is {} {} and the catalog has {}: update it first", m.name, d.meta.version, m.version));
        }
        let ids = a.stack_ids(&env).map_err(|e| e.2)?;
        let bind = env.get("RYNAS_BIND").cloned().unwrap_or_else(|| "127.0.0.1".into());
        let provided = a.provided(&l, stack, &bind, ids);
        let mut req = rynas_apps::render::request_from_env(app, stack, &env);
        req.user_photos = Some(root_s.clone());
        let r = rynas_apps::render::render(app, &cat.source, &provided, &req).map_err(|e| render_err(e).2)?;
        let (old, new) = ((d.compose.clone(), d.env.clone()), (file_text(&r, COMPOSE_FILE), file_text(&r, ".env")));
        ctx.log(&format!("{stack}: binding the staging folder {root_s} (the app restarts)"));
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
    }
    ctx.progress(30);
    let client = client_of(app, &env).and_then(|c| c.ok());
    let view = match &client {
        Some(c) => match c.view().await {
            Ok(v) => Some(v),
            Err(e) => {
                ctx.log(&format!("warning: {e}: the mounts stay as they are"));
                None
            }
        },
        None => None,
    };
    let path = store_path(&a.docker, stack);
    let store = load_store(&path);
    let users = users_checked(&passwd(a))?;
    let mounted = mounted_users(&mountinfo(), &root);
    let api = up.key_env.is_some();
    let (steps, mut kept) = plan(&store, &users, &mounted, api, view.as_ref().map(|(x, y)| (x.as_slice(), y.as_slice())), &up.target);
    kept.stack_dir = Some(l.stack(stack));
    save_store(&path, &kept)?;
    let gid = a.apps_ids().map(|i| i.1);
    let mut ready = Vec::new();
    for (name, step) in steps {
        match step {
            Step::Mount => {
                let Some(u) = users.iter().find(|u| u.name == name) else { continue };
                // a sync always tries; a failure is remembered for the mount table's changes
                let id = source_id(&a.docker.root, u);
                match mount_user(&a.docker, &root, u, gid).await {
                    Ok(out) => {
                        FAILURES.lock().unwrap().ok(stack, &name);
                        ctx.log(&format!("{name}: {out}"));
                        ready.push(name);
                    }
                    Err(e) => {
                        FAILURES.lock().unwrap().failed(stack, &name, id);
                        if mounted.contains(&name) {
                            ctx.log(&format!("warning: {name}: {e}: the folder mounted before stays"));
                        } else {
                            ctx.log(&format!("warning: {name}: {e}: not mounted"));
                        }
                    }
                }
            }
            Step::Unmount(why) => {
                if mounted.contains(&name) {
                    unmount_user(&a.docker, &root, &name).await?;
                }
                ctx.log(&format!("{name}: not mounted: {why}"));
            }
            Step::Forget => {
                unmount_user(&a.docker, &root, &name).await?;
                ctx.log(&format!("{name} is no longer a NAS user: the folder left the app, which may move its photos to its trash"));
            }
            Step::Hold(why) => ctx.log(&format!("{name}: not mounted: {why}")),
        }
    }
    ctx.progress(70);
    if let (Some(c), Some((accounts, libs))) = (&client, &view) {
        for name in &ready {
            let Some(acc) = kept.users.get(name).and_then(|m| m.account.clone()) else { continue };
            let p = import_path(&up.target, name);
            match library_state(name, Some(&acc), accounts, libs, &up.target) {
                PhotosLibrary::Missing => {
                    let lib = c.create_library(&acc, &p).await?;
                    ctx.log(&format!("{name}: library created for the confirmed account, reading {p}"));
                    c.scan(&lib.id).await?;
                }
                PhotosLibrary::Linked => ctx.log(&format!("{name}: library already there")),
                PhotosLibrary::NoAccount => ctx.log(&format!("warning: {name}: the confirmed account no longer exists in the app")),
                PhotosLibrary::OtherOwner | PhotosLibrary::Unconfirmed => {}
            }
        }
    } else if api {
        ctx.log("no answer from the app or no API key: libraries not checked");
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
    // busy first, then the files as they are now (no write over a change made meanwhile)
    let _guard = mark_busy(&a.docker, &stack)?;
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
        c.view().await.map_err(|e| Error::invalid("key", e))?;
    }
    write_stack(&a.docker, &l, &stack, &(d.compose.clone(), new_env)).await.map_err(Error::internal)?;
    audit(&who, if key.is_empty() { "photos-key-removed" } else { "photos-key-set" }, &stack);
    Ok(axum::http::StatusCode::NO_CONTENT)
}

pub fn router(st: AppsState) -> Router {
    Router::new()
        .route("/v1/apps/installed/{stack}/photos", get(state_get))
        .route("/v1/apps/installed/{stack}/photos/sync", post(sync_post))
        .route("/v1/apps/installed/{stack}/photos/key", post(key_post))
        .route("/v1/apps/installed/{stack}/photos/users/{user}", post(user_post))
        .with_state(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(id: &str, email: &str, name: &str) -> Account {
        Account { id: id.into(), email: email.into(), name: name.into(), deleted_at: None }
    }

    fn lib(id: &str, owner: &str, path: &str) -> Library {
        Library { id: id.into(), owner_id: owner.into(), import_paths: vec![path.into()] }
    }

    fn user(name: &str) -> NasUser {
        NasUser { name: name.into(), uid: 1000, gid: 100, home: format!("/home/{name}") }
    }

    fn store(v: &[(&str, Option<&str>)]) -> Store {
        Store { users: v.iter().map(|(n, a)| (n.to_string(), Member { account: a.map(str::to_string), blocked: None })).collect(), api: false, stack_dir: None }
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

    /// Review M2: a proposal is only a proposal; email first, a name only when unique.
    #[test]
    fn accounts_are_proposed_never_chosen() {
        let accounts = [acc("1", "ann@example.com", "Ann"), acc("2", "bob@example.com", "Robert"), acc("3", "x@example.com", "cid"), acc("4", "y@example.org", "CID"), Account { deleted_at: Some("2026-01-01".into()), ..acc("5", "dan@example.com", "dan") }];
        assert_eq!(propose("ann", "", &accounts).unwrap().id, "1");
        assert_eq!(propose("robert", "BOB@example.com", &accounts).unwrap().id, "2");
        assert_eq!(propose("ann", "bob@example.com", &accounts).unwrap().id, "2", "the email decides");
        assert!(propose("cid", "", &accounts).is_none(), "two names: none");
        assert!(propose("dan", "", &accounts).is_none(), "deleted accounts never");
        // the email's local part proposes nothing (anyone may name an address so)
        assert!(propose("x", "", &accounts).is_none());
        // nothing is linked without a confirmation: an unconfirmed user is never mounted
        let (steps, kept) = plan(&store(&[("ann", None)]), &[user("ann")], &BTreeSet::new(), true, Some((&accounts, &[])), "/mnt/photos");
        assert_eq!(steps, [("ann".to_string(), Step::Hold("no account confirmed".into()))]);
        assert_eq!(kept.users["ann"].account, None);
        // a confirmed account stays, whatever the names become
        let renamed = [acc("1", "other@example.com", "Mallory")];
        let (steps, kept) = plan(&store(&[("ann", Some("1"))]), &[user("ann")], &BTreeSet::new(), true, Some((&renamed, &[])), "/mnt/photos");
        assert_eq!(steps, [("ann".to_string(), Step::Mount)]);
        assert_eq!(kept.users["ann"].account.as_deref(), Some("1"));
        // the choices survive on disk
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("photos/immich.json");
        save_store(&p, &kept).unwrap();
        assert_eq!(load_store(&p), kept);
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
    }

    /// Review M3: a library of another account on the user's path keeps the user unmounted
    /// (and stays so without an answer from the app); a rescan asks only for the confirmed
    /// account's library.
    #[test]
    fn another_accounts_library_blocks_and_is_never_scanned() {
        let accounts = [acc("a1", "ann@example.com", "ann"), acc("m1", "m@example.com", "mallory")];
        let libs = [lib("L1", "m1", "/mnt/photos/ann/")];
        assert_eq!(library_state("ann", Some("a1"), &accounts, &libs, "/mnt/photos"), PhotosLibrary::OtherOwner);
        assert!(scan_target(&libs, "/mnt/photos/ann", "a1").is_none());
        let mounted: BTreeSet<String> = ["ann".to_string()].into();
        let (steps, kept) = plan(&store(&[("ann", Some("a1"))]), &[user("ann")], &mounted, true, Some((&accounts, &libs)), "/mnt/photos");
        assert!(matches!(&steps[0].1, Step::Unmount(w) if w.contains("another account")), "{steps:?}");
        assert!(kept.users["ann"].blocked.is_some());
        // the app does not answer: the block stays
        let (steps, _) = plan(&kept, &[user("ann")], &BTreeSet::new(), true, None, "/mnt/photos");
        assert!(matches!(&steps[0].1, Step::Unmount(_)));
        // resolved in the app (the other library gone, the user's own made): mounted again
        let libs = [lib("L1", "m1", "/mnt/photos/old"), lib("L2", "a1", "/mnt/photos/ann")];
        let (steps, kept) = plan(&kept, &[user("ann")], &BTreeSet::new(), true, Some((&accounts, &libs)), "/mnt/photos");
        assert_eq!(steps[0].1, Step::Mount);
        assert!(kept.users["ann"].blocked.is_none());
        assert_eq!(scan_target(&libs, "/mnt/photos/ann", "a1").map(|l| l.id.as_str()), Some("L2"));
        assert_eq!(library_state("ann", Some("a1"), &accounts, &libs, "/mnt/photos"), PhotosLibrary::Linked);
        assert_eq!(library_state("ann", Some("gone"), &accounts, &[], "/mnt/photos"), PhotosLibrary::NoAccount);
        assert_eq!(library_state("ann", None, &accounts, &[], "/mnt/photos"), PhotosLibrary::Unconfirmed);
    }

    /// Review M4: only users gone from passwd lose their mount; anything transient (no answer
    /// from the app, a home that cannot be reached) keeps it; users left out are unmounted.
    #[test]
    fn a_sync_keeps_mounts_on_transient_failures() {
        let s = store(&[("ann", None), ("bob", None), ("gone", None)]);
        let mounted: BTreeSet<String> = ["ann", "gone", "cid"].iter().map(|s| s.to_string()).collect();
        let (steps, kept) = plan(&s, &[user("ann"), user("bob"), user("cid")], &mounted, false, None, "/app/data/images");
        assert_eq!(
            steps,
            [
                ("ann".to_string(), Step::Mount),
                ("bob".to_string(), Step::Mount),
                ("gone".to_string(), Step::Forget),
                ("cid".to_string(), Step::Unmount("left out by an administrator".into())),
            ]
        );
        assert_eq!(kept.users.keys().collect::<Vec<_>>(), ["ann", "bob"]);
        // a home on a volume that is not mounted yet: the step is still Mount (the helper
        // fails before touching the target, the task keeps the mount)
        let t = tempfile::tempdir().unwrap();
        let far = NasUser { home: "/vol9/homes/ann".into(), ..user("ann") };
        assert!(real_home(t.path(), &far).is_err());
    }

    /// Review 2 H1: the apps' account is let in through the real call path (open the folder,
    /// walk from its descriptor): every entry of the user, no error, never a hard link.
    #[test]
    fn photos_are_opened_to_the_apps_account_from_the_checked_folder() {
        use std::os::unix::fs::MetadataExt;
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("ann");
        std::fs::create_dir_all(home.join("Photos/2026/11")).unwrap();
        std::fs::write(home.join("Photos/2026/11/a.jpg"), "jpg").unwrap();
        std::fs::write(t.path().join("elsewhere"), "secret").unwrap();
        std::fs::hard_link(t.path().join("elsewhere"), home.join("Photos/linked.jpg")).unwrap();
        let uid = std::fs::metadata(&home).unwrap().uid();
        let c = open_to_apps(&rynas_services::idremap::DryRemap, &home, uid, 54400).unwrap();
        assert_eq!(c.errors, 0, "{c:?}");
        // Photos, 2026, 11 (access + default each) and a.jpg; never linked.jpg
        assert_eq!(c.acl_entries, 7, "{c:?}");
        // a link instead of the folder: refused before any walk
        std::fs::rename(home.join("Photos"), home.join("Old")).unwrap();
        std::os::unix::fs::symlink("/", home.join("Photos")).unwrap();
        assert!(open_to_apps(&rynas_services::idremap::DryRemap, &home, uid, 54400).unwrap_err().contains("link"));
    }

    /// Review 2 M4/M5 and the boot gap: another account next to the user's own library still
    /// blocks; an empty or unreadable user list changes nothing; the mount-table check and the
    /// boot unit mount only what is chosen, confirmed, not blocked and still missing.
    #[test]
    fn guards_of_the_review() {
        let accounts = [acc("a1", "ann@example.com", "ann"), acc("m1", "m@example.com", "mallory")];
        let both = [lib("L1", "a1", "/mnt/photos/ann"), lib("L2", "m1", "/mnt/photos/ann")];
        assert_eq!(library_state("ann", Some("a1"), &accounts, &both, "/mnt/photos"), PhotosLibrary::OtherOwner);
        assert!(users_checked("").is_err());
        assert!(users_checked("root:x:0:0::/root:/bin/sh\n").is_err());
        assert_eq!(users_checked("ann:x:1000:100::/home/ann:/bin/sh\n").unwrap().len(), 1);
        let mut s = store(&[("ann", Some("a1")), ("bob", None), ("cid", Some("c1")), ("dan", Some("d1"))]);
        s.api = true;
        s.users.get_mut("dan").unwrap().blocked = Some("other library".into());
        let mounted: BTreeSet<String> = ["cid".to_string()].into();
        assert_eq!(to_restore(&s, &mounted), ["ann"]);
        s.api = false;
        assert_eq!(to_restore(&s, &mounted), ["ann", "bob"]);
        // the boot unit with nobody in passwd: nothing touched, said why
        let t = tempfile::tempdir().unwrap();
        let log = boot_restore(&t.path().join("state/docker"), t.path());
        assert!(log.len() == 1 && log[0].contains("no NAS user"), "{log:?}");
        // with a user whose folder is not there: logged, nothing mounted, never an error
        std::fs::create_dir_all(t.path().join("etc")).unwrap();
        std::fs::create_dir_all(t.path().join("home/ann")).unwrap();
        std::fs::write(t.path().join("etc/passwd"), "ann:x:1000:100::/home/ann:/bin/sh\n").unwrap();
        let choices = t.path().join("state/docker/photos/pigallery2.json");
        let mut s = store(&[("ann", None)]);
        save_store(&choices, &s).unwrap();
        // a stack that is not installed (no compose file where it was): nothing for it
        let log = boot_restore(&t.path().join("state/docker"), t.path());
        assert_eq!(log, ["pigallery2: not installed: nothing mounted"]);
        let users = users_checked("ann:x:1000:100::/home/ann:/bin/sh\n").unwrap();
        let none = Failures::new();
        assert!(!missing_anywhere(&t.path().join("state/docker/photos"), t.path(), &users, &none));
        std::fs::create_dir_all(t.path().join("vol1/docker/stacks/pigallery2")).unwrap();
        std::fs::write(t.path().join("vol1/docker/stacks/pigallery2/compose.yaml"), "services: {}\n").unwrap();
        s.stack_dir = Some("/vol1/docker/stacks/pigallery2".into());
        save_store(&choices, &s).unwrap();
        let photos = t.path().join("state/docker/photos");
        assert!(missing_anywhere(&photos, t.path(), &users, &none), "ann is chosen and not mounted");
        // a failed mount is not tried again on mount table changes while the source is the same
        let mut failures = Failures::new();
        failures.failed("pigallery2", "ann", source_id(t.path(), &users[0]));
        assert_eq!(source_id(t.path(), &users[0]), "absent");
        assert!(!missing_anywhere(&photos, t.path(), &users, &failures), "nothing changed: no helper");
        // the user's phone made the folder: worth a try again
        std::fs::create_dir_all(t.path().join("home/ann/Photos")).unwrap();
        assert_ne!(source_id(t.path(), &users[0]), "absent");
        assert!(missing_anywhere(&photos, t.path(), &users, &failures));
        // replaced by another folder: another identity
        let id = source_id(t.path(), &users[0]);
        failures.failed("pigallery2", "ann", id.clone());
        assert!(!missing_anywhere(&photos, t.path(), &users, &failures));
        std::fs::rename(t.path().join("home/ann/Photos"), t.path().join("home/ann/Old")).unwrap();
        std::fs::create_dir_all(t.path().join("home/ann/Photos")).unwrap();
        assert_ne!(source_id(t.path(), &users[0]), id);
        assert!(missing_anywhere(&photos, t.path(), &users, &failures));
        // a success or a start forgets it
        failures.ok("pigallery2", "ann");
        assert!(!failures.skip("pigallery2", "ann", &id));
        failures.failed("pigallery2", "ann", id.clone());
        failures.clear();
        assert!(!failures.skip("pigallery2", "ann", &id));
        std::fs::remove_dir(t.path().join("home/ann/Photos")).unwrap();
        std::fs::remove_dir(t.path().join("home/ann/Old")).unwrap();
        let log = boot_restore(&t.path().join("state/docker"), t.path());
        assert_eq!(log.len(), 1, "{log:?}");
        assert!(log[0].starts_with("pigallery2: ann: ") && !log[0].ends_with("mounted"), "{log:?}");
    }

    #[test]
    fn staging_folders_are_made_and_never_links() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("photos/pigallery2");
        ensure_staging(&root, ["Shared", "ann"]).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(root.join("ann")).unwrap().mode() & 0o777, 0o755);
        std::os::unix::fs::symlink("/", root.join("bob")).unwrap();
        assert!(ensure_staging(&root, ["bob"]).unwrap_err().contains("not a folder"));
        // the second sync: a mounted user's folder is never touched (it shows the user's
        // Photos, read-only: a chmod there failed with EROFS and dropped the user)
        std::fs::set_permissions(root.join("ann"), std::fs::Permissions::from_mode(0o775)).unwrap();
        let mounted = format!("40 30 0:5 /x {} ro master:1 - btrfs /dev/sdb rw\n", root.join("ann").display());
        ensure_staging_in(&mounted, &root, ["ann", "Shared"]).unwrap();
        assert_eq!(std::fs::metadata(root.join("ann")).unwrap().mode() & 0o777, 0o775);
        ensure_staging_in("", &root, ["ann"]).unwrap();
        assert_eq!(std::fs::metadata(root.join("ann")).unwrap().mode() & 0o777, 0o755, "not mounted: made 0755");
        // nothing mounted in a fresh folder
        assert!(mounted_users("", &root).is_empty());
        let mi = format!("40 30 0:5 /x {} ro master:1 - btrfs /dev/sdb rw\n", root.join("ann").display());
        assert_eq!(mounted_users(&mi, &root).into_iter().collect::<Vec<_>>(), ["ann"]);
    }

    #[test]
    fn debounce_waits_for_quiet_or_the_longest_wait() {
        let t0 = Instant::now();
        let mut d = Debounce::default();
        assert_eq!(d.next_due(), None, "nothing queued: nothing to wake up for");
        d.poke("ann", t0);
        d.poke("ann", t0 + Duration::from_secs(30));
        assert_eq!(d.next_due(), Some(t0 + Duration::from_secs(30) + QUIET));
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
        let (accounts, libs) = c.view().await.unwrap();
        assert_eq!(accounts.len(), 2);
        let confirmed = [("ann", "u-ann"), ("bob", "u-bob")];
        for (u, a) in confirmed {
            assert_eq!(library_state(u, Some(a), &accounts, &libs, "/mnt/photos"), PhotosLibrary::Missing);
            let lib = c.create_library(a, &import_path("/mnt/photos", u)).await.unwrap();
            c.scan(&lib.id).await.unwrap();
        }
        assert_eq!(f.lock().unwrap().log, ["create u-ann /mnt/photos/ann", "scan lib-1", "create u-bob /mnt/photos/bob", "scan lib-2"]);
        // a second sync finds them, and a rescan finds the confirmed account's library
        let libs = c.libraries().await.unwrap();
        for (u, a) in confirmed {
            assert_eq!(library_state(u, Some(a), &accounts, &libs, "/mnt/photos"), PhotosLibrary::Linked);
        }
        assert_eq!(scan_target(&libs, "/mnt/photos/bob", "u-bob").map(|l| l.id.as_str()), Some("lib-2"));
        assert!(scan_target(&libs, "/mnt/photos/bob", "u-ann").is_none());
        // a wrong key: a plain message, never the key
        let bad = Immich::new(&base, "s3cret-wrong").unwrap();
        assert_eq!(bad.accounts().await.unwrap_err(), "Immich refused the API key");
        // nothing listening
        let gone = Immich::new("http://127.0.0.1:9", "k3y").unwrap();
        assert!(!gone.libraries().await.unwrap_err().contains("k3y"));
        // a library id that could change the path is refused before any request
        assert!(c.scan("../admin").await.is_err());
    }
}
