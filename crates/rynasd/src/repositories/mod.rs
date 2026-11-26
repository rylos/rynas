//! System › Software repositories (`/v1/repositories/...`, D-REPOS; design in
//! `docs/phase2/repositories.md`).
//!
//! - `GET` reads `/etc/apt/sources.list` and every `*.list`/`*.sources` of
//!   `sources.list.d` (both formats; a file that does not parse is reported and never blocks
//!   the others), classifies each entry (Debian, rynas's files, the hardware support's system
//!   layer, third party) by the cached Release files, runs the checks and returns a digest
//!   of the files. Administrators only.
//! - Every write carries that digest (409 `conflict` when the files changed meanwhile) and is
//!   an `updates.repositories` task: the files it changes are copied to
//!   `<state>/apt-backup/<timestamp>/` first, written atomically (0644), then `apt-get update`
//!   runs in the rynas-apt unit and its `W:`/`E:` lines are mapped to the entries. A new
//!   third-party source whose update fails is removed again.
//! - The Debian entries are changed only on plain Debian without a hardware module's system
//!   layer (the rule of the backports switch); rynas then writes one deb822
//!   `debian.sources`, backports stay in rynas's `rynas-backports.sources`.
//! - Enabled third-party sources, testing/unstable, other distributions and insecure
//!   entries make the configuration "unsupported": a flag of the state and the
//!   `repositories:unsupported` notification.
//! - `--dry-system`: the files below the dry root, apt and the network not used.

pub mod aptlog;
pub mod classify;
pub mod keys;
pub mod official;
pub mod sources;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use rynas_api_types::error_codes;
use rynas_api_types::notify::{EventAction, NotifyCategory, NotifySeverity};
use rynas_api_types::repositories::*;
use rynas_config::files;
use rynas_notify::event::{code, field, firing, resolved};
use rynas_tasks::{CmdError, TaskCtx};

use crate::ops::{Error, Res, user};
use crate::updates::{UpdState, Updates};

use classify::{Class, Context, Lists};
use official::{Action, Change};
use sources::{Entry, LIST_DIR, Scan};

const KEYRINGS: &str = "/etc/apt/keyrings";
const NOTIFY_KEY: &str = "repositories:unsupported";
const UPDATE_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) fn ms(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn now_ms() -> u64 {
    ms(SystemTime::now())
}

/// The dpkg architecture of this build.
pub fn dpkg_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "armhf",
        "x86" => "i386",
        "powerpc64" => "ppc64el",
        "riscv64" => "riscv64",
        "s390x" => "s390x",
        other => other,
    }
}

/// The files, their classification and the facts both need.
pub struct View {
    pub scan: Scan,
    pub classes: Vec<Vec<Class>>,
    pub codename: Option<String>,
    pub packages: BTreeMap<String, String>,
    pub system_layer: bool,
}

impl View {
    pub fn read(root: &Path, system_layer: bool) -> View {
        let os = std::fs::read_to_string(files::under_root(root, Path::new("/etc/os-release"))).unwrap_or_default();
        let codename = crate::backports::debian_codename(&os);
        let scan = sources::scan(root);
        let packages = classify::package_files(root);
        let mut lists = Lists::new(root);
        let cx = Context { codename: codename.as_deref(), system_layer, packages: &packages };
        let classes = scan
            .files
            .iter()
            .map(|f| f.entries.iter().map(|e| classify::classify(f, e, &cx, &mut lists)).collect())
            .collect();
        View { scan, classes, codename, packages, system_layer }
    }

    fn class(&self, fi: usize, ei: usize) -> Option<&Class> {
        self.classes.get(fi).and_then(|v| v.get(ei))
    }

    fn entry(&self, path: &str, index: usize) -> Option<(usize, &Entry, &Class)> {
        let fi = self.scan.files.iter().position(|f| f.path == path)?;
        let e = self.scan.files[fi].entries.get(index)?;
        Some((fi, e, self.class(fi, index)?))
    }

    /// Enabled `deb` entries of Debian's main suite, but `skip`.
    fn debian_main_count(&self, skip: Option<(&str, usize)>) -> usize {
        let Some(cn) = self.codename.as_deref() else { return usize::MAX };
        let mut n = 0;
        for (fi, f) in self.scan.files.iter().enumerate() {
            for (ei, e) in f.entries.iter().enumerate() {
                if skip == Some((f.path.as_str(), ei)) {
                    continue;
                }
                if e.enabled && e.is_deb() && e.suites.iter().any(|s| s == cn) && self.class(fi, ei).is_some_and(|c| c.kind == RepoKind::Debian) {
                    n += 1;
                }
            }
        }
        n
    }

    fn official(&self, root: &Path) -> OfficialState {
        official::state(root, &self.scan, &self.classes, self.codename.as_deref(), self.system_layer)
    }
}

/// The last check, kept in `<state>/repositories/check.json`.
fn check_file(state_dir: &Path) -> PathBuf {
    state_dir.join("repositories/check.json")
}

