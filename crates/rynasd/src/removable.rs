//! Removable drives (`/v1/removable/...`, public under `/api/v1/removable/...`): the watcher,
//! the actions as tasks, the `removable` configuration key, the SMB shares of mounted
//! volumes and the notifications.
//!
//! - **Watcher**: a dedicated thread owns the [`Scanner`] (sysfs, udev database, mount table
//!   below `--hw-root`); every [`TICK`] the async loop asks it for a reading (a read that hangs
//!   on a dead device only blocks that thread). When the drives change, the new
//!   [`RemovableState`] goes to the task stream's `removable` events (`?removable=1`, which
//!   rynas-web forwards on `/api/v1/events` as `removable.state`).
//! - **Actions** are immediate (not staged) and run as tasks `removable.<verb>`; the request
//!   is checked against a fresh reading first, so "not found", "locked", "busy" (with the
//!   processes that hold the volume, `GET …/holders`) come back at once as 404/409.
//! - **Notifications**: a drive unplugged while mounted (`removable.unplugged`, warning;
//!   not after a safe removal), a drive that can be removed safely
//!   (`removable.safe_to_remove`, info, setting `notify_safe_removal`), an NTFS volume that
//!   Windows left dirty and that was mounted read-only (`removable.ntfs_read_only`). Failed
//!   actions are failed tasks (`task.failed`). Plugging in is not notified.
//! - **Auto-mount** (setting, on by default): volumes of a drive that arrives, and at boot
//!   (first reading within [`BOOT_WINDOW_S`] of the system start) those already plugged in.
//!   A volume unmounted by hand stays unmounted until the drive is plugged in again.
//! - **SMB** (setting, off by default): every mounted volume as a private share
//!   (`rynas_removable::smb`), removed and disconnected before its volume is unmounted.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
use rynas_api_types::removable::{
    MountRequest, RemovableAction, RemovableDrive, RemovableMount, RemovableSettings,
    RemovableSettingsState, RemovableState, RemoveMethod, UnlockVolumeRequest, VolumeHolder,
};
use rynas_api_types::{TaskStarted, error_codes};
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile};
use rynas_notify::Notifier;
use rynas_notify::event::{code, field, oneshot};
use rynas_removable::ops::{Ctx, find_drive, find_volume, valid_name};
use rynas_removable::{Removable, RemovableError, Scanner, Tree, smb};
use serde_json::Value;
use tokio::sync::{broadcast, oneshot as once, watch};

use crate::ops::{Error, Ops, Res, user};
use crate::services::{docs, stage};

pub const REMOVABLE_KEY: &str = "removable";
/// Period of the watcher.
pub const TICK: Duration = Duration::from_secs(2);
/// Longest wait for a reading of the scanner thread.
const SCAN_WAIT: Duration = Duration::from_secs(10);
/// Drives already plugged in are mounted automatically when rynasd starts this soon after
/// the system (a boot), not after a restart of rynasd alone.
pub const BOOT_WINDOW_S: f64 = 300.0;
/// User of the tasks rynasd starts by itself (auto-mount).
pub const SYSTEM_USER: &str = "rynas";

// ---------------------------------------------------------------------------------------------
// the configuration domain
// ---------------------------------------------------------------------------------------------

/// `removable`: settings only (no file); an apply hands the value to the watcher.
pub struct RemovableDomain {
    pub tx: watch::Sender<RemovableSettings>,
}

impl RemovableDomain {
    pub fn new() -> (Self, watch::Receiver<RemovableSettings>) {
        let (tx, rx) = watch::channel(RemovableSettings::default());
        (Self { tx }, rx)
    }
}

fn settings_of(v: &Value) -> Result<RemovableSettings, Invalid> {
    serde_json::from_value(v.clone())
        .map_err(|e| Invalid::from(format!("invalid removable document: {e}")))
}

/// Checks a settings document.
pub fn normalize(s: RemovableSettings) -> Result<RemovableSettings, Invalid> {
    if !rynas_api_types::validate::account_name(&s.share.group) {
        return Err(Invalid::at("share.group", "not a valid group name"));
    }
    Ok(s)
}

impl Applier for RemovableDomain {
    fn key(&self) -> &str {
        REMOVABLE_KEY
    }

    fn order(&self) -> i32 {
        // after users (the access group) and shares
        25
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let s = normalize(settings_of(&value)?)?;
        serde_json::to_value(s).map_err(|e| e.to_string().into())
    }

    fn render(&self, _value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        Ok(Vec::new())
    }

