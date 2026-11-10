//! The storage write side of the internal API: storage plans executed as tasks, LUKS
//! unlock/lock, subvolumes, per-disk and per-volume settings (see
//! `rynas_api_types::storage` for the routes and `rynas_storage::exec` for the execution).
//!
//! Every destructive request is checked here first, so the interface gets a plain error at
//! once: a valid plan on a fresh inventory, the typed confirmation (volume label and the
//! serials of the disks to erase), the kernel's word that the disks are free
//! (`open(O_EXCL)`), and no other running operation on the same disks or volume. The task
//! checks again with a fresh inventory before the first step. Secrets (passphrase, recovery
//! key) never reach a log: they travel on standard input, and the recovery key of a new
//! volume is kept in memory for one hour and handed out once.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use rynas_api_types::storage::{
    ArrayTuning, BtrfsFilesystem, DiskOverride, DiskSettings, DiskSettingsState, DiskSummary,
    GrowPlanRequest, HealthAckRequest, HealthAckResult, MdArray, RecoveryKey, ReplacePlanRequest, ScrubAction, ScrubPlanRequest,
    StoragePlan, StorageTuning, StorageVolumes, SubvolumeCreateRequest, SubvolumeInfo,
    SubvolumeSettings, UnlockRequest, VolumeCreateRequest, VolumeGrowRequest, VolumeRecord,
    VolumeReplaceRequest, VolumeScrubRequest, VolumeSettings, VolumeSettingsState,
};
use rynas_api_types::{SharesConfig, TaskStarted, error_codes};
use rynas_config::Config;
use rynas_storage::domain::{
    TUNING_KEY, VOLUMES_KEY, effective, parse_tuning, parse_volumes, propose_stripe_cache,
    type_defaults, valid_serial,
};
use rynas_storage::exec::{
    self, LuksVolume, Machine, Output, RunOpts, Secrets, VolumeChange, check_confirmation,
    claim_targets,
};
use rynas_storage::{Inventory, Storage, plan, subvol};
use rynas_tasks::{Cmd, TaskCtx, now_ms};
use serde::Deserialize;

use crate::ops::{Error, Ops, Res, user};

/// How long the recovery key of a new volume waits to be revealed.
const RECOVERY_KEY_TTL_MS: u64 = 3600 * 1000;
/// Recovery keys kept at once (one per volume created in the last hour).
const MAX_RECOVERY_KEYS: usize = 16;
/// Largest LUKS2 header backup served (16 MiB metadata area + keyslots).
const MAX_HEADER: u64 = 64 << 20;
/// Longest wait for a running inventory refresh.
const STORAGE_WAIT: Duration = Duration::from_secs(85);

/// What the storage write endpoints work on.
pub struct StorageOps {
    pub ops: Arc<Ops>,
    pub storage: Arc<Storage>,
    /// Shared with the read side: one inventory refresh at a time.
    pub storage_lock: Arc<tokio::sync::Mutex<()>>,
    /// `--dry-system`: commands are logged, not run.
    pub dry: bool,
    /// label → (expiry ms, recovery key). Capped at [`MAX_RECOVERY_KEYS`], expired entries
    /// dropped on every access.
    recovery: Mutex<HashMap<String, (u64, String)>>,
    /// Disks and volumes an operation is working on (`disk:sdb`, `volume:vol1`,
    /// `scrub:vol1`); bounded by the number of running tasks.
    reserved: Mutex<HashSet<String>>,
    /// label → id of its running scrub task.
    scrubs: Mutex<HashMap<String, String>>,
}

impl StorageOps {
    pub fn new(
        ops: Arc<Ops>,
        storage: Arc<Storage>,
        storage_lock: Arc<tokio::sync::Mutex<()>>,
        dry: bool,
    ) -> Self {
        Self {
            ops,
            storage,
            storage_lock,
            dry,
            recovery: Mutex::default(),
            reserved: Mutex::default(),
            scrubs: Mutex::default(),
        }
    }

    fn config(&self) -> &Arc<Config> {
        &self.ops.config
    }

    /// The inventory: cached (5 s) unless `fresh`, one refresh at a time.
    async fn inventory(&self, fresh: bool) -> Res<Arc<Inventory>> {
        if !fresh && let Some(i) = self.storage.cached_inventory() {
            return Ok(i);
        }
        let _guard = tokio::time::timeout(STORAGE_WAIT, self.storage_lock.lock())
            .await
            .map_err(|_| {
                Error::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    error_codes::TIMEOUT,
                    "the storage inventory is still being read".into(),
                )
            })?;
        if !fresh && let Some(i) = self.storage.cached_inventory() {
            return Ok(i);
        }
        let s = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            if fresh {
                s.invalidate();
            }
            s.refresh_inventory()
        })
        .await
        .map_err(Error::internal)?
        .map_err(Error::from)
    }

    fn reserve(self: &Arc<Self>, keys: Vec<String>) -> Res<Reservation> {
        let mut r = self.reserved.lock().unwrap();
        if let Some(k) = keys.iter().find(|k| r.contains(*k)) {
            return Err(Error::new(
                StatusCode::CONFLICT,
                error_codes::BUSY,
                format!(
                    "another storage operation is working on {}",
                    k.split_once(':').map(|(_, v)| v).unwrap_or(k)
                ),
            ));
        }
        r.extend(keys.iter().cloned());
        Ok(Reservation {
            ops: self.clone(),
            keys,
        })
    }

    fn keep_recovery_key(&self, label: &str, key: String) {
        let now = now_ms();
        let mut r = self.recovery.lock().unwrap();
        r.retain(|_, (exp, _)| *exp > now);
        if r.len() >= MAX_RECOVERY_KEYS
            && let Some(oldest) = r
                .iter()
                .min_by_key(|(_, (e, _))| *e)
                .map(|(k, _)| k.clone())
        {
            r.remove(&oldest);
        }
        r.insert(label.to_string(), (now + RECOVERY_KEY_TTL_MS, key));
    }

    fn take_recovery_key(&self, label: &str) -> Option<String> {
        let now = now_ms();
        let mut r = self.recovery.lock().unwrap();
        r.retain(|_, (exp, _)| *exp > now);
        r.remove(label).map(|(_, k)| k)
    }

    fn volumes_doc(&self) -> Result<StorageVolumes, String> {
        let v = self
            .config()
            .current(VOLUMES_KEY)
            .map_err(|e| e.to_string())?;
        parse_volumes(v.as_ref())
    }
}