pub fn load_check(state_dir: &Path) -> Option<RepoCheck> {
    let t = std::fs::read(check_file(state_dir)).ok()?;
    serde_json::from_slice(&t).ok()
}

fn save_check(state_dir: &Path, c: &RepoCheck) {
    let p = check_file(state_dir);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(t) = serde_json::to_vec(c)
        && let Err(e) = std::fs::write(&p, t)
    {
        tracing::warn!("{}: {e}", p.display());
    }
}

/// The page's state.
pub fn state(root: &Path, state_dir: &Path, system_layer: bool) -> RepositoriesState {
    let v = View::read(root, system_layer);
    let last = load_check(state_dir);
    let checked = classify::check(root, &v.scan, &v.classes, v.codename.as_deref(), last.as_ref());
    let official = v.official(root);
    let files = v
        .scan
        .files
        .iter()
        .enumerate()
        .map(|(fi, f)| {
            let entries: Vec<RepoEntry> = f
                .entries
                .iter()
                .enumerate()
                .map(|(ei, e)| {
                    let c = v.class(fi, ei).cloned().unwrap_or_default();
                    RepoEntry {
                        index: ei,
                        enabled: e.enabled,
                        types: e.types.clone(),
                        uris: e.uris.clone(),
                        suites: e.suites.clone(),
                        components: e.components.clone(),
                        options: e
                            .options
                            .iter()
                            .map(|(k, val)| (k.clone(), if val.contains('\n') { "(embedded key)".to_string() } else { val.clone() }))
                            .collect(),
                        comment: e.comment.clone(),
                        kind: c.kind,
                        origin: c.origin,
                        read_only: c.read_only,
                        manage_url: c.manage_url,
                        last_fetch_ms: c.last_fetch_ms,
                        errors: classify::errors_for(e, last.as_ref()),
                    }
                })
                .collect();
            let package = v.packages.get(&f.path).cloned();
            let removable = removable(f, &entries, package.is_some());
            RepoFile { path: f.path.clone(), format: f.format, entries, error: f.error.clone(), package, removable }
        })
        .collect();
    let convertible = official::modernize(root, &v.scan, &v.classes)
        .into_iter()
        .filter(|c| matches!(c.action, Action::Rename(_)))
        .map(|c| c.path)
        .collect();
    RepositoriesState {
        available: !v.scan.files.is_empty() || files::under_root(root, Path::new(LIST_DIR)).is_dir(),
        codename: v.codename.clone(),
        arch: dpkg_arch().into(),
        digest: v.scan.digest.clone(),
        files,
        ignored: v.scan.ignored.clone(),
        infos: checked.infos,
        status: checked.status,
        unsupported: checked.unsupported,
        official,
        last_check: last,
        convertible,
        running_task: None,
    }
}

/// A third-party file of `sources.list.d` that no package ships.
fn removable(f: &sources::SourceFile, entries: &[RepoEntry], packaged: bool) -> bool {
    f.path.starts_with(&format!("{LIST_DIR}/"))
        && !packaged
        && f.path != official::DEBIAN_FILE
        && entries.iter().all(|e| e.kind == RepoKind::ThirdParty)
        && (f.error.is_some() || !entries.is_empty())
}

// ------------------------------------------------------------------ applying

/// Copies the files `changes` touch to `<state>/apt-backup/<timestamp>/`, then applies them.
pub fn apply(root: &Path, state_dir: &Path, changes: &[Change], log: &dyn Fn(&str)) -> Result<PathBuf, String> {
    let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%S%.3fZ").to_string();
    let backup = state_dir.join("apt-backup").join(stamp);
    for c in changes {
        let real = files::under_root(root, Path::new(&c.path));
        if real.exists() {
            let dest = backup.join(c.path.trim_start_matches('/'));
            if let Some(d) = dest.parent() {
                std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
            }
            std::fs::copy(&real, &dest).map_err(|e| format!("backup of {}: {e}", c.path))?;
        }
    }
    for c in changes {
        let real = files::under_root(root, Path::new(&c.path));
        match &c.action {
            Action::Write(t) => {
                log(&format!("writing {}", c.path));
                sources::write_atomic(&real, t).map_err(|e| format!("{}: {e}", c.path))?;
            }
            Action::Rename(to) => {
                log(&format!("renaming {} to {to}", c.path));
                std::fs::rename(&real, files::under_root(root, Path::new(to))).map_err(|e| format!("{}: {e}", c.path))?;
            }
            Action::Remove => {
                log(&format!("removing {}", c.path));
                std::fs::remove_file(&real).map_err(|e| format!("{}: {e}", c.path))?;
            }
        }
    }
    if backup.exists() {
        log(&format!("the previous files are kept in {}", backup.display()));
    }
    Ok(backup)
}