    fn reload(&self, value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        let s = match value {
            Some(v) => settings_of(v).map_err(|e| e.message)?,
            None => RemovableSettings::default(),
        };
        (ctx.log)("removable drives: settings handed to the watcher");
        self.tx.send_replace(s);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// the service
// ---------------------------------------------------------------------------------------------

/// A request to the scanner thread.
struct ScanReq {
    force: bool,
    reply: once::Sender<Vec<RemovableDrive>>,
}

#[derive(Default)]
struct Inner {
    /// The last reading.
    drives: Vec<RemovableDrive>,
    /// Actions in progress: device name → action.
    pending: HashMap<String, RemovableAction>,
    /// Drives being removed safely (their disappearance is not an unsafe unplug).
    released: HashSet<String>,
    /// Drives of the last reading: name → (title, serial, mount points).
    known: HashMap<String, (String, String, Vec<String>)>,
    /// The last state sent (JSON), to send only changes.
    last_json: Option<Arc<str>>,
    /// SMB shares offered: mount point → share name.
    shares: HashMap<String, String>,
    /// The include file as last written.
    smb_written: Option<String>,
    first: bool,
}

/// What the removable endpoints work on.
pub struct RemovableSvc {
    pub ops: Arc<Ops>,
    pub svc: Arc<Removable>,
    pub notifier: Arc<dyn Notifier>,
    settings: watch::Receiver<RemovableSettings>,
    inner: Mutex<Inner>,
    tx: broadcast::Sender<Arc<str>>,
    scan: std::sync::mpsc::SyncSender<ScanReq>,
    /// Samba is installed: the SMB include is kept up to date.
    pub smb: bool,
}

pub type RmState = Option<Arc<RemovableSvc>>;

fn unavailable() -> Error {
    Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        error_codes::BACKEND_UNAVAILABLE,
        "removable drives are not enabled in this rynasd".into(),
    )
}

fn rsvc(st: &RmState) -> Res<Arc<RemovableSvc>> {
    st.clone().ok_or_else(unavailable)
}

impl From<RemovableError> for Error {
    fn from(e: RemovableError) -> Self {
        let msg = e.to_string();
        match e {
            RemovableError::NotFound(_) => Error::not_found(msg),
            RemovableError::Busy(..) => Error::new(StatusCode::CONFLICT, error_codes::BUSY, msg),
            RemovableError::Conflict(_) | RemovableError::Unsupported(_) => {
                Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, msg)
            }
            RemovableError::WrongPassphrase => Error::invalid("passphrase", msg),
            RemovableError::Failed(_) => Error::internal(msg),
        }
    }
}

fn now_ms() -> u64 {
    rynas_tasks::now_ms()
}

fn uptime_s(tree: &Tree) -> f64 {
    tree.read("/proc/uptime")
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(f64::MAX)
}

/// The numeric gid of `group` from `<root>/etc/group`, if it exists.
pub fn lookup_gid(root: &std::path::Path, group: &str) -> Option<u32> {
    std::fs::read_to_string(root.join("etc/group")).unwrap_or_default().lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.first() == Some(&group)).then(|| f.get(2)?.parse().ok())?
    })
}

/// The numeric gid of `group` from `<root>/etc/group`; 0 (root) when it does not exist.
pub fn group_gid(root: &std::path::Path, group: &str) -> u32 {
    lookup_gid(root, group)
        .unwrap_or_else(|| {
            tracing::warn!("removable drives: group {group} not found, files owned by root only");
            0
        })
}

impl RemovableSvc {
    /// Starts the scanner thread on `hw_root` and returns the service (the watcher loop is
    /// started by [`spawn_watch`]).
    pub fn new(
        ops: Arc<Ops>,
        svc: Arc<Removable>,
        notifier: Arc<dyn Notifier>,
        settings: watch::Receiver<RemovableSettings>,
        smb: bool,
    ) -> Arc<Self> {
        let (scan_tx, scan_rx) = std::sync::mpsc::sync_channel::<ScanReq>(4);
        let root = svc.tree().root().to_path_buf();
        std::thread::Builder::new()
            .name("removable-scan".into())
            .stack_size(256 << 10)
            .spawn(move || {
                let mut scanner = Scanner::new(Tree::new(root));
                while let Ok(r) = scan_rx.recv() {
                    let drives = scanner.tick(r.force);
                    let _ = r.reply.send(drives);
                }
            })
            .expect("removable scanner thread");
        let (tx, _) = broadcast::channel(16);
        Arc::new(Self {
            ops,
            svc,
            notifier,
            settings,
            inner: Mutex::new(Inner {
                first: true,
                ..Default::default()
            }),
            tx,
            scan: scan_tx,
            smb,
        })
    }

    pub fn settings(&self) -> RemovableSettings {
        self.settings.borrow().clone()
    }