/// Released when the task that holds it ends.
pub struct Reservation {
    ops: Arc<StorageOps>,
    keys: Vec<String>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut r = self.ops.reserved.lock().unwrap();
        for k in &self.keys {
            r.remove(k);
        }
    }
}

// ------------------------------------------------------------------------------- machine

/// [`Machine`] of a rynasd task: commands through [`Cmd`] (argv only, own process group,
/// timeout, killed on cancel), the task log and progress, the configuration store.
struct TaskMachine {
    ctx: TaskCtx,
    ops: Arc<StorageOps>,
    user: String,
}

/// Plausible outputs for `--dry-system`, where nothing runs.
fn dry_output(argv: &[String]) -> Output {
    let line = argv.join(" ");
    let stdout = if line.starts_with("mdadm --detail --export") {
        "MD_UUID=00000000:00000000:00000000:00000000\n".to_string()
    } else if line.starts_with("blkid") || line.starts_with("cryptsetup luksUUID") {
        "00000000-0000-0000-0000-000000000000\n".to_string()
    } else if line.starts_with("systemd-cryptenroll") {
        format!("{}\n", ["cccccccc"; 8].join("-"))
    } else {
        String::new()
    };
    Output {
        stdout,
        stderr: String::new(),
    }
}

impl Machine for TaskMachine {
    fn log(&self, line: &str) {
        self.ctx.log(line);
    }

    fn progress(&self, pct: u8) {
        self.ctx.progress(pct);
    }

    fn is_cancelled(&self) -> bool {
        self.ctx.is_cancelled()
    }

    fn run(
        &self,
        argv: Vec<String>,
        opts: RunOpts,
    ) -> impl std::future::Future<Output = Result<Output, String>> + Send {
        let ctx = self.ctx.clone();
        let dry = self.ops.dry;
        async move {
            let mut cmd = Cmd::new(&argv[0])
                .args(&argv[1..])
                .env_clear()
                .timeout(opts.timeout);
            if let Some(s) = opts.stdin {
                cmd = cmd.stdin(s);
            }
            ctx.log(&format!(
                "$ {}{}",
                cmd.display(),
                if opts.quiet {
                    "  (output not logged)"
                } else {
                    ""
                }
            ));
            if dry {
                ctx.log("(dry system: not run)");
                return Ok(dry_output(&argv));
            }
            let token = ctx.token().clone();
            let sink = |l: &str| ctx.log(l);
            let r = cmd
                .run_checked(
                    opts.cancellable.then_some(&token),
                    if opts.quiet { None } else { Some(&sink) },
                )
                .await;
            r.map(|o| Output {
                stdout: o.stdout,
                stderr: o.stderr,
            })
            .map_err(|e| e.to_string())
        }
    }

    fn write_file(&self, path: &str, value: &str) -> Result<(), String> {
        self.ctx.log(&format!("echo {value} > {path}"));
        if self.ops.dry {
            return Ok(());
        }
        std::fs::write(path, value).map_err(|e| format!("{path}: {e}"))
    }

    fn read_file(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn exists(&self, path: &str) -> bool {
        Path::new(path).exists()
    }

    fn remove_file(&self, path: &str) {
        if !self.ops.dry {
            let _ = std::fs::remove_file(path);
        }
    }

    fn claim_free(&self, dev: &str) -> Result<(), String> {
        if self.ops.dry {
            return Ok(());
        }
        rynas_storage::host::claim_free(dev)
    }

    fn sleep(&self, d: Duration) -> impl std::future::Future<Output = ()> + Send {
        let token = self.ctx.token().clone();
        async move {
            tokio::select! {
                _ = tokio::time::sleep(d) => {}
                _ = token.cancelled() => {}
            }
        }
    }

    fn systemd(&self) -> bool {
        Path::new("/run/systemd/system").exists()
    }

    fn commit(
        &self,
        change: VolumeChange,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let ops = self.ops.clone();
        let ctx = self.ctx.clone();
        let user = self.user.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                let mut doc = ops.volumes_doc()?;
                let comment = match &change {
                    VolumeChange::Add(r) => {
                        doc.volumes.retain(|v| v.label != r.label);
                        doc.volumes.push(r.clone());
                        format!("volume {} created", r.label)
                    }
                    VolumeChange::Remove(l) => {
                        doc.volumes.retain(|v| &v.label != l);
                        format!("volume {l} removed (rollback)")
                    }
                };
                let v = serde_json::to_value(doc).map_err(|e| e.to_string())?;
                ops.config()
                    .apply_value(&user, VOLUMES_KEY, Some(v), &comment, &|l| ctx.log(l))
                    .map(|rev| ctx.log(&format!("configuration revision {rev}")))
                    .map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| e.to_string())?
        }
    }
}

// ------------------------------------------------------------------------------- helpers

type StoState = Option<Arc<StorageOps>>;

fn sto(st: &StoState) -> Res<Arc<StorageOps>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "the storage write side is not enabled in this rynasd".into(),
        )
    })
}

fn bad(code: &'static str, msg: impl Into<String>) -> Error {
    Error::new(StatusCode::BAD_REQUEST, code, msg.into())
}

fn conflict(msg: impl Into<String>) -> Error {
    Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, msg.into())
}

fn json_body<T>(b: Result<Json<T>, JsonRejection>) -> Res<T> {
    b.map(|Json(v)| v).map_err(Error::from)
}

