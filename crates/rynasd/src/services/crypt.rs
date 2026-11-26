//! Encrypted shared folders (D-CRYPT E1, `docs/phase2/encrypted-folders.md`).
//!
//! - `GET /v1/crypt`, `POST /v1/crypt/install` (task `crypt.install`, administrators);
//! - `POST /v1/crypt/folders` (task `crypt.create`: ciphertext, init, mount),
//!   `POST /v1/crypt/folders/{volume}/{name}/reveal-recovery-key` (to the administrator who
//!   created it, until they confirm with `.../recovery-key-saved`, at most 7 days; kept in a
//!   root-only file meanwhile, so a closed browser does not lose it; `GET
//!   /v1/crypt/recovery-pending` lists the caller's waiting keys) and `.../recovery-key`
//!   (again, with the password);
//! - on an applied encrypted share: `POST /v1/shares/{share}/unlock`, `.../lock`,
//!   `.../crypt/password`, `PUT .../crypt/auto-unlock`;
//! - `POST /v1/crypt/auto-unlock` (root peers: the boot unit, before Docker).
//!
//! Every change is for administrators, checked here as well as in rynas-web.
//!
//! Passwords arrive in the request body, become [`Secret`]s (wiped on drop) at once and go
//! to [`rynas_services::crypt`], which hands them to gocryptfs on standard input or as a
//! systemd credential. Wrong passwords are answered after a pause; unlocks are serialised by
//! the backend.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
use rynas_notify::Notifier;
use rynas_notify::event::{code, field, oneshot};

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rynas_api_types::storage::RecoveryKey;
use rynas_api_types::{
    CryptAutoUnlockRequest, CryptCreateRequest, CryptInfo, CryptPasswordRequest, CryptPasswordResponse,
    CryptRecoveryKeyRequest, CryptState, CryptUnlockRequest, CryptUnlockResponse, Share, ShareView, SharesConfig,
    TaskStarted, error_codes,
};
use rynas_services::SHARES_KEY;
use rynas_services::accounts::Secret;
use rynas_services::crypt::{self, CryptError};
use rynas_services::shares::{NFS_SERVER, SMBD};

use super::{Services, SvcState, bad, blocking, conflict, docs, internal, json_body, not_found, share_view, svc, unit_running};
use crate::ops::{Error, Res, user};

/// How long the recovery key of a new folder waits for its creator to confirm it is saved
/// (afterwards it is shown again only with the folder's password).
const RECOVERY_TTL_MS: u64 = 7 * 24 * 3600 * 1000;
/// Recovery keys kept at once.
const MAX_RECOVERY_KEYS: usize = 16;
/// Pause before answering a wrong password.
const WRONG_PASSWORD_PAUSE: Duration = Duration::from_secs(1);

/// The recovery key of a new folder until its creator confirms it is saved: handed out only
/// to the administrator who created the folder, never logged nor in a task log.
#[derive(serde::Serialize, serde::Deserialize)]
struct Kept {
    path: String,
    volume: String,
    name: String,
    owner: String,
    created_ms: u64,
    expires_ms: u64,
    /// Shown once already: showing it again needs a fresh sign-in (password, TOTP).
    #[serde(default)]
    revealed: bool,
    #[serde(serialize_with = "secret_str", deserialize_with = "secret_de")]
    key: Secret,
}

fn secret_str<S: serde::Serializer>(k: &Secret, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(k.expose())
}

fn secret_de<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Secret, D::Error> {
    <String as serde::Deserialize>::deserialize(d).map(Secret::new)
}

/// Where the kept keys survive a restart of rynasd: tmpfs (`/run/rynas`), never a snapshotted
/// or backed-up place (a system restore point of `/var/lib` would keep a copy for weeks); a
/// reboot loses them (the key is then shown again with the folder's password).
const RECOVERY_FILE: &str = "crypt/recovery-pending.json";
/// Where 0.9.15 kept them (inside the root subvolume): moved to [`RECOVERY_FILE`] under the
/// run folder at start and wiped.
const OLD_RECOVERY_FILE: &str = "crypt/recovery-pending.json";

/// The kept keys: memory, and their file once rynasd set it ([`load_recovery_keys`]; tests:
/// memory only). Expiry is a one-shot timer at the earliest `expires_ms`, armed after every
/// change ([`RecoveryStore::arm`]): no periodic loop.
#[derive(Default)]
struct RecoveryStore {
    keys: Mutex<HashMap<String, Kept>>,
    file: Mutex<Option<std::path::PathBuf>>,
    /// Bumped at every arming: an older timer that wakes does nothing.
    timer: std::sync::atomic::AtomicU64,
}

static STORE: std::sync::LazyLock<RecoveryStore> = std::sync::LazyLock::new(RecoveryStore::default);

/// Where failed automatic unlocks are reported (set once the engine runs).
static NOTIFIER: OnceLock<Arc<dyn Notifier>> = OnceLock::new();

/// The notification engine, for the automatic unlock's failures.
pub fn set_notifier(n: Arc<dyn Notifier>) {
    let _ = NOTIFIER.set(n);
}

/// At rynasd's start: the kept keys from `<run_dir>/crypt/recovery-pending.json` (tmpfs), the
/// ones an older rynasd left in `<state_dir>/crypt/recovery-pending.json` moved there and
/// that file wiped; expired ones dropped.
pub fn load_recovery_keys(run_dir: &std::path::Path, state_dir: &std::path::Path) {
    STORE.open(&run_dir.join(RECOVERY_FILE), Some(&state_dir.join(OLD_RECOVERY_FILE)));
    STORE.arm();
}

/// Reads one file of kept keys; an unreadable one is removed (its content never logged).
fn read_kept(f: &std::path::Path) -> Vec<Kept> {
    let Ok(bytes) = std::fs::read(f).map(zeroize::Zeroizing::new) else {
        return Vec::new();
    };
    match serde_json::from_slice::<Vec<Kept>>(&bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("encrypted folders: {} unreadable ({:?}), removed", f.display(), e.classify());
            wipe_file(f);
            Vec::new()
        }
    }
}