    /// The state changes as JSON (`removable` events of the task stream).
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.tx.subscribe()
    }

    /// The last state sent, as JSON.
    pub fn latest_json(&self) -> Option<Arc<str>> {
        self.inner.lock().unwrap().last_json.clone()
    }

    /// A reading of the scanner thread.
    async fn read(&self, force: bool) -> Res<Vec<RemovableDrive>> {
        let (reply, rx) = once::channel();
        self.scan.try_send(ScanReq { force, reply }).map_err(|_| {
            Error::new(
                StatusCode::CONFLICT,
                error_codes::BUSY,
                "the drive scanner is busy".into(),
            )
        })?;
        match tokio::time::timeout(SCAN_WAIT, rx).await {
            Ok(Ok(d)) => Ok(d),
            _ => Err(Error::new(
                StatusCode::GATEWAY_TIMEOUT,
                error_codes::TIMEOUT,
                "reading the removable drives took too long (a drive does not answer)".into(),
            )),
        }
    }

    /// A fresh reading, processed (notifications, shares, state event); the drives.
    pub async fn refresh(self: &Arc<Self>, force: bool) -> Res<Vec<RemovableDrive>> {
        let drives = self.read(force).await?;
        // the SMB include and smbcontrol are blocking work
        let (s, d) = (self.clone(), drives.clone());
        tokio::task::spawn_blocking(move || s.process(d))
            .await
            .map_err(Error::internal)?;
        Ok(drives)
    }

    fn state_of(&self, inner: &Inner) -> RemovableState {
        let mut drives = inner.drives.clone();
        for d in &mut drives {
            d.pending = inner.pending.get(&d.name).copied();
            for v in &mut d.volumes {
                v.pending = inner
                    .pending
                    .get(&v.name)
                    .or_else(|| v.clear.as_ref().and_then(|c| inner.pending.get(&c.name)))
                    .copied();
                v.share = v
                    .mount_point
                    .as_ref()
                    .and_then(|m| inner.shares.get(m).cloned());
            }
        }
        RemovableState {
            drives,
            mount_root: rynas_removable::mount::MOUNT_ROOT.into(),
            read_at_ms: now_ms(),
        }
    }

    /// The current state (with pending actions and shares).
    pub fn state(&self) -> RemovableState {
        self.state_of(&self.inner.lock().unwrap())
    }

    fn publish(&self, inner: &mut Inner) {
        let mut st = self.state_of(inner);
        st.read_at_ms = 0;
        let json: Arc<str> = serde_json::to_string(&st).unwrap_or_default().into();
        if inner.last_json.as_deref() != Some(&*json) {
            inner.last_json = Some(json.clone());
            let _ = self.tx.send(json);
        }
    }

    /// Takes a reading: unsafe unplugs, auto-mount of arrivals, SMB shares, state event.
    fn process(self: &Arc<Self>, drives: Vec<RemovableDrive>) {
        let settings = self.settings();
        let mut arrivals = Vec::new();
        let job = {
            let mut inner = self.inner.lock().unwrap();
            let first = std::mem::replace(&mut inner.first, false);
            let present: HashSet<String> = drives.iter().map(|d| d.name.clone()).collect();
            for d in &drives {
                let mounts: Vec<String> = d
                    .volumes
                    .iter()
                    .filter_map(|v| v.mount_point.clone())
                    .collect();
                let new = !inner.known.contains_key(&d.name);
                if new && (!first || uptime_s(self.svc.tree()) < BOOT_WINDOW_S) {
                    arrivals.push(d.name.clone());
                }
                if mounts.is_empty() {
                    // seen released: a later mount counts again
                    inner.released.remove(&d.name);
                }
                inner
                    .known
                    .insert(d.name.clone(), (d.title.clone(), d.serial.clone(), mounts));
            }
            let gone: Vec<String> = inner
                .known
                .keys()
                .filter(|k| !present.contains(*k))
                .cloned()
                .collect();
            for name in gone {
                let (title, serial, mounts) = inner.known.remove(&name).unwrap_or_default();
                let released = inner.released.remove(&name);
                inner.pending.remove(&name);
                if !mounts.is_empty() && !released {
                    self.notifier
                        .emit(unplugged_event(&name, &title, &serial, &mounts));
                }
            }
            inner.drives = drives;
            let job = self.update_shares(&mut inner, &settings);
            self.publish(&mut inner);
            job
        };
        self.write_include(job);
        if settings.auto_mount {
            for name in arrivals {
                self.auto_mount(&name);
            }
        }
    }

    /// Volumes of `drive` that auto-mount takes: plain, supported, not mounted, no action.
    fn auto_mount(self: &Arc<Self>, drive: &str) {
        let vols: Vec<String> = {
            let inner = self.inner.lock().unwrap();
            find_drive(&inner.drives, drive)
                .map(|d| {
                    d.volumes
                        .iter()
                        .filter(|v| {
                            !v.luks
                                && v.supported
                                && v.mount_point.is_none()
                                && !inner.pending.contains_key(&v.name)
                        })
                        .map(|v| v.name.clone())
                        .collect()
                })
                .unwrap_or_default()
        };
        for v in vols {
            if let Err(e) = start_mount(self, SYSTEM_USER, &v, false) {
                tracing::warn!("auto-mount of {v}: {}", e.2);
            }
        }
    }

    /// Keeps the SMB include in step with the mounted volumes and the setting.
    /// Computes the SMB include for the mounted volumes; the file to write (and whether smbd
    /// must read it again) when it changed. Written by [`Self::write_include`] outside the lock.
    fn update_shares(&self, inner: &mut Inner, s: &RemovableSettings) -> Option<(String, bool)> {
        if !self.smb {
            return None;
        }
        let mut specs = Vec::new();
        let mut shares = HashMap::new();
        if s.share.enabled {
            let mut used = HashSet::new();
            for d in &inner.drives {
                for v in &d.volumes {
                    let Some(mp) = &v.mount_point else { continue };
                    let mut name = smb::share_name(mp);
                    let base = name.clone();
                    let mut i = 2;
                    while !used.insert(name.to_lowercase()) {
                        name = format!("{base}-{i}");
                        i += 1;
                    }
                    shares.insert(mp.clone(), name.clone());
                    specs.push(smb::ShareSpec {
                        name,
                        path: mp.clone(),
                        comment: d.title.clone(),
                        fs_type: v.mounted_fs().to_string(),
                    });
                }
            }
        }
        let text = smb::render(&specs, &s.share.group, s.share.access);
        inner.shares = shares;
        if inner.smb_written.as_deref() == Some(text.as_str()) {
            return None;
        }
        // the first write only creates the file: nothing to tell smbd yet
        let reload = inner.smb_written.is_some();
        inner.smb_written = Some(text.clone());
        Some((text, reload))
    }

    fn write_include(&self, job: Option<(String, bool)>) {
        let Some((text, reload)) = job else { return };
        match self.svc.host().write_file(smb::INCLUDE, &text) {
            Ok(()) if reload => self.smb_reload(),
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("removable drives: {e}");
                self.inner.lock().unwrap().smb_written = None;
            }
        }
    }

    fn smb_reload(&self) {
        self.smbcontrol(&["smbd", "reload-config"]);
    }

    fn smbcontrol(&self, args: &[&str]) {
        if !self.svc.host().has_program("smbcontrol") {
            return;
        }
        let mut argv = vec!["smbcontrol".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        match self.svc.host().run(&argv, None, Duration::from_secs(15)) {
            Ok(o) if !o.ok() => tracing::debug!("{}: {}", argv.join(" "), o.reason()),
            Err(e) => tracing::debug!("{e}"),
            _ => {}
        }
    }

    /// Before a volume is unmounted: its share goes away and its clients are disconnected.
    fn drop_share(&self, mount_point: &str) {
        let (name, job) = {
            let mut inner = self.inner.lock().unwrap();
            let Some(name) = inner.shares.get(mount_point).cloned() else {
                return;
            };
            inner.drives.iter_mut().for_each(|d| {
                d.volumes.iter_mut().for_each(|v| {
                    if v.mount_point.as_deref() == Some(mount_point) {
                        v.mount_point = None;
                    }
                })
            });
            let s = self.settings();
            let job = self.update_shares(&mut inner, &s);
            (name, job)
        };
        self.write_include(job);
        self.smbcontrol(&["smbd", "close-share", &name]);
    }

    fn set_pending(&self, dev: &str, a: Option<RemovableAction>) {
        let mut inner = self.inner.lock().unwrap();
        match a {
            Some(a) => inner.pending.insert(dev.to_string(), a),
            None => inner.pending.remove(dev),
        };
        self.publish(&mut inner);
    }

    fn busy_with(&self, dev: &str) -> bool {
        self.inner.lock().unwrap().pending.contains_key(dev)
    }

    fn ctx_parts(&self) -> (u32, bool) {
        let s = self.settings();
        (
            group_gid(self.svc.tree().root(), &s.share.group),
            s.read_only,
        )
    }

    /// Holders of a mounted volume.
    pub fn holders(&self, mount_point: &str) -> Vec<VolumeHolder> {
        self.svc.holders(mount_point)
    }
}