/// `apt-get update` in the rynas-apt unit; its problems mapped (dry: not run).
async fn update(u: &Updates, ctx: &TaskCtx) -> Result<RepoCheck, String> {
    if u.dry {
        ctx.log("dry system, not run: apt-get update");
        tokio::time::sleep(Duration::from_millis(300)).await;
        return Ok(RepoCheck { at_ms: now_ms(), ok: true, errors: Vec::new() });
    }
    let cmd = crate::updates::apt(&["update"], UPDATE_TIMEOUT);
    let (ok, o) = match ctx.run(&cmd).await {
        Ok(o) => (true, o),
        Err(CmdError::Failed(o)) => (false, o),
        Err(e) => return Err(format!("apt-get update: {e}")),
    };
    let errors = aptlog::parse(&format!("{}\n{}", o.stdout, o.stderr));
    Ok(RepoCheck { at_ms: now_ms(), ok, errors })
}

/// The update after a write: recorded, its problems logged (the write stays).
async fn update_after(u: &Updates, ctx: &TaskCtx) -> Result<RepoCheck, String> {
    let c = update(u, ctx).await?;
    save_check(&u.state_dir, &c);
    for e in &c.errors {
        ctx.log(&format!("{}: {}", if e.error { "error" } else { "warning" }, e.message));
    }
    Ok(c)
}

/// The problems of `check` that mean the new source does not work.
pub fn new_source_failures(e: &Entry, check: &RepoCheck) -> Vec<FetchError> {
    classify::errors_for(e, Some(check))
        .into_iter()
        .filter(|x| {
            x.error
                || matches!(
                    x.kind,
                    FetchErrorKind::NoPubkey
                        | FetchErrorKind::NotSigned
                        | FetchErrorKind::BadSignature
                        | FetchErrorKind::ExpiredKey
                        | FetchErrorKind::NotFound
                        | FetchErrorKind::Network
                        | FetchErrorKind::ExpiredRelease
                )
        })
        .collect()
}

// ------------------------------------------------------------------ requests

fn conflict(msg: impl Into<String>) -> Error {
    Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, msg.into())
}

fn stale() -> Error {
    conflict("the apt sources changed meanwhile: read them again")
}

fn read_only(msg: impl Into<String>) -> Error {
    Error::new(StatusCode::CONFLICT, error_codes::READ_ONLY, msg.into())
}

/// Refuses an entry rynas does not change here.
fn writable(c: &Class) -> Result<(), Error> {
    match c.read_only {
        None => Ok(()),
        Some(ReadOnly::Rynas) => Err(read_only(format!(
            "rynas manages this source on another page ({})",
            c.manage_url.as_deref().unwrap_or("")
        ))),
        Some(ReadOnly::SystemLayer) => Err(read_only("the hardware support manages this source")),
        Some(ReadOnly::DebianOnSystemLayer) => Err(read_only("the hardware support manages the Debian sources of this system")),
    }
}

/// The plan of a toggle (`Err` when refused).
pub fn plan_toggle(v: &View, req: &ToggleRequest) -> Result<Vec<Change>, Error> {
    if req.digest != v.scan.digest {
        return Err(stale());
    }
    let (fi, e, c) = v.entry(&req.path, req.index).ok_or_else(|| Error::not_found(format!("no entry {} in {}", req.index, req.path)))?;
    writable(c)?;
    if e.enabled == req.enabled {
        return Err(conflict(if req.enabled { "the entry is already enabled" } else { "the entry is already disabled" }));
    }
    let main = v.codename.as_deref().is_some_and(|cn| e.suites.iter().any(|s| s == cn)) && c.kind == RepoKind::Debian && e.is_deb();
    if !req.enabled && main && v.debian_main_count(Some((&req.path, req.index))) == 0 {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::LAST_DEBIAN_ENTRY,
            "this is the last enabled Debian source of the release: without it the system gets no updates".into(),
        ));
    }
    let f = &v.scan.files[fi];
    Ok(vec![Change { path: f.path.clone(), before: f.text.clone(), action: Action::Write(sources::toggle(&f.text, e, req.enabled)) }])
}

/// The plan of a change of the Debian section.
pub fn plan_official(root: &Path, v: &View, req: &OfficialRequest) -> Result<Vec<Change>, Error> {
    if req.digest != v.scan.digest {
        return Err(stale());
    }
    let cur = v.official(root);
    let Some(cn) = v.codename.as_deref().filter(|_| cur.manageable) else {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::NOT_AVAILABLE,
            if v.system_layer {
                "the hardware support manages the Debian sources of this system".into()
            } else {
                "not a plain Debian system".into()
            },
        ));
    };
    official::validate(req).map_err(|(f, m)| Error::invalid(f, m))?;
    Ok(official::plan(root, &v.scan, &v.classes, cn, req, &cur))
}

pub fn plan_modernize(root: &Path, v: &View, req: &ModernizeRequest) -> Result<Vec<Change>, Error> {
    if req.digest != v.scan.digest {
        return Err(stale());
    }
    Ok(official::modernize(root, &v.scan, &v.classes))
}