/// Overwrites `f` with zeros, then removes it (snapshots taken before keep their copy: the
/// reason the file no longer lives there).
fn wipe_file(f: &std::path::Path) {
    if let Ok(m) = std::fs::symlink_metadata(f)
        && m.is_file()
    {
        use std::io::Write;
        if let Ok(mut h) = std::fs::OpenOptions::new().write(true).open(f) {
            let _ = h.write_all(&vec![0u8; m.len() as usize]);
            let _ = h.sync_all();
        }
    }
    if let Err(e) = std::fs::remove_file(f)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("encrypted folders: {} not removed: {e}", f.display());
    }
}

impl RecoveryStore {
    /// Sets the file, reads it (and `old`, then wiped), drops what expired, writes it back.
    fn open(&self, file: &std::path::Path, old: Option<&std::path::Path>) {
        *self.file.lock().unwrap_or_else(|e| e.into_inner()) = Some(file.to_path_buf());
        let mut kept = read_kept(file);
        if let Some(o) = old.filter(|o| *o != file && o.exists()) {
            kept.extend(read_kept(o));
            wipe_file(o);
            tracing::info!("encrypted folders: kept recovery keys moved from {} to {}", o.display(), file.display());
        }
        let now = rynas_tasks::now_ms();
        let mut m = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        for k in kept.into_iter().filter(|k| k.expires_ms > now) {
            m.insert(k.path.clone(), k);
        }
        self.save(&m);
    }

    /// Writes the kept keys to their file (none: the file goes): serialised into a buffer
    /// sized in advance and wiped afterwards, written 0600 (no symlink followed, a temporary
    /// created exclusively) in a 0700 folder.
    fn save(&self, m: &HashMap<String, Kept>) {
        let Some(f) = self.file.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return;
        };
        let r = if m.is_empty() {
            match std::fs::remove_file(&f) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            let list: Vec<&Kept> = m.values().collect();
            // no reallocation leaves a copy behind: room for every entry from the start
            let mut data = zeroize::Zeroizing::new(Vec::with_capacity(512 * list.len() + 64));
            serde_json::to_writer(&mut *data, &list).map_err(std::io::Error::other).and_then(|()| {
                let dir = f.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
                std::fs::create_dir_all(dir)?;
                std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
                crate::tls::write_atomic(&f, &data, 0o600, None)
            })
        };
        if let Err(e) = r {
            tracing::warn!("encrypted folders: {} not written: {e}", f.display());
        }
    }

    fn keep(&self, path: &str, volume: &str, name: &str, who: &str, key: Secret) {
        let now = rynas_tasks::now_ms();
        let mut m = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|_, k| k.expires_ms > now);
        if m.len() >= MAX_RECOVERY_KEYS
            && let Some(oldest) = m.values().min_by_key(|k| k.expires_ms).map(|k| k.path.clone())
        {
            m.remove(&oldest);
        }
        let k = Kept {
            path: path.to_string(),
            volume: volume.to_string(),
            name: name.to_string(),
            owner: who.to_string(),
            created_ms: now,
            expires_ms: now + RECOVERY_TTL_MS,
            revealed: false,
            key,
        };
        m.insert(path.to_string(), k);
        self.save(&m);
    }

    /// The kept key of `path` for the user who created the folder (it stays until
    /// [`RecoveryStore::forget`]). The first time freely; again only with `fresh` (a sign-in
    /// within the re-authentication window), else `Err(())`. `Ok(None)`: none kept for `who`.
    fn reveal(&self, path: &str, who: &str, fresh: bool) -> Result<Option<Secret>, ()> {
        let now = rynas_tasks::now_ms();
        let mut m = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let Some(k) = m.get_mut(path).filter(|k| k.expires_ms > now && k.owner == who) else {
            return Ok(None);
        };
        if k.revealed && !fresh {
            return Err(());
        }
        let first = !k.revealed;
        k.revealed = true;
        let key = k.key.clone();
        if first {
            self.save(&m);
        }
        Ok(Some(key))
    }

    /// Wipes the kept key of `path` (its creator saved it, or the folder is gone): whether
    /// there was one for `who` (`None`: any user's).
    fn forget(&self, path: &str, who: Option<&str>) -> bool {
        let mut m = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        if who.is_some_and(|w| m.get(path).is_some_and(|k| k.owner != w)) {
            return false;
        }
        let gone = m.remove(path).is_some();
        if gone {
            self.save(&m);
        }
        gone
    }

    /// The keys `who` has not confirmed yet (without the keys), oldest first. Expired entries
    /// go; an entry goes for its folder only when `gone(path, volume)` says so (the volume is
    /// mounted and the folder's configuration is not there): a volume that is not mounted
    /// keeps its entries.
    fn pending(&self, who: &str, gone: impl Fn(&str, &str) -> bool) -> Vec<rynas_api_types::CryptPendingKey> {
        let now = rynas_tasks::now_ms();
        let mut m = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let before = m.len();
        m.retain(|_, k| k.expires_ms > now && (k.owner != who || !gone(&k.path, &k.volume)));
        if m.len() != before {
            self.save(&m);
        }
        let mut v: Vec<_> = m
            .values()
            .filter(|k| k.owner == who)
            .map(|k| rynas_api_types::CryptPendingKey {
                volume: k.volume.clone(),
                name: k.name.clone(),
                path: k.path.clone(),
                created_ms: k.created_ms,
                expires_ms: k.expires_ms,
            })
            .collect();
        v.sort_by_key(|k| k.created_ms);
        v
    }

    /// Drops the expired entries; the earliest expiry left.
    fn purge(&self) -> Option<u64> {
        let now = rynas_tasks::now_ms();
        let mut m = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let before = m.len();
        m.retain(|_, k| k.expires_ms > now);
        if m.len() != before {
            self.save(&m);
        }
        m.values().map(|k| k.expires_ms).min()
    }

    /// One timer at the earliest expiry: it purges and arms the next one. Needs a runtime
    /// (none: nothing armed, the next change or read purges).
    fn arm(&'static self) {
        let Some(at) = self.purge() else {
            self.timer.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return;
        };
        let generation = self.timer.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        rt.spawn(async move {
            let wait = at.saturating_sub(rynas_tasks::now_ms());
            tokio::time::sleep(Duration::from_millis(wait)).await;
            if self.timer.load(std::sync::atomic::Ordering::SeqCst) == generation {
                self.arm();
            }
        });
    }
}