fn unplugged_event(
    name: &str,
    title: &str,
    serial: &str,
    mounts: &[String],
) -> rynas_api_types::notify::NewEvent {
    let id = if serial.is_empty() { name } else { serial };
    let mut ev = oneshot(
        NotifyCategory::Storage,
        NotifySeverity::Warning,
        "removable",
        format!("removable:unplugged:{id}"),
        format!("{title} was unplugged while mounted"),
    );
    ev.body = format!(
        "Files being written to {} may be lost or damaged. Remove drives safely before unplugging them.",
        mounts.join(", ")
    );
    let ev = field(code(ev, "removable.unplugged"), "drive", title);
    let ev = field(ev, "device", name);
    field(ev, "mount_points", mounts.join(", "))
}

fn safe_event(title: &str, name: &str, how: RemoveMethod) -> rynas_api_types::notify::NewEvent {
    let ev = oneshot(
        NotifyCategory::Storage,
        NotifySeverity::Info,
        "removable",
        format!("removable:safe:{name}"),
        format!("{title} can be removed safely"),
    );
    let method = match how {
        RemoveMethod::PowerOff => "power_off",
        RemoveMethod::Eject => "eject",
        RemoveMethod::Release => "release",
    };
    let ev = field(code(ev, "removable.safe_to_remove"), "drive", title);
    let ev = field(ev, "device", name);
    field(ev, "method", method)
}

fn ntfs_event(
    title: &str,
    volume: &str,
    label: &str,
    mount_point: &str,
) -> rynas_api_types::notify::NewEvent {
    let shown = if label.is_empty() { volume } else { label };
    let mut ev = oneshot(
        NotifyCategory::Storage,
        NotifySeverity::Warning,
        "removable",
        format!("removable:ntfs:{volume}"),
        format!("{shown} mounted read-only"),
    );
    ev.body = "Windows did not close this NTFS volume cleanly (Fast Startup, hibernation or an unsafe unplug), so it was mounted read-only. Shut Windows down completely or run chkdsk, then plug it in again to write to it.".into();
    let ev = field(code(ev, "removable.ntfs_read_only"), "volume", shown);
    let ev = field(ev, "drive", title);
    field(ev, "mount_point", mount_point)
}