fn word_ok(s: &str, extra: &[u8]) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
}

/// Checks an add request against the files: the source and keyring it would write.
pub fn plan_add(root: &Path, v: &View, req: &AddRepoRequest) -> Result<(Vec<Change>, Entry), Error> {
    if req.digest != v.scan.digest {
        return Err(stale());
    }
    let name = req.name.trim();
    if !repo_name_ok(name) {
        return Err(Error::invalid("name", "use lower-case letters, digits, dots, dashes and underscores, starting with a letter or digit"));
    }
    let src = format!("{LIST_DIR}/{name}.sources");
    let key = format!("{KEYRINGS}/{name}.asc");
    for p in [src.clone(), format!("{LIST_DIR}/{name}.list"), key.clone()] {
        if files::under_root(root, Path::new(&p)).exists() {
            return Err(Error::new(StatusCode::CONFLICT, error_codes::ALREADY_EXISTS, format!("{p} exists already")).field("name"));
        }
    }
    if req.uris.is_empty() || req.uris.len() > 4 {
        return Err(Error::invalid("uris", "one to four addresses"));
    }
    for u in &req.uris {
        if u.starts_with("file:") || u.starts_with("cdrom:") || u.starts_with("copy:") {
            return Err(Error::invalid("uris", "local sources (file:, cdrom:) cannot be added here"));
        }
        if !official::valid_mirror(u.trim()) {
            return Err(Error::invalid("uris", format!("{u} is not an http:// or https:// address")));
        }
        if classify::other_distro(u, None) {
            return Err(Error::invalid("uris", "packages of another distribution would break Debian"));
        }
    }
    if req.suites.is_empty() || req.suites.len() > 8 || req.suites.iter().any(|s| !word_ok(s, b"._-/~+")) {
        return Err(Error::invalid("suites", "one to eight suites (letters, digits and ._-/~+)"));
    }
    let exact = req.suites.iter().any(|s| s.ends_with('/'));
    if exact && (req.suites.len() != 1 || !req.components.is_empty()) {
        return Err(Error::invalid("suites", "an exact path (ending in /) stands alone, without components"));
    }
    if !exact && req.components.is_empty() {
        return Err(Error::invalid("components", "give at least one component"));
    }
    if req.components.len() > 16 || req.components.iter().any(|c| !word_ok(c, b"._-/")) {
        return Err(Error::invalid("components", "components are words of letters, digits and ._-/"));
    }
    if req.architectures.len() > 8 || req.architectures.iter().any(|a| !word_ok(a, b"-")) {
        return Err(Error::invalid("architectures", "architectures are words such as amd64 or arm64"));
    }
    let (keys, armored) = keys::preview(req.key.as_bytes(), now_ms()).map_err(|e| Error::new(StatusCode::BAD_REQUEST, error_codes::KEY_INVALID, e).field("key"))?;
    let norm = |f: &str| f.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_uppercase();
    let mut want: Vec<String> = req.fingerprints.iter().map(|f| norm(f)).collect();
    let mut have: Vec<String> = keys.iter().map(|k| k.fingerprint.clone()).collect();
    want.sort();
    have.sort();
    if want != have {
        return Err(Error::new(StatusCode::BAD_REQUEST, error_codes::KEY_INVALID, "the key's fingerprints are not the ones confirmed".into()).field("fingerprints"));
    }
    if keys.iter().all(|k| k.expired) {
        return Err(Error::new(StatusCode::BAD_REQUEST, error_codes::KEY_INVALID, "every key of the keyring has expired".into()).field("key"));
    }
    let arch = if req.architectures.is_empty() { vec![dpkg_arch().to_string()] } else { req.architectures.clone() };
    let mut text = format!(
        "# Added by rynas (System > Software repositories)\nTypes: {}\nURIs: {}\nSuites: {}\n",
        if req.sources { "deb deb-src" } else { "deb" },
        req.uris.iter().map(|u| u.trim()).collect::<Vec<_>>().join(" "),
        req.suites.join(" ")
    );
    if !exact {
        text.push_str(&format!("Components: {}\n", req.components.join(" ")));
    }
    text.push_str(&format!("Architectures: {}\nSigned-By: {key}\n", arch.join(" ")));
    let entry = sources::parse_deb822(&text).ok().and_then(|mut v| v.pop()).ok_or_else(|| Error::internal("the new source does not parse"))?;
    Ok((
        vec![
            Change { path: key, before: String::new(), action: Action::Write(armored) },
            Change { path: src, before: String::new(), action: Action::Write(text) },
        ],
        entry,
    ))
}