fn started(id: String) -> Response {
    (StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response()
}

/// 400 `invalid_value` with the plan's errors; `field` is the request field of the first
/// issue (the UI also gets every issue with a dry run or the plan endpoint).
fn invalid_plan(p: &StoragePlan) -> Error {
    let e = bad(error_codes::INVALID_VALUE, p.errors.join("; "));
    match p.issues.first().and_then(|i| i.field.clone()) {
        Some(f) => e.field(f),
        None => e,
    }
}

fn check_passphrase(p: &str) -> Res<()> {
    if p.chars().count() < exec::MIN_PASSPHRASE || p.len() > 512 || p.chars().any(char::is_control)
    {
        return Err(Error::invalid(
            "passphrase",
            format!(
                "the passphrase needs {}-512 characters, without control characters",
                exec::MIN_PASSPHRASE
            ),
        ));
    }
    Ok(())
}

/// Kernel claims of the disks, at request time (the task checks again).
fn check_free_now(s: &StorageOps, inv: &Inventory, disks: &[String]) -> Res<()> {
    if s.dry {
        return Ok(());
    }
    for d in disks {
        for dev in claim_targets(inv, d) {
            rynas_storage::host::claim_free(&dev).map_err(conflict)?;
        }
    }
    Ok(())
}

fn disks_of<'a>(inv: &'a Inventory, names: &[String]) -> Res<Vec<&'a DiskSummary>> {
    names
        .iter()
        .map(|n| {
            inv.disks
                .iter()
                .find(|d| &d.name == n)
                .ok_or_else(|| Error::not_found(format!("disk {n}")))
        })
        .collect()
}

fn confirm(label: &str, confirm: &str, disks: &[&DiskSummary], serials: &[String]) -> Res<()> {
    check_confirmation(label, confirm, disks, serials)
        .map_err(|e| bad(error_codes::CONFIRMATION_REQUIRED, e))
}

/// A volume by label: its filesystem, the md array below and the LUKS mapping between.
struct VolumeRef {
    label: String,
    md: Option<MdArray>,
    fs: Option<BtrfsFilesystem>,
    /// Open mapping (kernel name of the dm device's mapper name).
    mapper: Option<String>,
    /// The md array holds a LUKS container (open or not).
    luks: bool,
    record: Option<VolumeRecord>,
}

impl VolumeRef {
    fn mountpoint(&self) -> String {
        self.record
            .as_ref()
            .map(|r| r.mountpoint.clone())
            .or_else(|| self.fs.as_ref().and_then(|f| f.mountpoint.clone()))
            .unwrap_or_else(|| format!("/{}", self.label))
    }

    fn mounted(&self) -> Option<&BtrfsFilesystem> {
        self.fs.as_ref().filter(|f| f.mountpoint.is_some())
    }
}

fn resolve(inv: &Inventory, doc: &StorageVolumes, label: &str) -> Res<VolumeRef> {
    if !plan::valid_label(label) {
        return Err(Error::bad_request("invalid volume name"));
    }
    let record = doc.volumes.iter().find(|v| v.label == label).cloned();
    let fs = inv
        .filesystems
        .iter()
        .find(|f| {
            f.label.as_deref() == Some(label)
                || record.as_ref().is_some_and(|r| r.fs_uuid == f.uuid)
        })
        .cloned();
    let md = inv
        .arrays
        .iter()
        .find(|a| {
            a.md_name
                .as_deref()
                .is_some_and(|n| n == label || n.ends_with(&format!(":{label}")))
                || record
                    .as_ref()
                    .is_some_and(|r| a.uuid.as_deref() == Some(r.md_uuid.as_str()))
        })
        .cloned();
    let (mapper, luks) = match &md {
        Some(a) => {
            let above = inv.graph.above(&a.name);
            let mapper = above
                .iter()
                .find(|d| d.kind == "crypt")
                .map(|d| d.name.clone());
            let luks = inv
                .graph
                .get(&a.name)
                .is_some_and(|d| d.fstype.as_deref() == Some("crypto_LUKS"));
            (mapper, luks)
        }
        None => (None, false),
    };
    if fs.is_none() && md.is_none() && record.is_none() {
        return Err(Error::not_found(format!("volume {label}")));
    }
    Ok(VolumeRef {
        label: label.to_string(),
        md,
        fs,
        mapper,
        luks: luks || record.as_ref().is_some_and(|r| r.luks_uuid.is_some()),
        record,
    })
}

async fn volume(s: &StorageOps, label: &str, fresh: bool) -> Res<(Arc<Inventory>, VolumeRef)> {
    let inv = s.inventory(fresh).await?;
    let doc = s.volumes_doc().map_err(Error::internal)?;
    let v = resolve(&inv, &doc, label)?;
    Ok((inv, v))
}

/// Spawns a storage task: the job gets a [`TaskMachine`]; the reservation is released and
/// the inventory forgotten when it ends.
fn spawn<F, Fut>(
    s: &Arc<StorageOps>,
    kind: &str,
    user: &str,
    reservation: Reservation,
    job: F,
) -> Res<String>
where
    F: FnOnce(TaskMachine) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let ops = s.clone();
    let u = user.to_string();
    let id = s.ops.tasks.spawn(kind, user, move |ctx| async move {
        let _held = reservation;
        let storage = ops.storage.clone();
        let m = TaskMachine { ctx, ops, user: u };
        let r = job(m).await;
        storage.invalidate();
        r
    })?;
    Ok(id)
}

// ------------------------------------------------------------------------------- router

pub fn router(st: StoState) -> Router {
    Router::new()
        .route("/v1/storage/volumes", post(volume_create))
        .route("/v1/storage/volumes/{name}/grow", post(volume_grow))
        .route("/v1/storage/volumes/{name}/replace", post(volume_replace))
        .route("/v1/storage/volumes/{name}/scrub", post(volume_scrub))
        .route("/v1/storage/volumes/{name}/unlock", post(volume_unlock))
        .route("/v1/storage/volumes/{name}/lock", post(volume_lock))
        .route(
            "/v1/storage/volumes/{name}/reveal-recovery-key",
            post(reveal_recovery_key),
        )
        .route(
            "/v1/storage/volumes/{name}/luks-header",
            get(header_get).delete(header_delete),
        )
        .route(
            "/v1/storage/volumes/{name}/settings",
            get(volume_settings_get).put(volume_settings_put),
        )
        .route(
            "/v1/storage/volumes/{name}/subvolumes",
            get(subvolumes_list).post(subvolume_create),
        )
        .route(
            "/v1/storage/volumes/{name}/subvolumes/{sub}",
            put(subvolume_put).delete(subvolume_delete),
        )
        .route(
            "/v1/storage/disks/{name}/settings",
            get(disk_settings_get).put(disk_settings_put),
        )
        .route(
            "/v1/storage/disks/{name}/health-ack",
            post(health_ack_post).delete(health_ack_delete),
        )
        .with_state(st)
}

// ------------------------------------------------------------------------------- SMART acks