/// The watcher loop: a reading every [`TICK`], settings changes at once.
pub fn spawn_watch(s: Arc<RemovableSvc>) {
    tokio::spawn(async move {
        let mut settings = s.settings.clone();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = s.refresh(false).await {
                        tracing::debug!("removable drives: {}", e.2);
                    }
                }
                r = settings.changed() => {
                    if r.is_err() {
                        return;
                    }
                    let _ = s.refresh(true).await;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------------------------
// actions
// ---------------------------------------------------------------------------------------------

fn accepted(id: String) -> Response {
    (StatusCode::ACCEPTED, Json(TaskStarted { task_id: id })).into_response()
}

fn check_name(name: &str) -> Res<()> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(Error::bad_request("invalid device name"))
    }
}

fn already(dev: &str) -> Error {
    Error::new(
        StatusCode::CONFLICT,
        error_codes::BUSY,
        format!("an action on {dev} is in progress"),
    )
}

/// Runs `f` (blocking) with a task context for the action; the drives are read again first.
async fn run_action<T: Send + 'static>(
    s: Arc<RemovableSvc>,
    ctx_log: rynas_tasks::TaskCtx,
    f: impl FnOnce(&RemovableSvc, &[RemovableDrive], &Ctx) -> Result<T, RemovableError> + Send + 'static,
) -> Result<T, String> {
    let drives = s.refresh(true).await.map_err(|e| e.2)?;
    let s2 = s.clone();
    let log = ctx_log.clone();
    let r = tokio::task::spawn_blocking(move || {
        let (gid, read_only) = s2.ctx_parts();
        let logf = |l: &str| log.log(l);
        let hook = |mp: &str| s2.drop_share(mp);
        let ctx = Ctx {
            log: &logf,
            gid,
            read_only,
            before_unmount: &hook,
        };
        f(&s2, &drives, &ctx)
    })
    .await
    .map_err(|e| e.to_string())?;
    r.map_err(|e| e.to_string())
}

/// Starts a mount task (also auto-mount).
pub fn start_mount(s: &Arc<RemovableSvc>, who: &str, volume: &str, read_only: bool) -> Res<String> {
    if s.busy_with(volume) {
        return Err(already(volume));
    }
    s.set_pending(volume, Some(RemovableAction::Mount));
    let s2 = s.clone();
    let vol = volume.to_string();
    let r = s
        .ops
        .tasks
        .spawn("removable.mount", who, move |ctx| async move {
            let s3 = s2.clone();
            let v = vol.clone();
            let r = run_action(s2.clone(), ctx, move |svc, drives, c| {
                let m = svc.svc.mount(drives, &v, read_only, c)?;
                if m.ntfs_dirty
                    && let Some((d, vv)) = find_volume(drives, &v)
                {
                    svc.notifier.emit(ntfs_event(
                        &d.title,
                        &v,
                        vv.display_label().unwrap_or(""),
                        &m.mount_point,
                    ));
                }
                Ok(())
            })
            .await;
            s3.set_pending(&vol, None);
            let _ = s3.refresh(true).await;
            r
        });
    match r {
        Ok(id) => Ok(id),
        Err(e) => {
            s.set_pending(volume, None);
            Err(e.into())
        }
    }
}

async fn fresh(s: &Arc<RemovableSvc>) -> Res<Vec<RemovableDrive>> {
    s.refresh(true).await
}

async fn get_state(State(st): State<RmState>) -> Res<Json<RemovableState>> {
    let s = rsvc(&st)?;
    if s.latest_json().is_none() {
        s.refresh(true).await?;
    }
    Ok(Json(s.state()))
}

async fn refresh(State(st): State<RmState>) -> Res<Json<RemovableState>> {
    let s = rsvc(&st)?;
    s.svc.refresh_support();
    s.refresh(true).await?;
    Ok(Json(s.state()))
}

async fn mounts(State(st): State<RmState>) -> Res<Json<Vec<RemovableMount>>> {
    let s = rsvc(&st)?;
    let drives = fresh(&s).await?;
    Ok(Json(rynas_removable::mounts(&drives)))
}

async fn mount(
    State(st): State<RmState>,
    hd: HeaderMap,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> Res<Response> {
    let (s, who) = (rsvc(&st)?, user(&hd)?);
    check_name(&name)?;
    // the body is optional
    let req: MountRequest = if body.iter().all(u8::is_ascii_whitespace) {
        MountRequest::default()
    } else {
        rynas_http::from_slice(&body).map_err(Error::from)?
    };
    let drives = fresh(&s).await?;
    let (_, v) = s.svc.check_mount(&drives, &name)?;
    if v.mount_point.is_some() {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            format!("{name} is already mounted"),
        ));
    }
    Ok(accepted(start_mount(&s, &who, &name, req.read_only)?))
}