/// The plan of removing a third-party file (and its keyring in `/etc/apt/keyrings` when no
/// other source names it).
pub fn plan_remove(root: &Path, v: &View, name: &str, digest: &str) -> Result<Vec<Change>, Error> {
    if digest != v.scan.digest {
        return Err(stale());
    }
    let path = format!("{LIST_DIR}/{name}");
    let fi = v.scan.files.iter().position(|f| f.path == path).ok_or_else(|| Error::not_found(format!("no sources file {path}")))?;
    let f = &v.scan.files[fi];
    let packaged = v.packages.contains_key(&f.path);
    let classes = v.classes.get(fi).cloned().unwrap_or_default();
    if let Some(c) = classes.iter().find(|c| c.kind != RepoKind::ThirdParty) {
        writable(c)?;
        return Err(read_only("only third-party files can be removed: disable the Debian entries instead"));
    }
    if packaged || f.path == official::DEBIAN_FILE || classify::rynas_file(f).is_some() || classify::system_layer_file(f, &Context { codename: None, system_layer: v.system_layer, packages: &v.packages }) {
        return Err(read_only("this file belongs to a package or to rynas: disable its entries instead"));
    }
    let mut out = vec![Change { path: f.path.clone(), before: f.text.clone(), action: Action::Remove }];
    let others: Vec<&str> = v.scan.files.iter().filter(|x| x.path != f.path).flat_map(|x| x.entries.iter().filter_map(|e| e.signed_by())).collect();
    let mut keys: Vec<&str> = f.entries.iter().filter_map(|e| e.signed_by()).filter(|k| k.starts_with(&format!("{KEYRINGS}/")) && !k.contains("..")).collect();
    keys.sort_unstable();
    keys.dedup();
    for k in keys {
        if !others.contains(&k) && files::under_root(root, Path::new(k)).is_file() {
            out.push(Change { path: k.to_string(), before: String::new(), action: Action::Remove });
        }
    }
    Ok(out)
}

fn preview_of(root: &Path, changes: &[Change]) -> RepoPreview {
    RepoPreview { changes: changes.iter().map(|c| c.to_api(files::under_root(root, Path::new(&c.path)).exists())).collect() }
}

// ------------------------------------------------------------------ notification

/// `repositories:unsupported` firing while the configuration is outside what rynas
/// supports, resolved otherwise.
pub fn notify(u: &Updates, st: &RepositoriesState) {
    if !st.available {
        return;
    }
    if st.unsupported.is_empty() {
        u.notifier.emit(code(
            resolved(NotifyCategory::Update, "repositories", NOTIFY_KEY, "The software sources are supported"),
            "repositories.supported",
        ));
        return;
    }
    let list = st.unsupported.join(", ");
    let mut ev = firing(
        NotifyCategory::Update,
        NotifySeverity::Warning,
        "repositories",
        NOTIFY_KEY,
        "Unsupported software sources",
    );
    ev.body = format!("Packages from {list} are not tested with rynas; updates and support may break.");
    ev.actions = vec![EventAction { label: "Open software repositories".into(), url: "/system/repositories".into() }];
    u.notifier.emit(field(code(ev, "repositories.unsupported"), "sources", list));
}

/// Reads the state and notifies (after a task, by the daily check).
pub async fn notify_now(u: &Arc<Updates>) {
    let me = u.clone();
    if let Ok(st) = tokio::task::spawn_blocking(move || state(&me.config_root, &me.state_dir, me.system_layer())).await {
        notify(u, &st);
    }
}

// ------------------------------------------------------------------ routes

pub fn router(st: UpdState) -> Router {
    Router::new()
        .route("/v1/repositories", get(get_state))
        .route("/v1/repositories/entries/toggle", post(toggle))
        .route("/v1/repositories/official", put(put_official))
        .route("/v1/repositories/third-party", post(add))
        .route("/v1/repositories/third-party/{file}", delete(remove))
        .route("/v1/repositories/key-preview", post(key_preview))
        .route("/v1/repositories/check", post(check))
        .route("/v1/repositories/mirror-test", post(mirror_test))
        .route("/v1/repositories/modernize", post(modernize))
        .with_state(st)
}

fn upd(st: &UpdState) -> Res<Arc<Updates>> {
    st.clone().ok_or_else(|| {
        Error::new(StatusCode::SERVICE_UNAVAILABLE, error_codes::BACKEND_UNAVAILABLE, "updates are not enabled in this rynasd".into())
    })
}

/// The service and the administrator asking.
fn admin(st: &UpdState, hd: &HeaderMap) -> Res<(Arc<Updates>, String)> {
    let (u, who) = (upd(st)?, user(hd)?);
    u.ops.admins.require_admin(&who, "Software repositories")?;
    Ok((u, who))
}

async fn view(u: &Arc<Updates>) -> Res<View> {
    let (root, layer) = (u.config_root.clone(), u.system_layer());
    tokio::task::spawn_blocking(move || View::read(&root, layer)).await.map_err(Error::internal)
}

async fn get_state(State(st): State<UpdState>, hd: HeaderMap) -> Res<Json<RepositoriesState>> {
    let (u, _) = admin(&st, &hd)?;
    let me = u.clone();
    let mut r = tokio::task::spawn_blocking(move || state(&me.config_root, &me.state_dir, me.system_layer())).await.map_err(Error::internal)?;
    r.running_task = u.running_task();
    Ok(Json(r))
}