/// Where the SMART acknowledgements live (`rynas_storage::health_ack`): runtime state of
/// rynasd, not configuration (nothing on the system is rendered from them).
pub fn health_acks_file(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("storage").join("health-acks.json")
}

fn check_disk_name(name: &str) -> Res<()> {
    if name.is_empty()
        || name.len() > 32
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return Err(Error::bad_request("invalid disk name"));
    }
    Ok(())
}

/// The storage lock, as the inventory refresh takes it: an acknowledgement must not race a
/// refresh that would cache the verdict from before it.
async fn storage_guard(s: &StorageOps) -> Res<tokio::sync::MutexGuard<'_, ()>> {
    tokio::time::timeout(STORAGE_WAIT, s.storage_lock.lock())
        .await
        .map_err(|_| {
            Error::new(
                StatusCode::GATEWAY_TIMEOUT,
                error_codes::TIMEOUT,
                "the storage inventory is still being read".into(),
            )
        })
}

/// Largest number of codes in one acknowledgement (there are six acknowledgeable ones).
const MAX_ACK_CODES: usize = 32;

/// `POST /v1/storage/disks/{name}/health-ack`: marks as seen the acknowledgeable SMART
/// reasons the administrator saw ([`HealthAckRequest`]); rynasd reads SMART again and
/// records per code the lower of the seen and the current count. Administrators only; 409
/// when nothing seen can be acknowledged now, or SMART cannot be read now (standby,
/// smartctl failed), or the serial is shared by several disks.
async fn health_ack_post(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<HealthAckRequest>, JsonRejection>,
) -> Res<Json<HealthAckResult>> {
    let (s, who) = (sto(&st)?, user(&h)?);
    s.ops.admins.require_admin(&who, "SMART acknowledgements")?;
    check_disk_name(&name)?;
    let req = json_body(body)?;
    if req.codes.len() > MAX_ACK_CODES {
        return Err(Error::invalid("codes", "too many codes"));
    }
    let storage = s.storage.clone();
    let _guard = storage_guard(&s).await?;
    let r = tokio::task::spawn_blocking(move || storage.acknowledge_health(&name, &who, &req.codes))
        .await
        .map_err(Error::internal)??;
    if r.acknowledged.is_empty() {
        return Err(conflict(format!(
            "{}: no SMART warning that can be marked as seen",
            r.disk
        )));
    }
    tracing::info!(
        disk = %r.disk,
        serial = %r.serial,
        codes = ?r.acknowledged.iter().map(|c| format!("{}={}", c.code, c.n)).collect::<Vec<_>>(),
        grew = ?r.grew,
        "SMART warnings marked as seen"
    );
    Ok(Json(r))
}