async fn unmount(
    State(st): State<RmState>,
    hd: HeaderMap,
    Path(name): Path<String>,
) -> Res<Response> {
    let (s, who) = (rsvc(&st)?, user(&hd)?);
    check_name(&name)?;
    let drives = fresh(&s).await?;
    let mp = s.svc.check_unmount(&drives, &name)?;
    let h = s.holders(&mp);
    if !h.is_empty() {
        return Err(RemovableError::Busy(format!("{name} ({mp})"), h).into());
    }
    if s.busy_with(&name) {
        return Err(already(&name));
    }
    s.set_pending(&name, Some(RemovableAction::Unmount));
    let (s2, vol) = (s.clone(), name.clone());
    let id = s
        .ops
        .tasks
        .spawn("removable.unmount", &who, move |ctx| async move {
            let v = vol.clone();
            let r = run_action(s2.clone(), ctx, move |svc, drives, c| {
                svc.svc.unmount(drives, &v, c)
            })
            .await;
            s2.set_pending(&vol, None);
            let _ = s2.refresh(true).await;
            r
        })
        .inspect_err(|_| s.set_pending(&name, None))?;
    Ok(accepted(id))
}

async fn unlock(
    State(st): State<RmState>,
    hd: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<UnlockVolumeRequest>, JsonRejection>,
) -> Res<Response> {
    let (s, who) = (rsvc(&st)?, user(&hd)?);
    check_name(&name)?;
    let Json(req) = body?;
    if req.passphrase.is_empty() || req.passphrase.len() > 512 {
        return Err(Error::invalid("passphrase", "1 to 512 characters"));
    }
    let drives = fresh(&s).await?;
    s.svc.check_unlock(&drives, &name)?;
    if s.busy_with(&name) {
        return Err(already(&name));
    }
    s.set_pending(&name, Some(RemovableAction::Unlock));
    let (s2, vol) = (s.clone(), name.clone());
    let then_mount = req.mount;
    let pass = req.passphrase;
    let id = s
        .ops
        .tasks
        .spawn("removable.unlock", &who, move |ctx| async move {
            let v = vol.clone();
            let r = run_action(s2.clone(), ctx.clone(), move |svc, drives, c| {
                svc.svc.unlock(drives, &v, &pass, c)
            })
            .await;
            s2.set_pending(&vol, None);
            r?;
            if then_mount {
                // the cleartext device is in the next reading
                let v = vol.clone();
                s2.set_pending(&vol, Some(RemovableAction::Mount));
                let r = run_action(s2.clone(), ctx, move |svc, drives, c| {
                    let m = svc.svc.mount(drives, &v, false, c)?;
                    if m.ntfs_dirty
                        && let Some((d, vv)) = find_volume(drives, &v)
                    {
                        svc.notifier.emit(ntfs_event(
                            &d.title,
                            &v,
                            vv.display_label().unwrap_or(""),
                            &m.mount_point,
                        ));
                    }
                    Ok(())
                })
                .await;
                s2.set_pending(&vol, None);
                let _ = s2.refresh(true).await;
                return r;
            }
            let _ = s2.refresh(true).await;
            Ok(())
        })
        .inspect_err(|_| s.set_pending(&name, None))?;
    Ok(accepted(id))
}

async fn lock(State(st): State<RmState>, hd: HeaderMap, Path(name): Path<String>) -> Res<Response> {
    let (s, who) = (rsvc(&st)?, user(&hd)?);
    check_name(&name)?;
    let drives = fresh(&s).await?;
    s.svc.check_lock(&drives, &name)?;
    if let Some((_, v)) = find_volume(&drives, &name)
        && let Some(mp) = &v.mount_point
    {
        let h = s.holders(mp);
        if !h.is_empty() {
            return Err(RemovableError::Busy(format!("{name} ({mp})"), h).into());
        }
    }
    if s.busy_with(&name) {
        return Err(already(&name));
    }
    s.set_pending(&name, Some(RemovableAction::Lock));
    let (s2, vol) = (s.clone(), name.clone());
    let id = s
        .ops
        .tasks
        .spawn("removable.lock", &who, move |ctx| async move {
            let v = vol.clone();
            let r = run_action(s2.clone(), ctx, move |svc, drives, c| {
                svc.svc.lock(drives, &v, c)
            })
            .await;
            s2.set_pending(&vol, None);
            let _ = s2.refresh(true).await;
            r
        })
        .inspect_err(|_| s.set_pending(&name, None))?;
    Ok(accepted(id))
}

async fn holders(
    State(st): State<RmState>,
    Path(name): Path<String>,
) -> Res<Json<Vec<VolumeHolder>>> {
    let s = rsvc(&st)?;
    check_name(&name)?;
    let drives = fresh(&s).await?;
    let (_, v) = find_volume(&drives, &name)
        .ok_or_else(|| Error::not_found(format!("no removable volume {name}")))?;
    let Some(mp) = v.mount_point.clone() else {
        return Ok(Json(Vec::new()));
    };
    let s2 = s.clone();
    let h = tokio::task::spawn_blocking(move || s2.holders(&mp))
        .await
        .map_err(Error::internal)?;
    Ok(Json(h))
}

/// Safe removal of one drive, inside a task: marks it released (power-off makes it vanish
/// before a reading sees it unmounted), notifies when done.
async fn remove_one(
    s: &Arc<RemovableSvc>,
    ctx: rynas_tasks::TaskCtx,
    drive: &str,
) -> Result<(), String> {
    s.inner.lock().unwrap().released.insert(drive.to_string());
    s.set_pending(drive, Some(RemovableAction::Remove));
    let d = drive.to_string();
    let r = run_action(s.clone(), ctx, move |svc, drives, c| {
        let title = find_drive(drives, &d)
            .map(|x| x.title.clone())
            .unwrap_or_default();
        svc.svc.safely_remove(drives, &d, c).map(|m| (title, m))
    })
    .await;
    s.set_pending(drive, None);
    match &r {
        Ok((title, how)) => {
            if s.settings().notify_safe_removal {
                s.notifier.emit(safe_event(title, drive, *how));
            }
        }
        Err(_) => {
            s.inner.lock().unwrap().released.remove(drive);
        }
    }
    let _ = s.refresh(true).await;
    r.map(|_| ())
}