fn keep_recovery_key(path: &str, volume: &str, name: &str, who: &str, key: Secret) {
    STORE.keep(path, volume, name, who, key);
    STORE.arm();
}

fn forget_recovery_key(path: &str, who: Option<&str>) -> bool {
    let gone = STORE.forget(path, who);
    if gone {
        STORE.arm();
    }
    gone
}

/// A secret field of a request: moved out (no copy left in the request) and checked
/// non-empty.
fn take_secret(field: &str, s: &mut String) -> Res<Secret> {
    let p = Secret::new(std::mem::take(s));
    if p.expose().is_empty() {
        return Err(Error::invalid(field, format!("{field} is empty")));
    }
    Ok(p)
}

/// The JSON answer of a recovery key, with the key's copy wiped once serialised.
fn key_answer(key: Secret) -> Res<Response> {
    use zeroize::Zeroize;
    let mut rk = RecoveryKey { key: key.expose().to_string() };
    let body = serde_json::to_vec(&rk).map_err(|e| internal(e.to_string()));
    rk.key.zeroize();
    Ok(([(axum::http::header::CONTENT_TYPE, "application/json")], body?).into_response())
}

fn audit(who: &str, action: &str, what: &str) {
    tracing::info!(target: "rynas::audit", "encrypted folder {action} {what} by {who}");
}

/// The answer of a backend error.
fn crypt_error(e: CryptError) -> Error {
    match e {
        CryptError::WrongPassword => bad(error_codes::CRYPT_WRONG_PASSWORD, "wrong password or recovery key"),
        CryptError::Busy(m) => conflict(error_codes::CRYPT_BUSY, format!("the folder is in use ({m}): close the files, stop the apps using it and try again")),
        CryptError::NotInstalled => conflict(error_codes::CRYPT_NOT_INSTALLED, "gocryptfs is not installed"),
        CryptError::Invalid(m) => conflict(error_codes::CONFLICT, m),
        CryptError::Failed(m) => internal(m),
    }
}

/// [`crypt_error`] after the pause a wrong password earns.
async fn answer(e: CryptError) -> Error {
    if e == CryptError::WrongPassword {
        tokio::time::sleep(WRONG_PASSWORD_PAUSE).await;
    }
    crypt_error(e)
}

/// A new password from a request field (moved out, then checked).
fn password(field: &str, p: &mut String) -> Res<Secret> {
    let s = Secret::new(std::mem::take(p));
    crypt::check_password(s.expose()).map_err(|e| Error::invalid(field, e))?;
    Ok(s)
}

/// The applied encrypted share `name`.
fn applied_share(s: &Services, owner: &str, name: &str) -> Res<Share> {
    let (_, cur, _): (SharesConfig, SharesConfig, _) = docs(&s.ops.config, owner, SHARES_KEY)?;
    let sh = cur
        .shares
        .iter()
        .find(|x| x.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| not_found(format!("applied share {name}")))?;
    if !sh.encrypted {
        return Err(conflict(error_codes::CONFLICT, format!("share {} is not an encrypted folder", sh.name)));
    }
    Ok(sh.clone())
}

/// After a lock or an unlock: NFS exports with `mp` are evaluated again; after an unlock
/// the ACL the apply skipped while the folder was locked is written.
fn after_change(s: &Services, sh: &Share, unlocked: bool) {
    if unlocked
        && let Err(e) = s.shares.write_acl(sh)
    {
        tracing::warn!("ACL of {} after its unlock: {e}", sh.path);
    }
    if sh.nfs.enabled && unit_running(s, NFS_SERVER) {
        let _ = s.ops.config.system().run(&rynas_tasks::Cmd::new("exportfs").arg("-ra"));
    }
}

pub(super) async fn info(State(st): State<SvcState>) -> Res<Json<CryptInfo>> {
    let s = svc(&st)?;
    blocking(&s, |s| Ok(s.shares.crypt().info())).await.map(Json)
}