/// `DELETE /v1/storage/disks/{name}/health-ack`: the disk's SMART warnings count again.
/// Administrators only; 204 also when there was nothing to forget.
async fn health_ack_delete(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Res<StatusCode> {
    let (s, who) = (sto(&st)?, user(&h)?);
    s.ops.admins.require_admin(&who, "SMART acknowledgements")?;
    check_disk_name(&name)?;
    let storage = s.storage.clone();
    let n = name.clone();
    let _guard = storage_guard(&s).await?;
    let cleared = tokio::task::spawn_blocking(move || storage.clear_health_acks(&n))
        .await
        .map_err(Error::internal)??;
    if cleared {
        tracing::info!(disk = %name, by = %who, "SMART acknowledgements removed");
    }
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------------------- volumes

async fn volume_create(
    State(st): State<StoState>,
    h: HeaderMap,
    body: Result<Json<VolumeCreateRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let mut req = json_body(body)?;
    let inv = s.inventory(true).await?;
    let p = plan::plan_volume(&inv, &req.plan);
    if !p.valid {
        return Err(invalid_plan(&p));
    }
    let label = req.plan.label.clone();
    confirm(
        &label,
        &req.confirm,
        &disks_of(&inv, &req.plan.disks)?,
        &req.serials,
    )?;
    let secrets = if req.plan.encrypt {
        let pass = req.passphrase.take().ok_or_else(|| {
            Error::invalid("passphrase", "an encrypted volume needs a passphrase")
        })?;
        check_passphrase(&pass)?;
        Secrets::new(Some(pass))
    } else {
        Secrets::default()
    };
    check_free_now(&s, &inv, &req.plan.disks)?;
    let mut keys: Vec<String> = req.plan.disks.iter().map(|d| format!("disk:{d}")).collect();
    keys.push(format!("volume:{label}"));
    let held = s.reserve(keys)?;
    if req.dry_run {
        return Ok(Json(p).into_response());
    }
    let plan_req = req.plan.clone();
    let ops = s.clone();
    let id = spawn(
        &s,
        "storage.volume-create",
        &user,
        held,
        move |m| async move {
            // the disks may have changed while the task waited in the queue
            let inv = ops.inventory(true).await.map_err(|e| e.2)?;
            let fresh = plan::plan_volume(&inv, &plan_req);
            if !fresh.valid || fresh.steps != p.steps {
                return Err(format!(
                    "the disks changed since the request: {}; plan again",
                    fresh.errors.join("; ")
                ));
            }
            let out = exec::create_volume(
                &m,
                &inv,
                &fresh,
                plan_req.level,
                &plan_req.disks,
                &secrets,
                now_ms(),
            )
            .await?;
            if let Some(k) = out.recovery_key {
                ops.keep_recovery_key(&plan_req.label, k);
                m.log(&format!(
                "the recovery key can be revealed once within the hour (reveal-recovery-key); the LUKS header backup waits for its download at {}",
                out.header_backup.as_deref().unwrap_or("?")
            ));
            }
            Ok(())
        },
    )?;
    Ok(started(id))
}

async fn volume_grow(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<VolumeGrowRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let mut req = json_body(body)?;
    let (inv, v) = volume(&s, &name, true).await?;
    let md =
        v.md.as_ref()
            .ok_or_else(|| conflict(format!("volume {name} has no md array")))?;
    let preq = GrowPlanRequest {
        array: md.name.clone(),
        disks: req.disks.clone(),
        level: req.level,
    };
    let p = plan::plan_grow(&inv, &preq);
    if !p.valid {
        return Err(invalid_plan(&p));
    }
    confirm(
        &name,
        &req.confirm,
        &disks_of(&inv, &req.disks)?,
        &req.serials,
    )?;
    let needs_secret = p.steps.iter().any(|s| {
        matches!(
            &s.action,
            rynas_api_types::storage::PlanAction::Command { stdin: Some(_), .. }
        )
    });
    let secrets = match (needs_secret, req.passphrase.take()) {
        (true, None) => {
            return Err(Error::invalid(
                "passphrase",
                "the volume is encrypted: the passphrase is needed to grow its LUKS mapping",
            ));
        }
        (_, pass) => Secrets::new(pass),
    };
    check_free_now(&s, &inv, &req.disks)?;
    let mut keys: Vec<String> = req.disks.iter().map(|d| format!("disk:{d}")).collect();
    keys.push(format!("volume:{name}"));
    let held = s.reserve(keys)?;
    if req.dry_run {
        return Ok(Json(p).into_response());
    }
    let ops = s.clone();
    let disks = req.disks.clone();
    let id = spawn(
        &s,
        "storage.volume-grow",
        &user,
        held,
        move |m| async move {
            let inv = ops.inventory(true).await.map_err(|e| e.2)?;
            let fresh = plan::plan_grow(&inv, &preq);
            if !fresh.valid || fresh.steps != p.steps {
                return Err(format!(
                    "the array or the disks changed since the request: {}; plan again",
                    fresh.errors.join("; ")
                ));
            }
            exec::run_plan(&m, &inv, &fresh, &disks, &secrets).await
        },
    )?;
    Ok(started(id))
}

async fn volume_replace(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<VolumeReplaceRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let req = json_body(body)?;
    let (inv, v) = volume(&s, &name, true).await?;
    let md =
        v.md.as_ref()
            .ok_or_else(|| conflict(format!("volume {name} has no md array")))?;
    let preq = ReplacePlanRequest {
        array: md.name.clone(),
        old: req.old.clone(),
        new_disk: req.new_disk.clone(),
    };
    let p = plan::plan_replace(&inv, &preq);
    if !p.valid {
        return Err(invalid_plan(&p));
    }
    let disks = vec![req.new_disk.clone()];
    confirm(&name, &req.confirm, &disks_of(&inv, &disks)?, &req.serials)?;
    check_free_now(&s, &inv, &disks)?;
    let held = s.reserve(vec![
        format!("disk:{}", req.new_disk),
        format!("volume:{name}"),
    ])?;
    if req.dry_run {
        return Ok(Json(p).into_response());
    }
    let ops = s.clone();
    let id = spawn(
        &s,
        "storage.volume-replace",
        &user,
        held,
        move |m| async move {
            let inv = ops.inventory(true).await.map_err(|e| e.2)?;
            let fresh = plan::plan_replace(&inv, &preq);
            if !fresh.valid || fresh.steps != p.steps {
                return Err(format!(
                    "the array or the disks changed since the request: {}; plan again",
                    fresh.errors.join("; ")
                ));
            }
            exec::run_plan(&m, &inv, &fresh, &disks, &Secrets::default()).await
        },
    )?;
    Ok(started(id))
}

async fn volume_scrub(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<VolumeScrubRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let req = json_body(body)?;
    let (inv, v) = volume(&s, &name, false).await?;
    let fs = v
        .mounted()
        .ok_or_else(|| conflict(format!("volume {name} is not mounted")))?;
    let mnt = fs.mountpoint.clone().unwrap_or_default();
    if req.action == ScrubAction::Stop {
        // the scrub task of rynas, if any, and whatever runs in the kernel
        let running = s.scrubs.lock().unwrap().get(&name).cloned();
        if let Some(id) = &running {
            let _ = s.ops.tasks.cancel(id);
        }
        let arrays: Vec<String> = v.md.iter().map(|a| a.name.clone()).collect();
        let held = s.reserve(vec![format!("scrub-stop:{name}")])?;
        let id = spawn(&s, "storage.scrub-stop", &user, held, move |m| async move {
            let mut o = RunOpts::new(Duration::from_secs(60));
            o.cancellable = false;
            match m
                .run(
                    vec!["btrfs".into(), "scrub".into(), "cancel".into(), mnt],
                    o,
                )
                .await
            {
                Ok(_) => {}
                Err(e) if e.contains("exit code 2") => m.log("no btrfs scrub was running"),
                Err(e) => return Err(e),
            }
            for a in arrays {
                let p = format!("/sys/block/{a}/md/sync_action");
                if m.read_file(&p).is_some_and(|x| x.trim() == "check") {
                    m.write_file(&p, "idle")?;
                }
            }
            Ok(())
        })?;
        return Ok(started(id));
    }
    let p = plan::plan_scrub(
        &inv,
        &ScrubPlanRequest {
            filesystem: mnt,
            md_check: req.md_check,
        },
    );
    if !p.valid {
        return Err(invalid_plan(&p));
    }
    let held = s.reserve(vec![format!("scrub:{name}")])?;
    if req.dry_run {
        return Ok(Json(p).into_response());
    }
    let ops = s.clone();
    let label = name.clone();
    let id = spawn(&s, "storage.scrub", &user, held, move |m| async move {
        let inv = ops.inventory(false).await.map_err(|e| e.2)?;
        let r = exec::run_plan(&m, &inv, &p, &[], &Secrets::default()).await;
        ops.scrubs.lock().unwrap().remove(&label);
        r
    })?;
    s.scrubs.lock().unwrap().insert(name, id.clone());
    Ok(started(id))
}

fn luks_volume(v: &VolumeRef) -> Res<LuksVolume> {
    if !v.luks {
        return Err(conflict(format!("volume {} is not encrypted", v.label)));
    }
    if v.record.is_none() {
        return Err(conflict(format!(
            "volume {} was not created by rynas: it has no fstab entry to mount it from",
            v.label
        )));
    }
    let device = format!("/dev/md/{}", v.label);
    let device = match (&v.md, Path::new(&device).exists()) {
        (_, true) => device,
        (Some(a), false) => a.path.clone(),
        (None, false) => {
            return Err(conflict(format!(
                "the array of volume {} is not assembled",
                v.label
            )));
        }
    };
    Ok(LuksVolume {
        label: v.label.clone(),
        device,
        mapper: v
            .mapper
            .clone()
            .unwrap_or_else(|| exec::mapper_name(&v.label)),
        mountpoint: v.mountpoint(),
    })
}

async fn volume_unlock(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<UnlockRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let req = json_body(body)?;
    if req.passphrase.is_empty() || req.passphrase.len() > 512 {
        return Err(Error::invalid("passphrase", "the passphrase is empty"));
    }
    let (_, v) = volume(&s, &name, true).await?;
    let lv = luks_volume(&v)?;
    let held = s.reserve(vec![format!("volume:{name}")])?;
    let secrets = Secrets::new(Some(req.passphrase.clone()));
    drop(req);
    let id = spawn(&s, "storage.unlock", &user, held, move |m| async move {
        exec::unlock(&m, &lv, &secrets).await
    })?;
    Ok(started(id))
}

async fn volume_lock(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let (_, v) = volume(&s, &name, true).await?;
    let lv = luks_volume(&v)?;
    let held = s.reserve(vec![format!("volume:{name}")])?;
    let id = spawn(&s, "storage.lock", &user, held, move |m| async move {
        exec::lock(&m, &lv).await
    })?;
    Ok(started(id))
}

async fn reveal_recovery_key(
    State(st): State<StoState>,
    UrlPath(name): UrlPath<String>,
) -> Res<Json<RecoveryKey>> {
    let s = sto(&st)?;
    s.take_recovery_key(&name)
        .map(|key| Json(RecoveryKey { key }))
        .ok_or_else(|| {
            Error::not_found(format!(
                "no recovery key waiting for volume {name} (shown once, kept one hour)"
            ))
        })
}

fn header_path(name: &str) -> Res<String> {
    if !plan::valid_label(name) {
        return Err(Error::bad_request("invalid volume name"));
    }
    Ok(exec::header_backup_path(name))
}

async fn header_get(State(st): State<StoState>, UrlPath(name): UrlPath<String>) -> Res<Response> {
    sto(&st)?;
    let path = header_path(&name)?;
    let data = tokio::task::spawn_blocking(move || -> std::io::Result<Option<Vec<u8>>> {
        match std::fs::metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
            Ok(m) if m.len() > MAX_HEADER => Err(std::io::Error::other("header backup too large")),
            Ok(_) => std::fs::read(&path).map(Some),
        }
    })
    .await
    .map_err(Error::internal)?
    .map_err(Error::internal)?
    .ok_or_else(|| Error::not_found(format!("no LUKS header backup waiting for volume {name}")))?;
    let mut r = data.into_response();
    let hd = r.headers_mut();
    hd.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) =
        HeaderValue::from_str(&format!("attachment; filename=\"luks-header-{name}.img\""))
    {
        hd.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(r)
}

async fn header_delete(
    State(st): State<StoState>,
    UrlPath(name): UrlPath<String>,
) -> Res<StatusCode> {
    sto(&st)?;
    let path = header_path(&name)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::not_found(format!(
            "no LUKS header backup waiting for volume {name}"
        ))),
        Err(e) => Err(Error::internal(e)),
    }
}