/// The safe removal of a drive as a task (`POST …/drives/{name}/remove`), for the file
/// manager's eject too.
pub async fn start_remove(s: &Arc<RemovableSvc>, who: &str, drive: &str) -> Res<String> {
    check_name(drive)?;
    let drives = fresh(s).await?;
    s.svc.check_remove(&drives, drive)?;
    if s.busy_with(drive) {
        return Err(already(drive));
    }
    let (s2, d) = (s.clone(), drive.to_string());
    Ok(s.ops
        .tasks
        .spawn("removable.remove", who, move |ctx| async move {
            remove_one(&s2, ctx, &d).await
        })?)
}

async fn remove(
    State(st): State<RmState>,
    hd: HeaderMap,
    Path(name): Path<String>,
) -> Res<Response> {
    let (s, who) = (rsvc(&st)?, user(&hd)?);
    Ok(accepted(start_remove(&s, &who, &name).await?))
}

/// Every drive, one after the other; stops at the first that cannot be removed (busy).
async fn remove_all(State(st): State<RmState>, hd: HeaderMap) -> Res<Response> {
    let (s, who) = (rsvc(&st)?, user(&hd)?);
    let drives = fresh(&s).await?;
    if drives.is_empty() {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            "no removable drive".into(),
        ));
    }
    for d in &drives {
        s.svc.check_remove(&drives, &d.name)?;
    }
    let names: Vec<String> = drives.iter().map(|d| d.name.clone()).collect();
    let s2 = s.clone();
    let id = s
        .ops
        .tasks
        .spawn("removable.remove-all", &who, move |ctx| async move {
            let mut done: HashSet<String> = HashSet::new();
            for n in names {
                // a sibling slot already went with its card reader
                if done.contains(&n) {
                    continue;
                }
                let now = s2.refresh(true).await.map_err(|e| e.2)?;
                if find_drive(&now, &n).is_none() {
                    continue;
                }
                ctx.log(&format!("--- {n}"));
                remove_one(&s2, ctx.clone(), &n).await?;
                done.insert(n);
            }
            Ok(())
        })?;
    Ok(accepted(id))
}

/// Every unmounted, unencrypted, supported volume.
async fn mount_all(State(st): State<RmState>, hd: HeaderMap) -> Res<Response> {
    let (s, who) = (rsvc(&st)?, user(&hd)?);
    let drives = fresh(&s).await?;
    let vols: Vec<String> = drives
        .iter()
        .flat_map(|d| d.volumes.iter())
        .filter(|v| !v.luks && v.supported && v.mount_point.is_none() && !s.busy_with(&v.name))
        .map(|v| v.name.clone())
        .collect();
    if vols.is_empty() {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            "nothing to mount".into(),
        ));
    }
    for v in &vols {
        s.set_pending(v, Some(RemovableAction::Mount));
    }
    let s2 = s.clone();
    let id = s
        .ops
        .tasks
        .spawn("removable.mount-all", &who, move |ctx| async move {
            let mut failed = Vec::new();
            for v in vols {
                let name = v.clone();
                let r = run_action(s2.clone(), ctx.clone(), move |svc, drives, c| {
                    svc.svc.mount(drives, &name, false, c)
                })
                .await;
                s2.set_pending(&v, None);
                match r {
                    Ok(m) if m.ntfs_dirty => {
                        let drives = s2.refresh(true).await.unwrap_or_default();
                        if let Some((d, vv)) = find_volume(&drives, &v) {
                            s2.notifier.emit(ntfs_event(
                                &d.title,
                                &v,
                                vv.display_label().unwrap_or(""),
                                &m.mount_point,
                            ));
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        ctx.log(&e);
                        failed.push(v);
                    }
                }
            }
            let _ = s2.refresh(true).await;
            if failed.is_empty() {
                Ok(())
            } else {
                Err(format!("not mounted: {}", failed.join(", ")))
            }
        })
        .inspect_err(|_| {
            let mut inner = s.inner.lock().unwrap();
            inner.pending.retain(|_, a| *a != RemovableAction::Mount);
        })?;
    Ok(accepted(id))
}

fn settings_state(s: &RemovableSvc, owner: &str) -> Res<RemovableSettingsState> {
    let (eff, cur, _): (
        RemovableSettings,
        RemovableSettings,
        Option<RemovableSettings>,
    ) = docs(&s.ops.config, owner, REMOVABLE_KEY)?;
    Ok(RemovableSettingsState {
        pending: (eff != cur).then_some(eff),
        applied: cur,
        smb_available: s.smb,
    })
}