/// Starts the write task of `changes`: the digest is checked again when it starts, then
/// the files are applied and apt-get update runs.
fn spawn_write(u: &Arc<Updates>, who: &str, what: &str, digest: String, changes: Vec<Change>) -> Res<Response> {
    if changes.is_empty() {
        return Err(conflict("nothing to change"));
    }
    tracing::info!(target: "rynas::audit", "repositories {what} by {who}: {}", changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>().join(", "));
    u.spawn("updates.repositories", who, move |u, ctx| async move {
        let (root, state_dir) = (u.config_root.clone(), u.state_dir.clone());
        let ctx2 = ctx.clone();
        tokio::task::spawn_blocking(move || {
            if sources::scan(&root).digest != digest {
                return Err("the apt sources changed meanwhile: nothing done, read them again".to_string());
            }
            apply(&root, &state_dir, &changes, &|l| ctx2.log(l)).map(|_| ())
        })
        .await
        .map_err(|e| e.to_string())??;
        update_after(&u, &ctx).await?;
        notify_now(&u).await;
        Ok(())
    })
}

async fn toggle(State(st): State<UpdState>, hd: HeaderMap, body: Result<Json<ToggleRequest>, JsonRejection>) -> Res<Response> {
    let (u, who) = admin(&st, &hd)?;
    let Json(req) = body?;
    let v = view(&u).await?;
    let changes = plan_toggle(&v, &req)?;
    spawn_write(&u, &who, if req.enabled { "enable" } else { "disable" }, req.digest, changes)
}

async fn put_official(State(st): State<UpdState>, hd: HeaderMap, body: Result<Json<OfficialRequest>, JsonRejection>) -> Res<Response> {
    let (u, who) = admin(&st, &hd)?;
    let Json(req) = body?;
    let v = view(&u).await?;
    let changes = plan_official(&u.config_root, &v, &req)?;
    if req.preview {
        return Ok(Json(preview_of(&u.config_root, &changes)).into_response());
    }
    spawn_write(&u, &who, "official", req.digest, changes)
}

async fn modernize(State(st): State<UpdState>, hd: HeaderMap, body: Result<Json<ModernizeRequest>, JsonRejection>) -> Res<Response> {
    let (u, who) = admin(&st, &hd)?;
    let Json(req) = body?;
    let v = view(&u).await?;
    let changes = plan_modernize(&u.config_root, &v, &req)?;
    if req.preview {
        return Ok(Json(preview_of(&u.config_root, &changes)).into_response());
    }
    spawn_write(&u, &who, "modernize", req.digest, changes)
}

async fn add(State(st): State<UpdState>, hd: HeaderMap, body: Result<Json<AddRepoRequest>, JsonRejection>) -> Res<Response> {
    let (u, who) = admin(&st, &hd)?;
    let Json(req) = body?;
    let v = view(&u).await?;
    let (changes, entry) = plan_add(&u.config_root, &v, &req)?;
    tracing::info!(target: "rynas::audit", "repositories add {} ({}) by {who}", req.name, req.uris.join(" "));
    let digest = req.digest.clone();
    u.spawn("updates.repositories", &who, move |u, ctx| async move {
        let (root, state_dir) = (u.config_root.clone(), u.state_dir.clone());
        let ctx2 = ctx.clone();
        let written = changes.clone();
        tokio::task::spawn_blocking(move || {
            if sources::scan(&root).digest != digest {
                return Err("the apt sources changed meanwhile: nothing done, read them again".to_string());
            }
            apply(&root, &state_dir, &changes, &|l| ctx2.log(l)).map(|_| ())
        })
        .await
        .map_err(|e| e.to_string())??;
        let check = update_after(&u, &ctx).await?;
        let failed = new_source_failures(&entry, &check);
        if !failed.is_empty() {
            ctx.log("the new source does not work: removing it again");
            rollback(&u.config_root, &written);
            // the source is gone: its problems stay in the task's log, not on the page
            save_check(&u.state_dir, &without_source(check, &entry));
            notify_now(&u).await;
            return Err(format!(
                "the new source was removed again: {}",
                failed.iter().map(|f| f.message.as_str()).collect::<Vec<_>>().join("; ")
            ));
        }
        notify_now(&u).await;
        Ok(())
    })
}

/// `check` without the problems of `e`'s URIs (a new source removed again).
pub fn without_source(mut check: RepoCheck, e: &Entry) -> RepoCheck {
    let ours = |uri: &str| e.uris.iter().any(|u| u.trim_end_matches('/') == uri.trim_end_matches('/'));
    check.errors.retain(|x| !ours(&x.uri));
    check.ok = check.errors.iter().all(|x| !x.error);
    check
}

