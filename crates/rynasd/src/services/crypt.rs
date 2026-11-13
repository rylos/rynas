//! Encrypted shared folders (D-CRYPT E1, `docs/phase2/encrypted-folders.md`).
//!
//! - `GET /v1/crypt`, `POST /v1/crypt/install` (task `crypt.install`, administrators);
//! - `POST /v1/crypt/folders` (task `crypt.create`: ciphertext, init, mount),
//!   `POST /v1/crypt/folders/{volume}/{name}/reveal-recovery-key` (once, within the hour, to
//!   the administrator who created it) and `.../recovery-key` (again, with the password);
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

/// How long the recovery key of a new folder waits to be revealed.
const RECOVERY_TTL_MS: u64 = 3600 * 1000;
/// Recovery keys kept at once.
const MAX_RECOVERY_KEYS: usize = 16;
/// Pause before answering a wrong password.
const WRONG_PASSWORD_PAUSE: Duration = Duration::from_secs(1);

/// path → (expiry ms, creating user, recovery key): in memory only, handed out once, and only
/// to the administrator who created the folder.
type Kept = (u64, String, Secret);
static RECOVERY: Mutex<Option<HashMap<String, Kept>>> = Mutex::new(None);

/// Where failed automatic unlocks are reported (set once the engine runs).
static NOTIFIER: OnceLock<Arc<dyn Notifier>> = OnceLock::new();

/// The notification engine, for the automatic unlock's failures.
pub fn set_notifier(n: Arc<dyn Notifier>) {
    let _ = NOTIFIER.set(n);
}

fn keep_recovery_key(path: &str, who: &str, key: Secret) {
    let now = rynas_tasks::now_ms();
    let mut g = RECOVERY.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    m.retain(|_, (exp, _, _)| *exp > now);
    if m.len() >= MAX_RECOVERY_KEYS
        && let Some(oldest) = m.iter().min_by_key(|(_, (e, _, _))| *e).map(|(k, _)| k.clone())
    {
        m.remove(&oldest);
    }
    m.insert(path.to_string(), (now + RECOVERY_TTL_MS, who.to_string(), key));
}

/// The kept key of `path`, once, for the user who created the folder.
fn take_recovery_key(path: &str, who: &str) -> Option<Secret> {
    let now = rynas_tasks::now_ms();
    let mut g = RECOVERY.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.as_mut()?;
    m.retain(|_, (exp, _, _)| *exp > now);
    if m.get(path).is_some_and(|(_, owner, _)| owner != who) {
        return None;
    }
    m.remove(path).map(|(_, _, k)| k)
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
    let id = s.ops.tasks.spawn("crypt.create", &who, move |ctx| async move {
        let c = ctx.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let log = |l: &str| c.log(l);
            let crypt = svc.shares.crypt();
            let made = crypt.create(&path, &pw, &log).map_err(|e| e.to_string())?;
            keep_recovery_key(&path, &owner, made.key);
            log("the recovery key can be revealed once within the hour (reveal-recovery-key), or again later with the password");
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
    let r = CryptCreateRequest { volume, name, ..Default::default() };
    let path = blocking(&s, move |s| new_folder_path(s, &r)).await?;
    let key = take_recovery_key(&path, &who).ok_or_else(|| {
        not_found(format!(
            "recovery key of {path} (shown once to the administrator who created the folder, kept one hour; it can be shown again with the password)"
        ))
    })?;
    audit(&who, "recovery key revealed", &path);
    key_answer(key)
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
            let users = super::containers_using(&sh.path, true);
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
        // running containers that mount it were started while it was locked: they keep the
        // empty folder until they are restarted
        let containers = super::containers_using(&sh.path, true);
        let share = share_view(s, &w, &sh.name)?;
        Ok(Ok(CryptUnlockResponse { share, containers }))
    })
    .await?;
    match r {
        Ok(v) => Ok(Json(v)),
        Err(e) => Err(answer(e).await),
    }
}

pub(super) async fn lock(State(st): State<SvcState>, h: HeaderMap, Path(name): Path<String>) -> Res<Json<ShareView>> {
    let (s, who) = (svc(&st)?, user(&h)?);
    s.ops.admins.require_admin(&who, "encrypted folders")?;
    let v = blocking(&s, move |s| {
        let sh = applied_share(s, &who, &name)?;
        // a running container holds its own (rprivate) copy of the mount: the unmount would
        // succeed here while the plaintext stays readable in the container
        let users = super::containers_using(&sh.path, true);
        if !users.is_empty() {
            return Err(crypt_error(CryptError::Busy(format!(
                "running containers use it: {}; stop them first",
                users.join(", ")
            ))));
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
        audit(&who, "lock", &sh.path);
        share_view(s, &who, &sh.name)
    })
    .await?;
    Ok(Json(v))
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