async fn settings_get(
    State(st): State<RmState>,
    hd: HeaderMap,
) -> Res<Json<RemovableSettingsState>> {
    let (s, owner) = (rsvc(&st)?, user(&hd)?);
    let r = tokio::task::spawn_blocking(move || settings_state(&s, &owner))
        .await
        .map_err(Error::internal)??;
    Ok(Json(r))
}

async fn settings_put(
    State(st): State<RmState>,
    hd: HeaderMap,
    body: Result<Json<RemovableSettings>, JsonRejection>,
) -> Res<Json<RemovableSettingsState>> {
    let (s, owner) = (rsvc(&st)?, user(&hd)?);
    let Json(v) = body?;
    let r = tokio::task::spawn_blocking(move || {
        stage(&s.ops.config, &owner, REMOVABLE_KEY, &v)?;
        settings_state(&s, &owner)
    })
    .await
    .map_err(Error::internal)??;
    Ok(Json(r))
}

pub fn router(st: RmState) -> Router {
    Router::new()
        .route("/v1/removable", get(get_state))
        .route("/v1/removable/refresh", post(refresh))
        .route("/v1/removable/mounts", get(mounts))
        .route(
            "/v1/removable/settings",
            get(settings_get).put(settings_put),
        )
        .route("/v1/removable/mount-all", post(mount_all))
        .route("/v1/removable/remove-all", post(remove_all))
        .route("/v1/removable/volumes/{name}/mount", post(mount))
        .route("/v1/removable/volumes/{name}/unmount", post(unmount))
        .route("/v1/removable/volumes/{name}/unlock", post(unlock))
        .route("/v1/removable/volumes/{name}/lock", post(lock))
        .route("/v1/removable/volumes/{name}/holders", get(holders))
        .route("/v1/removable/drives/{name}/remove", post(remove))
        .with_state(st)
}

/// The settings applied at start (the domain only hands over later applies).
pub fn initial_settings(cfg: &rynas_config::Config) -> RemovableSettings {
    cfg.applied(REMOVABLE_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// The names file of the service (`<state>/removable/names.json`).
pub fn names_file(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join("removable").join("names.json")
}

/// The file manager's view of removable drives (`crate::files::RemovableDrives`): mounted
/// volumes from the last scan, safe removal started as a task.
pub struct FilesDrives {
    svc: Arc<RemovableSvc>,
    rt: tokio::runtime::Handle,
}

impl FilesDrives {
    /// Must be called inside the runtime (it keeps a handle for the worker threads).
    pub fn new(svc: Arc<RemovableSvc>) -> Self {
        Self { svc, rt: tokio::runtime::Handle::current() }
    }
}

impl crate::files::RemovableDrives for FilesDrives {
    fn drive_of(&self, mount: &std::path::Path) -> Option<crate::files::Drive> {
        let st = self.svc.state();
        st.drives.iter().find_map(|d| {
            d.volumes
                .iter()
                .any(|v| v.mount_point.as_deref().map(std::path::Path::new) == Some(mount))
                .then(|| crate::files::Drive { id: d.name.clone(), name: d.title.clone() })
        })
    }

    fn mounted(&self) -> Vec<crate::files::RemovableVolume> {
        rynas_removable::mounts(&self.svc.state().drives)
            .into_iter()
            .map(|m| crate::files::RemovableVolume {
                mount_point: PathBuf::from(m.mount_point),
                label: m.label,
                drive: crate::files::Drive { id: m.drive, name: m.device_name },
            })
            .collect()
    }

    fn access_gid(&self) -> Option<u32> {
        // root's own group is never the access group
        lookup_gid(self.svc.svc.tree().root(), &self.svc.settings().share.group).filter(|&gid| gid != 0)
    }

    fn safely_remove(&self, who: &str, drive: &str) -> Res<()> {
        self.rt.block_on(start_remove(&self.svc, who, drive)).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_lookup() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("etc")).unwrap();
        std::fs::write(
            d.path().join("etc/group"),
            "root:x:0:\nusers:x:100:\nrynas-admin:x:997:alice\n",
        )
        .unwrap();
        assert_eq!(group_gid(d.path(), "rynas-admin"), 997);
        assert_eq!(group_gid(d.path(), "users"), 100);
        assert_eq!(group_gid(d.path(), "nope"), 0);
    }

    #[test]
    fn settings_validation() {
        let mut s = RemovableSettings::default();
        assert!(normalize(s.clone()).is_ok());
        s.share.group = "bad group!".into();
        assert_eq!(
            normalize(s).unwrap_err().field.as_deref(),
            Some("share.group")
        );
    }

    #[test]
    fn events_have_codes_and_fields() {
        let e = unplugged_event("sdc", "SanDisk Ultra", "4C53", &["/media/S".into()]);
        assert_eq!(e.code, "removable.unplugged");
        assert_eq!(e.fields["drive"], "SanDisk Ultra");
        assert_eq!(e.key.as_deref(), Some("removable:unplugged:4C53"));
        let e = safe_event("SanDisk Ultra", "sdc", RemoveMethod::PowerOff);
        assert_eq!(
            (e.code.as_str(), e.fields["method"].as_str()),
            ("removable.safe_to_remove", "power_off")
        );
        let e = ntfs_event("WD", "sdd1", "", "/media/x");
        assert_eq!(e.fields["volume"], "sdd1");
    }
}