/// Removes the files an add wrote.
pub fn rollback(root: &Path, written: &[Change]) {
    for c in written {
        if matches!(c.action, Action::Write(_)) {
            let _ = std::fs::remove_file(files::under_root(root, Path::new(&c.path)));
        }
    }
}

fn query_digest(q: Option<String>) -> Res<String> {
    q.unwrap_or_default()
        .split('&')
        .find_map(|kv| kv.strip_prefix("digest=").map(String::from))
        .filter(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| Error::invalid("digest", "the digest of the sources read is needed"))
}

async fn remove(State(st): State<UpdState>, hd: HeaderMap, UrlPath(file): UrlPath<String>, RawQuery(q): RawQuery) -> Res<Response> {
    let (u, who) = admin(&st, &hd)?;
    if !apt_file_name_ok(&file) || !(file.ends_with(".list") || file.ends_with(".sources")) {
        return Err(Error::bad_request("not a sources file name"));
    }
    let digest = query_digest(q)?;
    let v = view(&u).await?;
    let changes = plan_remove(&u.config_root, &v, &file, &digest)?;
    spawn_write(&u, &who, "remove", digest, changes)
}

async fn check(State(st): State<UpdState>, hd: HeaderMap) -> Res<Response> {
    let (u, who) = admin(&st, &hd)?;
    if !u.apt_present() {
        return Err(Error::not_found("apt is not installed on this machine"));
    }
    u.spawn("updates.sources-check", &who, |u, ctx| async move {
        let c = update_after(&u, &ctx).await?;
        ctx.log(&format!("{} problems found", c.errors.len()));
        notify_now(&u).await;
        Ok(())
    })
}

// ------------------------------------------------------------------ key preview

async fn key_preview(State(st): State<UpdState>, hd: HeaderMap, body: Result<Json<KeyPreviewRequest>, JsonRejection>) -> Res<Json<KeyPreview>> {
    let _ = admin(&st, &hd)?;
    let Json(req) = body?;
    let data = match (req.armored.filter(|a| !a.trim().is_empty()), req.url.filter(|u| !u.trim().is_empty())) {
        (Some(a), None) => a.into_bytes(),
        (None, Some(url)) => download_key(url.trim()).await?,
        _ => return Err(Error::invalid("armored", "paste the key or give its address, not both")),
    };
    let (keys, armored) = keys::preview(&data, now_ms()).map_err(|e| Error::new(StatusCode::BAD_REQUEST, error_codes::KEY_INVALID, e).field("armored"))?;
    Ok(Json(KeyPreview { keys, armored }))
}