// ------------------------------------------------------------------------------- settings

/// `storage.disks` as `user` sees it: staged value if any, else current.
fn tuning_for(c: &Config, user: &str) -> Result<(StorageTuning, StorageTuning, bool), String> {
    let cur = parse_tuning(c.current(TUNING_KEY).map_err(|e| e.to_string())?.as_ref())?;
    match c
        .pending_value(user, TUNING_KEY)
        .map_err(|e| e.to_string())?
    {
        Some(v) => Ok((parse_tuning(v.as_ref())?, cur, true)),
        None => Ok((cur.clone(), cur, false)),
    }
}

fn mem_total() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines()
                .find_map(|l| l.strip_prefix("MemTotal:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        })
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

fn volume_settings_state(
    c: &Config,
    user: &str,
    v: &VolumeRef,
) -> Result<VolumeSettingsState, String> {
    let md = v.md.as_ref();
    let uuid = md.and_then(|a| a.uuid.clone());
    let (eff, cur, staged) = tuning_for(c, user)?;
    let pick = |t: &StorageTuning| VolumeSettings {
        stripe_cache_size: uuid.as_ref().and_then(|u| {
            t.arrays
                .iter()
                .find(|a| &a.md_uuid == u)
                .map(|a| a.stripe_cache_size)
        }),
    };
    let applied = pick(&cur);
    let pending = (staged && pick(&eff) != applied).then(|| pick(&eff));
    let proposal = md.and_then(|a| propose_stripe_cache(&a.level, a.raid_disks, mem_total()));
    Ok(VolumeSettingsState {
        volume: v.label.clone(),
        applied,
        pending,
        proposed_stripe_cache_size: proposal.map(|p| p.0),
        proposed_memory_bytes: proposal.map(|p| p.1),
        current_stripe_cache_size: md.and_then(|a| {
            std::fs::read_to_string(format!("/sys/block/{}/md/stripe_cache_size", a.name))
                .ok()
                .and_then(|s| s.trim().parse().ok())
        }),
    })
}

async fn volume_settings_get(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Res<Json<VolumeSettingsState>> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let (_, v) = volume(&s, &name, false).await?;
    let c = s.config().clone();
    tokio::task::spawn_blocking(move || volume_settings_state(&c, &user, &v))
        .await
        .map_err(Error::internal)?
        .map(Json)
        .map_err(Error::internal)
}

async fn volume_settings_put(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<VolumeSettings>, JsonRejection>,
) -> Res<Json<VolumeSettingsState>> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let req = json_body(body)?;
    let (_, v) = volume(&s, &name, false).await?;
    let md =
        v.md.clone()
            .ok_or_else(|| conflict(format!("volume {name} has no md array")))?;
    if req.stripe_cache_size.is_some() && !matches!(md.level.as_str(), "raid4" | "raid5" | "raid6")
    {
        return Err(Error::invalid(
            "stripe_cache_size",
            "stripe_cache_size exists for RAID4/5/6 only",
        ));
    }
    let uuid = md
        .uuid
        .clone()
        .ok_or_else(|| Error::internal("the array has no UUID"))?;
    let c = s.config().clone();
    tokio::task::spawn_blocking(move || -> Res<VolumeSettingsState> {
        let (mut doc, _, _) = tuning_for(&c, &user).map_err(Error::internal)?;
        doc.arrays.retain(|a| a.md_uuid != uuid);
        if let Some(n) = req.stripe_cache_size {
            doc.arrays.push(ArrayTuning {
                md_uuid: uuid,
                stripe_cache_size: n,
            });
        }
        let value = serde_json::to_value(doc).map_err(Error::internal)?;
        c.stage(&user, TUNING_KEY, Some(value))?;
        volume_settings_state(&c, &user, &v).map_err(Error::internal)
    })
    .await
    .map_err(Error::internal)?
    .map(Json)
}

fn disk_settings_state(
    c: &Config,
    user: &str,
    d: &DiskSummary,
) -> Result<DiskSettingsState, String> {
    let (eff, cur, staged) = tuning_for(c, user)?;
    let find = |t: &StorageTuning| {
        t.disks
            .iter()
            .find(|o| o.serial == d.serial)
            .map(|o| o.settings.clone())
    };
    let applied = find(&cur);
    let pending = if staged && find(&eff) != applied {
        Some(find(&eff).unwrap_or_default())
    } else {
        None
    };
    let q = format!("/sys/block/{}/queue", d.name);
    Ok(DiskSettingsState {
        disk: d.name.clone(),
        serial: d.serial.clone(),
        kind: d.kind,
        defaults: type_defaults(d.kind),
        effective: effective(d.kind, applied.as_ref()),
        applied,
        pending,
        current_scheduler: std::fs::read_to_string(format!("{q}/scheduler"))
            .ok()
            .and_then(|s| rynas_storage::tuning::active_scheduler(&s)),
        current_read_ahead_kb: std::fs::read_to_string(format!("{q}/read_ahead_kb"))
            .ok()
            .and_then(|s| s.trim().parse().ok()),
    })
}

async fn disk_of(s: &StorageOps, name: &str) -> Res<DiskSummary> {
    check_disk_name(name)?;
    let inv = s.inventory(false).await?;
    inv.disks
        .iter()
        .find(|d| d.name == name)
        .cloned()
        .ok_or_else(|| Error::not_found(format!("disk {name}")))
}

async fn disk_settings_get(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Res<Json<DiskSettingsState>> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let d = disk_of(&s, &name).await?;
    let c = s.config().clone();
    tokio::task::spawn_blocking(move || disk_settings_state(&c, &user, &d))
        .await
        .map_err(Error::internal)?
        .map(Json)
        .map_err(Error::internal)
}

async fn disk_settings_put(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<DiskSettings>, JsonRejection>,
) -> Res<Json<DiskSettingsState>> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let req = json_body(body)?;
    let d = disk_of(&s, &name).await?;
    if !valid_serial(&d.serial) {
        return Err(bad(
            error_codes::INVALID_VALUE,
            format!(
                "{name} has no serial number usable in a udev rule: per-disk settings cannot follow it"
            ),
        ));
    }
    if (req.apm.is_some() || req.standby_s.is_some())
        && d.kind != rynas_api_types::storage::DiskKind::Hdd
    {
        return Err(Error::invalid(
            if req.apm.is_some() {
                "apm"
            } else {
                "standby_s"
            },
            "APM and spin-down apply to hard disks only",
        ));
    }
    let c = s.config().clone();
    tokio::task::spawn_blocking(move || -> Res<DiskSettingsState> {
        let (mut doc, _, _) = tuning_for(&c, &user).map_err(Error::internal)?;
        doc.disks.retain(|o| o.serial != d.serial);
        if !req.is_empty() {
            doc.disks.push(DiskOverride {
                serial: d.serial.clone(),
                settings: req,
            });
        }
        let value = serde_json::to_value(doc).map_err(Error::internal)?;
        c.stage(&user, TUNING_KEY, Some(value))?;
        disk_settings_state(&c, &user, &d).map_err(Error::internal)
    })
    .await
    .map_err(Error::internal)?
    .map(Json)
}