pub(super) async fn install(State(st): State<SvcState>, h: HeaderMap) -> Res<Response> {
    let (s, who) = (svc(&st)?, user(&h)?);
    s.ops.admins.require_admin(&who, "gocryptfs install")?;
    if s.shares.crypt().info().installed {
        return Err(conflict(error_codes::ALREADY_EXISTS, "gocryptfs is already installed"));
    }
    audit(&who, "install", crypt::PACKAGE);
    let svc = s.clone();
    let id = s.ops.tasks.spawn("crypt.install", &who, move |ctx| async move {
        let c = svc.shares.crypt().clone();
        if c.simulated() {
            ctx.log(&format!("dry system, not run: apt-get install {}", crypt::PACKAGE));
            tokio::time::sleep(Duration::from_millis(600)).await;
            c.mark_installed();
            return Ok(());
        }
        ctx.log(&format!("installing {} from the Debian archive", crypt::PACKAGE));
        let cmd = crate::updates::apt(&["install", "--", crypt::PACKAGE], Duration::from_secs(1800));
        ctx.run(&cmd).await.map_err(|e| e.to_string())?;
        if !c.info().installed {
            return Err(format!("{} is still missing after the install", crypt::GOCRYPTFS));
        }
        Ok(())
    })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

/// The folder of a new encrypted folder: `<place>/<name>`.
fn new_folder_path(s: &Services, req: &CryptCreateRequest) -> Res<String> {
    if !rynas_api_types::validate::place_label(&req.volume) {
        return Err(Error::invalid("volume", format!("invalid volume {:?}", req.volume)));
    }
    if !rynas_services::validate::share_name(&req.name) {
        return Err(Error::invalid(
            "name",
            format!("invalid folder name {:?}: 1-64 letters, digits, '.', '_' or '-', starting with a letter or digit", req.name),
        ));
    }
    let base = match s.shares.volumes().into_iter().find(|(l, _)| *l == req.volume) {
        Some((_, dir)) => dir,
        None if s.shares.crypt().simulated() => rynas_api_types::storage::volume_dir(&req.volume),
        None => return Err(conflict(error_codes::CONFLICT, format!("volume {} is not mounted", req.volume))),
    };
    Ok(format!("{}/{}", base.trim_end_matches('/'), req.name))
}

pub(super) async fn create(
    State(st): State<SvcState>,
    h: HeaderMap,
    body: Result<Json<CryptCreateRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (svc(&st)?, user(&h)?);
    s.ops.admins.require_admin(&who, "encrypted folders")?;
    let mut req = json_body(body)?;
    let pw = password("password", &mut req.password)?;
    let auto = req.auto_unlock;
    let r = CryptCreateRequest { volume: req.volume.clone(), name: req.name.clone(), ..Default::default() };
    let (r_volume, r_name) = (req.volume.clone(), req.name.clone());
    drop(req);
    let path = blocking(&s, move |s| {
        let path = new_folder_path(s, &r)?;
        let info = s.shares.crypt().info();
        if !info.installed {
            return Err(crypt_error(CryptError::NotInstalled));
        }
        if !info.supported {
            return Err(conflict(
                error_codes::CONFLICT,
                format!(
                    "gocryptfs {} is too old: encrypted folders need {}.{} or newer (Debian 13 or newer)",
                    info.version.as_deref().unwrap_or("of unknown version"),
                    crypt::MIN_VERSION.0,
                    crypt::MIN_VERSION.1
                ),
            ));
        }
        if s.shares.host().probe(&path).exists || s.shares.crypt().state(&path) != CryptState::Missing {
            return Err(conflict(error_codes::ALREADY_EXISTS, format!("{path} already exists")));
        }
        Ok(path)
    })
    .await?;
    audit(&who, "create", &path);
    let svc = s.clone();
    let owner = who.clone();
    let (volume, name) = (r_volume, r_name);
    let id = s.ops.tasks.spawn_on("crypt.create", Some(&path.clone()), &who, move |ctx| async move {
        let c = ctx.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let log = |l: &str| c.log(l);
            let crypt = svc.shares.crypt();
            let made = crypt.create(&path, &pw, &log).map_err(|e| e.to_string())?;
            keep_recovery_key(&path, &volume, &name, &owner, made.key);
            log("the recovery key is kept for the administrator who created the folder until they confirm it is saved (at most 7 days), or shown again later with the password");
            svc.shares.sizes().clear();
            if made.unlock_error.is_some() {
                // kept, locked: the share can still be made; the user unlocks it later
                return Ok(());
            }
            if auto {
                // the folder exists: a failed seal is a warning, not a failed creation
                match crypt.set_auto_unlock(&path, Some(&pw), &log) {
                    Ok(()) => log("the password is sealed on this machine: the folder is unlocked at every start"),
                    Err(e) => log(&format!("warning: automatic unlock not turned on: {e}; turn it on later in the folder's settings")),
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?
    })?;
    Ok((StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response())
}

pub(super) async fn reveal(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path((volume, name)): Path<(String, String)>,
) -> Res<Response> {
    let (s, who) = (svc(&st)?, user(&h)?);
    s.ops.admins.require_admin(&who, "encrypted folders")?;
    // a sign-in within the re-authentication window, told by rynas-web (never by a client)
    let fresh = h.get(rynas_auth::ipc::AUTH_FRESH_HEADER).is_some_and(|v| v == "1");
    let r = CryptCreateRequest { volume, name, ..Default::default() };
    // the path needs the volume mounted: a folder missing there is gone for good
    let (path, gone) = blocking(&s, move |s| new_folder_path(s, &r).map(|p| { let g = s.shares.crypt().state(&p) == CryptState::Missing; (p, g) })).await?;
    if gone {
        forget_recovery_key(&path, None);
    }
    let key = match STORE.reveal(&path, &who, fresh) {
        Ok(Some(k)) => k,
        Ok(None) => {
            return Err(not_found(format!(
                "recovery key of {path} (kept for the administrator who created the folder until they confirm it is saved, at most 7 days and until the next restart; it can be shown again with the password)"
            )));
        }
        Err(()) => {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                error_codes::REAUTH_REQUIRED,
                "the recovery key was shown already: sign in again (password, and TOTP code when enabled) to show it again".into(),
            ));
        }
    };
    audit(&who, "recovery key revealed", &path);
    notify_revealed(&who, &path);
    key_answer(key)
}

/// A security notification for each reveal of a kept recovery key (who and which folder,
/// never the key).
fn notify_revealed(who: &str, path: &str) {
    let Some(n) = NOTIFIER.get() else { return };
    let mut ev = oneshot(
        NotifyCategory::Security,
        NotifySeverity::Notice,
        "crypt",
        format!("crypt-recovery-revealed:{path}:{}", rynas_tasks::now_ms()),
        format!("Recovery key of {path} shown to {who}"),
    );
    ev.body = format!("The recovery key of the new encrypted folder {path} was shown to {who}.");
    let ev = field(field(code(ev, "crypt.recovery_revealed"), "user", who), "path", path);
    n.emit(ev);
}

/// `GET /v1/crypt/recovery-pending`: the caller's new folders whose recovery key waits for
/// "saved" (no key in the answer). An entry goes for its folder only when the volume is
/// mounted and the folder's configuration is gone (deleted), never because the volume is not
/// mounted.
pub(super) async fn recovery_pending(State(st): State<SvcState>, h: HeaderMap) -> Res<Json<Vec<rynas_api_types::CryptPendingKey>>> {
    let (s, who) = (svc(&st)?, user(&h)?);
    s.ops.admins.require_admin(&who, "encrypted folders")?;
    let v = blocking(&s, move |s| {
        let mounted: Vec<String> = s.shares.volumes().into_iter().map(|(l, _)| l).collect();
        Ok(STORE.pending(&who, |path, volume| mounted.iter().any(|l| l == volume) && s.shares.crypt().state(path) == CryptState::Missing))
    })
    .await?;
    STORE.arm();
    Ok(Json(v))
}

/// `POST /v1/crypt/folders/{volume}/{name}/recovery-key-saved`: the creator saved the key; the
/// kept copy is wiped (memory and file). 204 also when nothing was kept.
pub(super) async fn recovery_saved(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path((volume, name)): Path<(String, String)>,
) -> Res<StatusCode> {
    let (s, who) = (svc(&st)?, user(&h)?);
    s.ops.admins.require_admin(&who, "encrypted folders")?;
    let r = CryptCreateRequest { volume, name, ..Default::default() };
    let path = blocking(&s, move |s| new_folder_path(s, &r)).await?;
    if forget_recovery_key(&path, Some(&who)) {
        audit(&who, "recovery key saved, kept copy wiped", &path);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/crypt/folders/{volume}/{name}/recovery-key`: the recovery key again, read from
/// the folder's configuration with its password.
pub(super) async fn recovery_key(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path((volume, name)): Path<(String, String)>,
    body: Result<Json<CryptRecoveryKeyRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (svc(&st)?, user(&h)?);
    s.ops.admins.require_admin(&who, "encrypted folders")?;
    let mut req = json_body(body)?;
    let pw = take_secret("password", &mut req.password)?;
    drop(req);
    let r = CryptCreateRequest { volume, name, ..Default::default() };
    let w = who.clone();
    let res = blocking(&s, move |s| {
        let path = new_folder_path(s, &r)?;
        if s.shares.crypt().state(&path) == CryptState::Missing {
            return Err(not_found(format!("encrypted folder {path}")));
        }
        Ok(s.shares.crypt().recovery_key(&path, &pw, &|l| tracing::info!("{l}")).map(|k| (path, k)))
    })
    .await?;
    match res {
        Ok((path, key)) => {
            audit(&w, "recovery key shown again", &path);
            key_answer(key)
        }
        Err(e) => Err(answer(e).await),
    }
}

pub(super) async fn unlock(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<CryptUnlockRequest>, JsonRejection>,
) -> Res<Json<CryptUnlockResponse>> {
    let (s, who) = (svc(&st)?, user(&h)?);
    s.ops.admins.require_admin(&who, "encrypted folders")?;
    let mut req = json_body(body)?;
    let pw = take_secret("password", &mut req.password)?;
    drop(req);
    let w = who.clone();
    let r = blocking(&s, move |s| {
        let sh = applied_share(s, &w, &name)?;
        let crypt = s.shares.crypt();
        if crypt.state(&sh.path) == CryptState::InUse {
            // a fresh unlock stops the gocryptfs that still serves these containers
            let h = super::lock_holders(&sh.path).map_err(cannot_check)?;
            let users: Vec<String> = h.blocking.into_iter().chain(h.restart).collect();
            if !users.is_empty() {
                return Err(crypt_error(CryptError::Busy(format!(
                    "gocryptfs still serves containers started while it was unlocked: {}; stop them first",
                    users.join(", ")
                ))));
            }
        }
        if let Err(e) = crypt.unlock(&sh.path, &pw, &|l| tracing::info!("{l}")) {
            return Ok(Err(e));
        }
        audit(&w, "unlock", &sh.path);
        after_change(s, &sh, true);
        crate::syncthing::live::after_unlock(&sh.path);
        // running containers with a private bind were started while it was locked: they keep
        // the empty folder until they are restarted (a follower bind shows the mount now)
        let containers = super::lock_holders(&sh.path).map(|h| h.blocking).unwrap_or_default();
        let share = share_view(s, &w, &sh.name)?;
        Ok(Ok(CryptUnlockResponse { share, containers }))
    })
    .await?;
    match r {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(answer(e).await),
    }
}

/// Docker could not be asked which containers use a folder: a lock or unlock cannot know
/// whether the plaintext stays readable somewhere.
fn cannot_check(e: String) -> Error {
    conflict(error_codes::CONFLICT, format!("cannot check the containers using this share: {e}"))
}

/// The containers to restart around the lock of `path` ([`super::Holders::restart`]), or 409
/// while a running container holds a private copy of the mount (an unmount would succeed
/// here while the plaintext stays readable in it) or when Docker cannot be asked.
fn holders(path: &str) -> Res<Vec<String>> {
    let h = super::lock_holders(path).map_err(cannot_check)?;
    if !h.blocking.is_empty() {
        return Err(crypt_error(CryptError::Busy(format!("running containers use it: {}; stop them first", h.blocking.join(", ")))));
    }
    Ok(h.restart)
}

/// `<state>/crypt/lock-stopped.json`: the containers a lock stopped and has not started again
/// yet (set at rynasd start, [`start_left_stopped`]).
static LOCK_STOPPED: OnceLock<std::path::PathBuf> = OnceLock::new();
const LOCK_STOPPED_FILE: &str = "crypt/lock-stopped.json";

fn read_stopped(f: &std::path::Path) -> Vec<String> {
    std::fs::read(f).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Writes the list (none: the file goes).
fn write_stopped(f: &std::path::Path, names: &[String]) -> std::io::Result<()> {
    if names.is_empty() {
        return match std::fs::remove_file(f) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let dir = f.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
    std::fs::create_dir_all(dir)?;
    crate::tls::write_atomic(f, &serde_json::to_vec(names).map_err(std::io::Error::other)?, 0o600, None)
}

/// Adds (`add`) or removes names of the record in `f`.
fn update_stopped(f: &std::path::Path, names: &[String], add: bool) {
    if names.is_empty() {
        return;
    }
    let mut v = read_stopped(f);
    if add {
        for n in names {
            if !v.contains(n) {
                v.push(n.clone());
            }
        }
    } else {
        v.retain(|n| !names.contains(n));
    }
    if let Err(e) = write_stopped(f, &v) {
        tracing::warn!("encrypted folder lock: {} not written: {e}", f.display());
    }
}

fn remember_stopped(names: &[String]) {
    if let Some(f) = LOCK_STOPPED.get() {
        update_stopped(f, names, true);
    }
}

fn forget_stopped(names: &[String]) {
    if let Some(f) = LOCK_STOPPED.get() {
        update_stopped(f, names, false);
    }
}

/// When rynasd starts: the containers a lock stopped and did not start again (rynasd ended
/// in between) are started, in a thread that waits for the engine (every 10 s, 5 minutes at
/// most); each one started leaves the record.
pub fn start_left_stopped(state_dir: &std::path::Path) {
    let f = state_dir.join(LOCK_STOPPED_FILE);
    let _ = LOCK_STOPPED.set(f.clone());
    if read_stopped(&f).is_empty() {
        return;
    }
    std::thread::Builder::new()
        .name("crypt-lock-leftovers".into())
        .spawn(move || {
            for round in 0..30 {
                let left = read_stopped(&f);
                if left.is_empty() {
                    return;
                }
                let started: Vec<String> = left.iter().filter(|n| super::docker_cmd("start", n).is_ok()).cloned().collect();
                for n in &started {
                    tracing::info!("encrypted folder lock: {n} stopped by a lock that did not end, started again");
                }
                update_stopped(&f, &started, false);
                if round < 29 {
                    std::thread::sleep(std::time::Duration::from_secs(10));
                }
            }
            let left = read_stopped(&f);
            if !left.is_empty() {
                tracing::warn!("encrypted folder lock: containers stopped by a lock not started again: {}", left.join(", "));
            }
        })
        .map(|_| ())
        .unwrap_or_else(|e| tracing::warn!("encrypted folder lock leftovers: {e}"));
}

/// `docker <action>` of each container, in order: the ones done and the failures (a stop
/// ends at the first failure, a start tries every container).
async fn docker_each(action: &'static str, names: Vec<String>) -> (Vec<String>, Option<String>) {
    tokio::task::spawn_blocking(move || {
        let mut done = Vec::new();
        let mut errors = Vec::new();
        for n in names {
            match super::docker_cmd(action, &n) {
                Ok(()) => done.push(n),
                Err(e) => {
                    errors.push(format!("{n}: {e}"));
                    if action == "stop" {
                        break;
                    }
                }
            }
        }
        (done, (!errors.is_empty()).then(|| errors.join("; ")))
    })
    .await
    .unwrap_or_else(|e| (Vec::new(), Some(e.to_string())))
}

pub(super) async fn lock(State(st): State<SvcState>, h: HeaderMap, Path(name): Path<String>) -> Res<Json<ShareView>> {
    let (s, who) = (svc(&st)?, user(&h)?);
    s.ops.admins.require_admin(&who, "encrypted folders")?;
    let (w, n) = (who.clone(), name.clone());
    let (path, restart) = blocking(&s, move |s| {
        let sh = applied_share(s, &w, &n)?;
        let restart = holders(&sh.path)?;
        Ok((sh.path, restart))
    })
    .await?;
    // Syncthing folders inside it are paused first (and resumed after the unlock), so the
    // app comes back paused, not in error
    crate::syncthing::live::before_lock(&path).await;
    // containers whose follower bind was made while the folder was mounted keep the mount
    // after the host unmounts it: stopped for the lock, started again after it (from then on
    // their bind follows the folder)
    if !restart.is_empty() {
        tracing::info!("encrypted folder lock of {path}: restarting around it: {}", restart.join(", "));
    }
    // recorded before the stop: a crash of rynasd in between starts them at its next start
    remember_stopped(&restart);
    let (stopped, stop_err) = docker_each("stop", restart.clone()).await;
    forget_stopped(&restart.iter().filter(|n| !stopped.contains(n)).cloned().collect::<Vec<_>>());
    let r = match stop_err {
        Some(e) => Err(conflict(error_codes::CONFLICT, format!("a container using the share could not be stopped for the lock: {e}"))),
        None => {
            let p = path.clone();
            blocking(&s, move |s| {
                let sh = applied_share(s, &who, &name)?;
                let late = holders(&sh.path)?;
                if !late.is_empty() {
                    return Err(crypt_error(CryptError::Busy(format!("containers started meanwhile use it: {}; try again", late.join(", ")))));
                }
                let sys = s.ops.config.system();
                // Samba keeps files of its clients open: their connections to this share end first
                if sh.smb.enabled && unit_running(s, SMBD) {
                    let _ = sys.run(&rynas_tasks::Cmd::new("smbcontrol").args(["smbd", "close-share", &sh.name]));
                }
                if sh.nfs.enabled && unit_running(s, NFS_SERVER) {
                    let _ = sys.run(&rynas_tasks::Cmd::new("exportfs").arg("-f"));
                }
                let r = s.shares.crypt().lock(&sh.path, &|l| tracing::info!("{l}"));
                // the state changed even when gocryptfs still serves another namespace
                after_change(s, &sh, false);
                r.map_err(crypt_error)?;
                // last check: no running container may still see the plaintext
                if let Some(cipher) = crypt::cipher_dir(&p) {
                    let still = super::mount_holders(&cipher).map_err(cannot_check)?;
                    if !still.is_empty() {
                        return Err(crypt_error(CryptError::Busy(format!(
                            "unmounted here, but still mounted in containers: {}; stop them, then lock again",
                            still.join(", ")
                        ))));
                    }
                }
                audit(&who, "lock", &sh.path);
                share_view(s, &who, &sh.name)
            })
            .await
        }
    };
    if !stopped.is_empty() {
        let (started, e) = docker_each("start", stopped).await;
        // one that did not start stays recorded: the next start of rynasd tries again
        forget_stopped(&started);
        if let Some(e) = e {
            tracing::warn!("encrypted folder lock of {path}: a stopped container was not started again: {e}");
        }
    }
    // a failed lock resumes the Syncthing folders it paused
    crate::syncthing::live::lock_finished(&path, r.is_ok());
    r.map(Json)
}

pub(super) async fn change_password(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<CryptPasswordRequest>, JsonRejection>,
) -> Res<Json<CryptPasswordResponse>> {
    let (s, who) = (svc(&st)?, user(&h)?);
    s.ops.admins.require_admin(&who, "encrypted folders")?;
    let mut req = json_body(body)?;
    let recovery = req.recovery;
    let new = password("new_password", &mut req.new_password)?;
    let current = take_secret("current", &mut req.current)?;
    drop(req);
    let r = blocking(&s, move |s| {
        let sh = applied_share(s, &who, &name)?;
        let warning = match s.shares.crypt().change_password(&sh.path, &current, recovery, &new, &|l| tracing::info!("{l}")) {
            Ok(w) => w,
            Err(e) => return Ok(Err(e)),
        };
        audit(&who, if recovery { "password reset with the recovery key" } else { "password change" }, &sh.path);
        if let Some(w) = &warning {
            tracing::warn!("encrypted folder {}: {w}", sh.path);
        }
        Ok(Ok(CryptPasswordResponse { auto_unlock_off: warning }))
    })
    .await?;
    match r {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(answer(e).await),
    }
}

pub(super) async fn auto_unlock(
    State(st): State<SvcState>,
    h: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<CryptAutoUnlockRequest>, JsonRejection>,
) -> Res<Json<ShareView>> {
    let (s, who) = (svc(&st)?, user(&h)?);
    s.ops.admins.require_admin(&who, "encrypted folders")?;
    let mut req = json_body(body)?;
    let enabled = req.enabled;
    let pw = Secret::new(std::mem::take(&mut req.password));
    drop(req);
    if enabled && pw.expose().is_empty() {
        return Err(Error::invalid("password", "turning the automatic unlock on needs the password"));
    }
    let r = blocking(&s, move |s| {
        let sh = applied_share(s, &who, &name)?;
        let p = enabled.then_some(&pw);
        if let Err(e) = s.shares.crypt().set_auto_unlock(&sh.path, p, &|l| tracing::info!("{l}")) {
            return Ok(Err(e));
        }
        audit(&who, if enabled { "automatic unlock on" } else { "automatic unlock off" }, &sh.path);
        share_view(s, &who, &sh.name).map(Ok)
    })
    .await?;
    match r {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(answer(e).await),
    }
}

/// Unlocks the applied encrypted shares whose password is sealed on this machine (a missing
/// volume or a failure is logged and notified, the others go on). Never asks Docker: at boot
/// it runs before the engine starts (`rynas-crypt-unlock.service`). Returns the paths it
/// unlocked.
fn unlock_sealed(s: &Arc<Services>) -> Vec<String> {
    let Ok(Some(v)) = s.ops.config.applied(SHARES_KEY) else { return Vec::new() };
    let Ok(c) = serde_json::from_value::<SharesConfig>(v) else { return Vec::new() };
    let crypt = s.shares.crypt();
    let mut done = Vec::new();
    for sh in c.shares.iter().filter(|x| x.encrypted) {
        if crypt.state(&sh.path) != CryptState::Locked || !crypt.auto_unlock(&sh.path) {
            continue;
        }
        let r = crypt
            .sealed_password(&sh.path)
            .and_then(|p| p.ok_or_else(|| CryptError::Invalid("no sealed password".into())))
            .and_then(|p| crypt.unlock(&sh.path, &p, &|l| tracing::info!("{l}")));
        match r {
            Ok(()) => {
                tracing::info!(target: "rynas::audit", "encrypted folder {} unlocked at start", sh.path);
                after_change(s, sh, true);
                // at boot the integration may not run yet: its first full read does it
                crate::syncthing::live::after_unlock(&sh.path);
                done.push(sh.path.clone());
            }
            Err(e) => {
                tracing::warn!("automatic unlock of {}: {e}", sh.path);
                if let Some(n) = NOTIFIER.get() {
                    let mut ev = oneshot(
                        NotifyCategory::Storage,
                        NotifySeverity::Error,
                        "crypt",
                        format!("crypt-auto-unlock:{}", sh.path),
                        format!("Encrypted folder {} was not unlocked automatically", sh.name),
                    );
                    ev.body = format!(
                        "{} stays locked: {e}. Unlock it with its password in Shared folders, then restart the apps and containers that use it.",
                        sh.path
                    );
                    let ev = field(field(code(ev, "crypt.auto_unlock_failed"), "share", &sh.name), "path", &sh.path);
                    n.emit(field(ev, "error", e));
                }
            }
        }
    }
    done
}

/// When rynasd starts: leftovers of a crash are wiped, key checks moved into their
/// ciphertext, and the folders with a sealed password unlocked (in a thread; the boot unit
/// may have done it already).
pub fn auto_unlock_at_start(s: &Arc<Services>) {
    let s = s.clone();
    std::thread::Builder::new()
        .name("crypt-auto-unlock".into())
        .spawn(move || {
            let paths: Vec<String> = s
                .ops
                .config
                .applied(SHARES_KEY)
                .ok()
                .flatten()
                .and_then(|v| serde_json::from_value::<SharesConfig>(v).ok())
                .map(|c| c.shares.into_iter().filter(|x| x.encrypted).map(|x| x.path).collect())
                .unwrap_or_default();
            s.shares.crypt().at_start(&paths, &|l| tracing::info!("{l}"));
            unlock_sealed(&s);
        })
        .map(|_| ())
        .unwrap_or_else(|e| tracing::warn!("automatic unlock: {e}"));
}

/// `POST /v1/crypt/auto-unlock` (root peers only: `rynasctl crypt auto-unlock` from
/// `rynas-crypt-unlock.service`, ordered before Docker at boot): the automatic unlock, done
/// before the answer, so containers start on unlocked folders.
pub(super) async fn auto_unlock_now(State(st): State<SvcState>) -> Res<Json<Vec<String>>> {
    let s = svc(&st)?;
    let done = tokio::task::spawn_blocking(move || unlock_sealed(&s)).await.map_err(|e| internal(e.to_string()))?;
    Ok(Json(done))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containers_stopped_by_a_lock_are_recorded_until_started() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join(LOCK_STOPPED_FILE);
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(read_stopped(&f).is_empty());
        update_stopped(&f, &names(&["syncthing", "photos"]), true);
        update_stopped(&f, &names(&["photos", "other"]), true);
        assert_eq!(read_stopped(&f), ["syncthing", "photos", "other"]);
        // started again: out of the record; the last one removes the file
        update_stopped(&f, &names(&["photos"]), false);
        assert_eq!(read_stopped(&f), ["syncthing", "other"]);
        update_stopped(&f, &names(&["syncthing", "other"]), false);
        assert!(!f.exists());
        update_stopped(&f, &[], false);
        assert!(!f.exists());
    }

    fn store() -> &'static RecoveryStore {
        Box::leak(Box::default())
    }

    fn key() -> Secret {
        Secret::new("0123abcd-".repeat(7))
    }

    /// S1 of the review: the kept keys live in the run folder (tmpfs), never in the state
    /// folder (a system restore point of `/var/lib` would keep them); an older file there is
    /// moved and wiped.
    #[test]
    fn kept_keys_live_in_the_run_folder_and_an_old_file_is_moved() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (run, state) = (tmp.path().join("run"), tmp.path().join("state"));
        // what 0.9.15 left in the state folder
        let old = store();
        old.open(&state.join(OLD_RECOVERY_FILE), None);
        old.keep("/vol1/secret", "vol1", "secret", "admin", key());
        assert!(state.join(OLD_RECOVERY_FILE).exists());
        let s = store();
        s.open(&run.join(RECOVERY_FILE), Some(&state.join(OLD_RECOVERY_FILE)));
        assert!(!state.join(OLD_RECOVERY_FILE).exists(), "wiped from the snapshotted place");
        let f = run.join(RECOVERY_FILE);
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(f.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(s.reveal("/vol1/secret", "admin", false).unwrap().unwrap().expose(), key().expose());
        // a restart reads it again; the confirmation removes the file
        let again = store();
        again.open(&f, Some(&state.join(OLD_RECOVERY_FILE)));
        assert_eq!(again.pending("admin", |_, _| false).len(), 1);
        assert!(again.forget("/vol1/secret", Some("admin")));
        assert!(!f.exists());
    }

    /// S2: shown once freely, again only after a fresh sign-in; only to its creator.
    #[test]
    fn a_second_reveal_needs_a_fresh_sign_in() {
        let s = store();
        s.keep("/vol1/secret", "vol1", "secret", "admin", key());
        assert!(matches!(s.reveal("/vol1/secret", "other", true), Ok(None)));
        assert!(matches!(s.reveal("/vol1/secret", "admin", false), Ok(Some(_))));
        assert!(s.reveal("/vol1/secret", "admin", false).is_err());
        assert!(matches!(s.reveal("/vol1/secret", "admin", true), Ok(Some(_))));
        // the flag survives a restart
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join(RECOVERY_FILE);
        *s.file.lock().unwrap() = Some(f.clone());
        s.save(&s.keys.lock().unwrap());
        let r = store();
        r.open(&f, None);
        assert!(r.reveal("/vol1/secret", "admin", false).is_err());
    }

    /// S3: one timer at the earliest expiry purges it (memory and file), no loop.
    #[tokio::test]
    async fn the_expiry_timer_purges() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join(RECOVERY_FILE);
        let s = store();
        s.open(&f, None);
        s.keep("/vol1/a", "vol1", "a", "admin", key());
        s.keep("/vol1/b", "vol1", "b", "admin", key());
        let soon = rynas_tasks::now_ms() + 150;
        s.keys.lock().unwrap().get_mut("/vol1/a").unwrap().expires_ms = soon;
        s.arm();
        assert_eq!(s.pending("admin", |_, _| false).len(), 2);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let left: Vec<String> = s.keys.lock().unwrap().keys().cloned().collect();
        assert_eq!(left, ["/vol1/b"]);
        let on_disk = read_kept(&f);
        assert_eq!(on_disk.iter().map(|k| k.path.as_str()).collect::<Vec<_>>(), ["/vol1/b"]);
    }

    /// S4: an unmounted volume keeps its entries; a mounted one whose folder is gone drops it.
    #[test]
    fn entries_go_only_when_the_folder_is_truly_gone() {
        let s = store();
        s.keep("/vol1/a", "vol1", "a", "admin", key());
        s.keep("/vol2/b", "vol2", "b", "admin", key());
        let mounted = ["vol1"];
        // nothing configured anywhere: only the mounted volume's folder is gone
        let gone = |_: &str, v: &str| mounted.contains(&v);
        let v = s.pending("admin", gone);
        assert_eq!(v.iter().map(|k| k.path.as_str()).collect::<Vec<_>>(), ["/vol2/b"]);
        assert_eq!(s.pending("admin", |_, _| false).len(), 1);
    }
}