/// Downloads a key over https (at most [`KEY_MAX_BYTES`]).
async fn download_key(url: &str) -> Res<Vec<u8>> {
    if !url.starts_with("https://") || !official::valid_mirror(url) {
        return Err(Error::invalid("url", "the key's address must start with https://"));
    }
    let fail = |m: String| Error::new(StatusCode::BAD_GATEWAY, error_codes::REMOTE_UNREACHABLE, format!("cannot download the key: {m}")).field("url");
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .https_only(true)
        .user_agent(concat!("rynas/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| fail(e.to_string()))?;
    let mut resp = client.get(url).send().await.map_err(|e| fail(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(fail(format!("HTTP {}", resp.status().as_u16())));
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| fail(e.to_string()))? {
        out.extend_from_slice(&chunk);
        if out.len() > KEY_MAX_BYTES {
            return Err(fail(format!("larger than {} KiB", KEY_MAX_BYTES / 1024)));
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ mirror test

/// The time limits of one mirror: the name lookup alone (so a slow resolver reports `dns`, not
/// a generic timeout), the HTTP request (connect, TLS, answer), and the whole test.
#[derive(Debug, Clone, Copy)]
struct MirrorLimits {
    dns: Duration,
    http: Duration,
    total: Duration,
}

/// 3 s covers a resolver retry after a lost reply (glibc with `timeout:2`); 3 + 5 s bounds the
/// test of every mirror (they run in parallel) to 9 s.
const MIRROR_LIMITS: MirrorLimits = MirrorLimits { dns: Duration::from_secs(3), http: Duration::from_secs(5), total: Duration::from_secs(9) };

async fn mirror_test(State(st): State<UpdState>, hd: HeaderMap, body: Result<Json<MirrorTestRequest>, JsonRejection>) -> Res<Json<MirrorTest>> {
    let (u, _) = admin(&st, &hd)?;
    let Json(req) = body?;
    if req.mirrors.len() > 5 {
        return Err(Error::invalid("mirrors", "at most five mirrors"));
    }
    let mut list = vec![official::DEFAULT_MIRROR.to_string()];
    for m in &req.mirrors {
        let m = m.trim().trim_end_matches('/').to_string();
        if !official::valid_mirror(&m) {
            return Err(Error::invalid("mirrors", format!("{m} is not an http:// or https:// address")));
        }
        if !list.contains(&m) {
            list.push(m);
        }
    }
    let os = std::fs::read_to_string(files::under_root(&u.config_root, Path::new("/etc/os-release"))).unwrap_or_default();
    let codename = crate::backports::debian_codename(&os).unwrap_or_else(|| "stable".into());
    if u.dry {
        let results = list.into_iter().enumerate().map(|(i, uri)| MirrorResult { uri, ms: Some(40 + 25 * i as u64), error: None }).collect();
        return Ok(Json(MirrorTest { results }));
    }
    Ok(Json(MirrorTest { results: test_mirrors(list, &codename).await }))
}

async fn test_mirrors(list: Vec<String>, codename: &str) -> Vec<MirrorResult> {
    test_mirrors_with(list, codename, MIRROR_LIMITS).await
}

async fn test_mirrors_with(list: Vec<String>, codename: &str, lim: MirrorLimits) -> Vec<MirrorResult> {
    let one = |uri: String| {
        let url = format!("{uri}/dists/{codename}/Release");
        async move {
            // a mirror that is late keeps its own verdict; the others keep theirs
            let r = match tokio::time::timeout(lim.total, test_mirror(&url, lim)).await {
                Ok(r) => r,
                Err(_) => Err(MirrorError::new(MirrorErrorCode::Timeout, format!("no answer within {} s", lim.total.as_secs_f32()))),
            };
            match r {
                Ok(ms) => MirrorResult { uri, ms: Some(ms), error: None },
                Err(e) => MirrorResult { uri, ms: None, error: Some(e) },
            }
        }
    };
    let mut results = futures_util::future::join_all(list.into_iter().map(one)).await;
    results.sort_by_key(|r| r.ms.unwrap_or(u64::MAX));
    results
}

/// The milliseconds to the Release file at `url` (name lookup and request), or why it failed.
async fn test_mirror(url: &str, lim: MirrorLimits) -> Result<u64, MirrorError> {
    let parsed = reqwest::Url::parse(url).map_err(|e| MirrorError::new(MirrorErrorCode::Other, e.to_string()))?;
    let host = parsed.host_str().unwrap_or("").to_string();
    let port = parsed.port_or_known_default().unwrap_or(80);
    let mut builder = reqwest::Client::builder().connect_timeout(lim.http).timeout(lim.http).user_agent(concat!("rynas/", env!("CARGO_PKG_VERSION")));
    let mut spent = Duration::ZERO;
    if host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_err() {
        let t = std::time::Instant::now();
        let addrs: Vec<std::net::SocketAddr> = match tokio::time::timeout(lim.dns, tokio::net::lookup_host((host.as_str(), port))).await {
            Err(_) => return Err(MirrorError::new(MirrorErrorCode::Dns, format!("the resolver did not answer within {} s", lim.dns.as_secs_f32()))),
            Ok(Err(e)) => return Err(MirrorError::new(MirrorErrorCode::Dns, e.to_string())),
            Ok(Ok(a)) => a.collect(),
        };
        if addrs.is_empty() {
            return Err(MirrorError::new(MirrorErrorCode::Dns, format!("{host} has no address")));
        }
        spent = t.elapsed();
        // the request uses these addresses: no second lookup
        builder = builder.resolve_to_addrs(&host, &addrs);
    }
    let client = builder.build().map_err(|e| MirrorError::new(MirrorErrorCode::Other, error_detail(&e)))?;
    let t = std::time::Instant::now();
    let resp = client.get(url).send().await.map_err(|e| reqwest_mirror_error(&e))?;
    if !resp.status().is_success() {
        return Err(MirrorError::http(resp.status().as_u16()));
    }
    Ok((spent + t.elapsed()).as_millis() as u64)
}

/// The causes of a reqwest error, without the request's URL (which could name "tls" or "ssl").
fn error_detail(e: &reqwest::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        let m = s.to_string();
        if !parts.iter().any(|p| p.contains(&m)) {
            parts.push(m);
        }
        src = s.source();
    }
    if parts.is_empty() { e.to_string() } else { parts.join(": ") }
}

fn reqwest_mirror_error(e: &reqwest::Error) -> MirrorError {
    let detail = error_detail(e);
    MirrorError::new(mirror_error_code(e.is_timeout(), e.is_connect(), &detail), detail)
}

/// The code of a failed request from reqwest's flags and its causes.
fn mirror_error_code(timeout: bool, connect: bool, detail: &str) -> MirrorErrorCode {
    let d = detail.to_ascii_lowercase();
    let tls = ["ssl", "tls", "certificate", "handshake"].iter().any(|w| d.contains(w));
    if timeout || d.contains("timed out") {
        MirrorErrorCode::Timeout
    } else if d.contains("dns error") || d.contains("failed to lookup address") {
        MirrorErrorCode::Dns
    } else if tls {
        MirrorErrorCode::Tls
    } else if connect {
        MirrorErrorCode::Connect
    } else {
        MirrorErrorCode::Other
    }
}

#[cfg(test)]
mod tests;