// ------------------------------------------------------------------------------- subvolumes

fn mounted_fs(v: &VolumeRef) -> Res<(&BtrfsFilesystem, String)> {
    let fs = v
        .mounted()
        .ok_or_else(|| conflict(format!("volume {} is not mounted", v.label)))?;
    let mnt = fs.mountpoint.clone().unwrap_or_default();
    Ok((fs, mnt))
}

async fn subvolumes_list(
    State(st): State<StoState>,
    UrlPath(name): UrlPath<String>,
) -> Res<Json<Vec<SubvolumeInfo>>> {
    let s = sto(&st)?;
    let (_, v) = volume(&s, &name, false).await?;
    let (fs, _) = mounted_fs(&v)?;
    let fs = fs.clone();
    let host = s.storage.host().clone();
    tokio::task::spawn_blocking(move || subvol::list(host.as_ref(), &fs))
        .await
        .map(Json)
        .map_err(Error::internal)
}

fn sub_name(sub: &str) -> Res<()> {
    if subvol::valid_name(sub) {
        Ok(())
    } else {
        Err(bad(
            error_codes::INVALID_VALUE,
            "subvolume names have 1-64 letters, digits, '.', '_' or '-', starting with a letter or digit",
        ))
    }
}

async fn subvolume_create(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath(name): UrlPath<String>,
    body: Result<Json<SubvolumeCreateRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let req = json_body(body)?;
    sub_name(&req.name).map_err(|e| e.field("name"))?;
    let (_, v) = volume(&s, &name, true).await?;
    let (fs, mnt) = mounted_fs(&v)?;
    if fs.subvolumes.iter().any(|x| x.path == req.name) {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::ALREADY_EXISTS,
            format!("subvolume {} already exists", req.name),
        ));
    }
    let path = format!("{}/{}", mnt.trim_end_matches('/'), req.name);
    if Path::new(&path).exists() {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::ALREADY_EXISTS,
            format!("{path} already exists"),
        ));
    }
    let held = s.reserve(vec![format!("volume:{name}")])?;
    let cmds = subvol::create_commands(&path, &req.settings);
    let id = spawn(
        &s,
        "storage.subvolume-create",
        &user,
        held,
        move |m| async move { exec::run_commands(&m, cmds).await },
    )?;
    Ok(started(id))
}

/// An existing, non-system subvolume of a mounted volume.
async fn existing_sub(s: &StorageOps, name: &str, sub: &str) -> Res<(VolumeRef, String)> {
    sub_name(sub)?;
    let (_, v) = volume(s, name, true).await?;
    let (fs, mnt) = mounted_fs(&v)?;
    if !fs
        .subvolumes
        .iter()
        .any(|x| x.path == sub && x.parent_id == 5)
    {
        return Err(Error::not_found(format!("subvolume {sub}")));
    }
    let path = format!("{}/{sub}", mnt.trim_end_matches('/'));
    Ok((v, path))
}

async fn subvolume_put(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath((name, sub)): UrlPath<(String, String)>,
    body: Result<Json<SubvolumeSettings>, JsonRejection>,
) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let req = json_body(body)?;
    let (_, path) = existing_sub(&s, &name, &sub).await?;
    let held = s.reserve(vec![format!("volume:{name}")])?;
    let cmds = subvol::settings_commands(&path, &req);
    let id = spawn(
        &s,
        "storage.subvolume-settings",
        &user,
        held,
        move |m| async move { exec::run_commands(&m, cmds).await },
    )?;
    Ok(started(id))
}

#[derive(Deserialize)]
struct ConfirmQuery {
    confirm: Option<String>,
}

async fn subvolume_delete(
    State(st): State<StoState>,
    h: HeaderMap,
    UrlPath((name, sub)): UrlPath<(String, String)>,
    Query(q): Query<ConfirmQuery>,
) -> Res<Response> {
    let (s, user) = (sto(&st)?, user(&h)?);
    let (_, path) = existing_sub(&s, &name, &sub).await?;
    // a share on it (applied, or staged by anyone) keeps it
    let c = s.config().clone();
    let p = path.clone();
    let used_by: Option<String> = tokio::task::spawn_blocking(move || {
        let shares: SharesConfig = c
            .current(rynas_services::SHARES_KEY)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        shares
            .shares
            .iter()
            .find(|sh| sh.path == p || sh.path.starts_with(&format!("{p}/")))
            .map(|sh| sh.name.clone())
    })
    .await
    .map_err(Error::internal)?;
    if let Some(share) = used_by {
        return Err(conflict(format!(
            "share {share} uses {path}: delete the share first"
        )));
    }
    let empty = std::fs::read_dir(&path)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false);
    if !empty && q.confirm.as_deref() != Some(sub.as_str()) {
        return Err(bad(
            error_codes::CONFIRMATION_REQUIRED,
            format!("{path} is not empty: add confirm={sub} to delete it with its content"),
        ));
    }
    let held = s.reserve(vec![format!("volume:{name}")])?;
    let cmds = subvol::delete_commands(&path);
    let id = spawn(
        &s,
        "storage.subvolume-delete",
        &user,
        held,
        move |m| async move { exec::run_commands(&m, cmds).await },
    )?;
    Ok(started(id))
}

// ------------------------------------------------------------------------------- shares hook

/// Creates the subvolumes that shares ask for, during the apply of `shares`.
pub struct HostSubvolumes {
    pub dry: bool,
    /// Power states of the disks ([`rynas_services::SubvolumeCreator::asleep`]).
    pub storage: Option<Arc<Storage>>,
}

/// rynas volumes in a mountinfo text: btrfs top levels mounted at `/<label>`.
pub fn volumes_in(mountinfo: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = rynas_storage::mounts::parse_mountinfo(mountinfo)
        .into_iter()
        .filter(|m| m.fstype == "btrfs" && m.root == "/")
        .filter_map(|m| {
            let label = m.mountpoint.strip_prefix('/')?.to_string();
            plan::valid_label(&label).then_some((label, m.mountpoint))
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

impl rynas_services::SubvolumeCreator for HostSubvolumes {
    /// The places: the rynas volumes and the system volume (D-ROOTVOL).
    fn volumes(&self) -> Vec<(String, String)> {
        crate::places::host_places()
    }

    fn asleep(&self, mount: &str) -> bool {
        self.storage.as_ref().is_some_and(|s| s.mount_asleep(mount))
    }

    fn volume_path(&self, volume: &str) -> Result<String, String> {
        // the system volume: its folder, made by `create` (D-ROOTVOL)
        if volume == rynas_api_types::storage::SYSTEM_VOLUME {
            return if crate::places::root_is_disk(&crate::places::mountinfo()) {
                Ok(rynas_api_types::storage::SYSTEM_VOLUME_DIR.into())
            } else {
                Err("the root filesystem is not a disk: it cannot hold the system volume".into())
            };
        }
        if !plan::valid_label(volume) {
            return Err(format!("invalid volume name {volume:?}"));
        }
        let want = format!("/{volume}");
        let mounts = rynas_storage::mounts::parse_mountinfo(
            &std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default(),
        );
        if mounts
            .iter()
            .any(|m| m.mountpoint == want && m.fstype == "btrfs")
        {
            Ok(want)
        } else {
            Err(format!("volume {volume} is not mounted"))
        }
    }

    fn create(&self, path: &str, compression: bool, nocow: bool, log: &dyn Fn(&str)) -> Result<(), String> {
        // in the system volume: its folder first; a plain folder when the root is no btrfs
        if crate::places::in_system_dir(path) && !self.dry {
            let dir = crate::places::ensure_system_dir(Path::new("/"), true, log)?;
            let btrfs = rynas_btrfs::ioctl::open_dir(&dir)
                .is_ok_and(|f| rynas_btrfs::ioctl::is_btrfs(&f));
            if !btrfs {
                use std::os::unix::fs::DirBuilderExt;
                log(&format!("mkdir {path}"));
                return std::fs::DirBuilder::new()
                    .mode(0o755)
                    .create(path)
                    .map_err(|e| format!("mkdir {path}: {e}"));
            }
        }
        let settings = SubvolumeSettings {
            compression,
            readonly: false,
            nocow,
        };
        for argv in subvol::create_commands(path, &settings) {
            let cmd = Cmd::new(&argv[0])
                .args(&argv[1..])
                .env_clear()
                .timeout(Duration::from_secs(60));
            log(&format!("$ {}", cmd.display()));
            if self.dry {
                log("(dry system: not run)");
                continue;
            }
            cmd.run_blocking()
                .map_err(|e| format!("{}: {e}", argv[0]))?;
        }
        Ok(())
    }
}
