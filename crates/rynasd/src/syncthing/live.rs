//! The installed Syncthing app, live (`docs/phase2/syncthing.md` §4-6, phases S1b-S1d).
//!
//! - Detection: the installed stacks of the catalogue entry (any stack name), from the
//!   stacks folder, and whether their container runs; again on every Docker change hint and
//!   every minute. One event loop per running instance, stopped when the app stops.
//! - Event loop: one long-poll of `/rest/events` with the fixed mask [`model::EVENT_MASK`],
//!   keeping `since`; a full read at start, on an id gap or a smaller id (Syncthing
//!   restarted), after any connection error and after `ConfigSaved` (then without
//!   `db/status` for the folders already known); device stats every 10 minutes. No
//!   `db/status` polling.
//! - Alerts every 30 s from the model (`syncthing.*` notification codes), respecting
//!   [`SyncthingSettings`].
//! - Encrypted shares: [`before_lock`] pauses the folders inside a share (remembered as
//!   paused by rynas in `<state>/syncthing/state.json`) and waits for them to be idle;
//!   [`after_unlock`] resumes them once their marker is back. At every full read a folder
//!   inside a locked share is paused (Syncthing may start before the boot unlock) and a
//!   folder paused by rynas inside an unlocked share is resumed. A folder the user paused is
//!   never resumed by rynas.
//! - The API key comes from the stack's `.env` and never leaves rynasd.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use rynas_api_types::docker::ContainerState;
use rynas_api_types::notify::{NotifyCategory, NotifySeverity};
use rynas_api_types::syncthing::*;
use rynas_notify::Notifier;
use rynas_notify::event::{code, field, firing, oneshot, quietly, resolved};
use serde::Deserialize;
use serde_json::Value;

use super::model::{self, Alert, Alerts, Model, Seen};
use super::{APP_ID, OLD_DATA, TARGET, config, enc, stack_path};
use crate::apps::{AppsState, AppsSvc};
use crate::ops::{Error, Res};

/// Request timeout of the REST calls (the long poll has its own).
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// The long poll's `timeout` parameter (s).
const POLL_S: u64 = 60;
/// Device stats are read this often.
const DEVICE_STATS_MS: u64 = 10 * 60 * 1000;
/// The start time of Syncthing is compared this often (a restart quicker than a request).
const STATUS_EVERY_MS: u64 = 30 * 1000;
/// Detection runs at least this often.
const DETECT_EVERY: Duration = Duration::from_secs(60);
/// Alerts are evaluated this often.
const ALERTS_EVERY: Duration = Duration::from_secs(30);
/// How long a lock waits for Syncthing (pausing, then the folders to stop scanning or
/// syncing). Short: the app's container is restarted around the lock anyway, and the whole
/// lock should answer within the interface's 30 s request timeout.
const IDLE_WAIT: Duration = Duration::from_secs(10);
/// Folder states during which Syncthing touches files.
const BUSY: &[&str] = &["scanning", "syncing", "sync-preparing", "cleaning"];
const SOURCE: &str = "syncthing";
const STATE_FILE: &str = "state.json";

fn now_ms() -> u64 {
    rynas_tasks::now_ms()
}

fn st(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

// ---------------------------------------------------------------------------------------------
// facts about shares (encrypted, snapshots)
// ---------------------------------------------------------------------------------------------

/// What the integration needs to know about the NAS's shares.
pub trait ShareFacts: Send + Sync {
    /// The applied encrypted shares: (path, locked now).
    fn encrypted(&self) -> Vec<(String, bool)>;
    /// The share whose path is `host_path` (or holds it) has an enabled scheduled snapshot
    /// job.
    fn snapshot_scheduled(&self, host_path: &str) -> bool;
}

/// [`ShareFacts`] from the applied configuration and the encrypted folders' backend.
pub struct ServicesFacts(pub Arc<crate::services::Services>);

impl ServicesFacts {
    fn applied<T: serde::de::DeserializeOwned + Default>(&self, key: &str) -> T {
        self.0.ops.config.applied(key).ok().flatten().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }
}

impl ShareFacts for ServicesFacts {
    fn encrypted(&self) -> Vec<(String, bool)> {
        let c: rynas_api_types::SharesConfig = self.applied(rynas_services::SHARES_KEY);
        let crypt = self.0.shares.crypt();
        c.shares
            .iter()
            .filter(|s| s.encrypted)
            .map(|s| (s.path.clone(), crypt.state(&s.path) != rynas_api_types::CryptState::Unlocked))
            .collect()
    }

    fn snapshot_scheduled(&self, host_path: &str) -> bool {
        let c: rynas_api_types::SharesConfig = self.applied(rynas_services::SHARES_KEY);
        let Some(share) = c.shares.iter().filter(|s| config::is_within(host_path, &s.path)).max_by_key(|s| s.path.len()) else {
            return false;
        };
        let snaps: rynas_api_types::snapshots::SnapshotsConfig = self.applied(rynas_btrfs::SNAPSHOTS_KEY);
        let jobs: BTreeSet<&str> = snaps
            .jobs
            .iter()
            .filter(|j| j.share.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&share.name)) || j.subvolume == share.path)
            .map(|j| j.name.as_str())
            .collect();
        let sched: rynas_api_types::scheduler::SchedulerConfig = self.applied(rynas_scheduler::SCHEDULER_KEY);
        sched.tasks.iter().any(|t| {
            t.enabled && matches!(&t.action, rynas_api_types::scheduler::TaskAction::Snapshot { job } if jobs.contains(job.as_str()))
        })
    }
}

// ---------------------------------------------------------------------------------------------
// REST client
// ---------------------------------------------------------------------------------------------

/// The REST interface of one instance (`X-API-Key`).
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    key: String,
}

impl Client {
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
        let r = rb.header("X-API-Key", &self.key).send().await.map_err(|e| {
            if e.is_timeout() {
                "Syncthing did not answer in time".to_string()
            } else if e.is_connect() {
                "Syncthing does not accept connections".to_string()
            } else {
                format!("Syncthing: {e}")
            }
        })?;
        let status = r.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err("Syncthing refused the API key of the app".into());
        }
        if !status.is_success() {
            return Err(format!("Syncthing answered {status}"));
        }
        r.text().await.map_err(|e| e.to_string())
    }

    async fn text(&self, path: &str) -> Result<String, String> {
        self.send(self.http.get(format!("{}{path}", self.base))).await
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        let t = self.text(path).await?;
        serde_json::from_str(&t).map_err(|e| format!("Syncthing answered something unexpected: {e}"))
    }

    /// Events after `since` (waits up to `timeout` s for one).
    async fn events(&self, since: u64, timeout: u64, limit: Option<u32>) -> Result<Vec<Value>, String> {
        let mut url = format!("{}/rest/events?since={since}&timeout={timeout}&events={}", self.base, model::EVENT_MASK);
        if let Some(l) = limit {
            url.push_str(&format!("&limit={l}"));
        }
        let t = self.send(self.http.get(url).timeout(Duration::from_secs(timeout + 15))).await?;
        let v: Value = serde_json::from_str(&t).map_err(|e| e.to_string())?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    /// The id of the latest event of the subscription (0: none yet).
    async fn position(&self) -> Result<u64, String> {
        Ok(self.events(0, 1, Some(1)).await?.iter().filter_map(|e| e.get("id").and_then(Value::as_u64)).max().unwrap_or(0))
    }

    pub async fn set_paused(&self, id: &str, paused: bool) -> Result<(), String> {
        let rb = self
            .http
            .patch(format!("{}/rest/config/folders/{}", self.base, enc(id)))
            .header("Content-Type", "application/json")
            .body(serde_json::json!({ "paused": paused }).to_string());
        self.send(rb).await.map(|_| ())
    }

    pub async fn scan(&self, id: &str) -> Result<(), String> {
        self.send(self.http.post(format!("{}/rest/db/scan?folder={}", self.base, enc(id)))).await.map(|_| ())
    }

    async fn folder_state(&self, id: &str) -> Result<String, String> {
        self.get(&format!("/rest/db/status?folder={}", enc(id))).await.map(|v| st(&v, "state"))
    }
}

// ---------------------------------------------------------------------------------------------
// instances
// ---------------------------------------------------------------------------------------------

/// A folder of the app (container path) → the NAS: (container prefix, host path, share).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub container: String,
    pub host: String,
    pub share: Option<String>,
}

/// The binds of a stack, from its `.env`: the share set items at `/shares/<name>`, and the
/// single share of the older layout at `/data`.
pub fn binds_of_env(stack_dir: &str, env_text: &str) -> Vec<Bind> {
    let env = rynas_apps::render::parse_env(env_text);
    let mut out = Vec::new();
    if let Some(items) = env.get("SYNC_SHARES").and_then(|v| rynas_apps::render::decode_share_set(v).ok()) {
        for i in items {
            out.push(Bind { container: format!("{TARGET}/{}", i.name), host: stack_path(stack_dir, &i.path), share: Some(i.name) });
        }
    }
    if let Some(i) = env.get("SYNC_SHARE").and_then(|v| rynas_apps::render::decode_share_set(v).ok()).and_then(|v| v.into_iter().next()) {
        out.push(Bind { container: OLD_DATA.into(), host: stack_path(stack_dir, &i.path), share: Some(i.name) });
    }
    out
}

/// A container path → (host path, share, part below the share).
pub fn host_of(binds: &[Bind], path: &str) -> Option<(String, Option<String>, String)> {
    let b = binds.iter().filter(|b| config::is_within(path, &b.container)).max_by_key(|b| b.container.len())?;
    let rest = config::under(path, &b.container)?;
    Some((config::join(&b.host, &rest), b.share.clone(), rest))
}

#[derive(Debug, Default)]
struct Inner {
    model: Model,
    reachable: bool,
    error: Option<String>,
    device_stats_ms: u64,
    /// `startTime` of `/rest/system/status` at the last full read.
    start_time: String,
}

/// A running instance and what is known of it.
pub struct Instance {
    pub stack: String,
    client: Client,
    binds: Vec<Bind>,
    inner: Mutex<Inner>,
}

impl Instance {
    pub fn new(stack: &str, client: Client, binds: Vec<Bind>) -> Arc<Self> {
        Arc::new(Self {
            stack: stack.into(),
            client,
            binds,
            inner: Mutex::new(Inner { error: Some("Syncthing has not answered yet".into()), ..Default::default() }),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        f(&mut self.inner.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn unreachable(&self, e: &str) {
        self.with(|i| {
            i.reachable = false;
            i.error = Some(e.to_string());
            for d in &mut i.model.devices {
                d.connected = false;
            }
        });
    }

    fn host(&self, path: &str) -> Option<String> {
        host_of(&self.binds, path).map(|(h, _, _)| h)
    }
}

/// An installed stack of the entry as detection found it.
#[derive(Debug, Clone)]
struct Detected {
    installed: SyncthingInstalled,
    binds: Vec<Bind>,
    /// Base URL and key, or why there is none.
    api: Result<(String, String), String>,
}

/// What survives restarts: the settings and the folders rynas paused.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(default)]
pub struct Persist {
    pub settings: SyncthingSettings,
    /// stack → folder ids paused by rynas while their encrypted share was locked.
    pub paused_by_rynas: BTreeMap<String, BTreeSet<String>>,
    /// stack → the notified conditions of its alerts ([`Alerts::persisted`]).
    pub alerts: BTreeMap<String, Alerts>,
}

struct Loop {
    sig: (String, String),
    task: tokio::task::JoinHandle<()>,
}

/// The integration: detection, the instances' loops, alerts, the crypt hooks.
pub struct Live {
    /// The configuration root (`/`, or the dry root): host paths are checked below it.
    root: PathBuf,
    facts: Arc<dyn ShareFacts>,
    notifier: Arc<dyn Notifier>,
    file: PathBuf,
    persist: Mutex<Persist>,
    detected: Mutex<Vec<Detected>>,
    instances: Mutex<BTreeMap<String, Arc<Instance>>>,
    loops: Mutex<BTreeMap<String, Loop>>,
    alerts: Mutex<BTreeMap<String, Alerts>>,
    /// `stack/folder` kept paused after an unlock (marker missing), already notified.
    kept_paused: Mutex<BTreeSet<String>>,
    /// Encrypted shares being locked now: locked for [`Live::reconcile`] until the lock ends.
    locking: Mutex<BTreeSet<String>>,
    /// Detection ran once (before it, no stack counts as gone).
    detected_once: std::sync::atomic::AtomicBool,
    handle: tokio::runtime::Handle,
    wake: tokio::sync::Notify,
}

static LIVE: OnceLock<Arc<Live>> = OnceLock::new();

/// The integration of this rynasd, once [`install`]ed.
pub fn get() -> Option<Arc<Live>> {
    LIVE.get().cloned()
}

/// Makes `live` the integration the API and the crypt hooks use.
pub fn install(live: Arc<Live>) {
    let _ = LIVE.set(live);
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Live {
    /// Must run inside the tokio runtime. `state_dir`: rynasd's state folder.
    pub fn new(root: &Path, state_dir: &Path, facts: Arc<dyn ShareFacts>, notifier: Arc<dyn Notifier>) -> Arc<Self> {
        let file = state_dir.join("syncthing").join(STATE_FILE);
        let persist: Persist = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let alerts = persist.alerts.clone();
        Arc::new(Self {
            root: root.to_path_buf(),
            facts,
            notifier,
            file,
            persist: Mutex::new(persist),
            detected: Mutex::new(Vec::new()),
            instances: Mutex::new(BTreeMap::new()),
            loops: Mutex::new(BTreeMap::new()),
            alerts: Mutex::new(alerts),
            kept_paused: Mutex::new(BTreeSet::new()),
            locking: Mutex::new(BTreeSet::new()),
            detected_once: std::sync::atomic::AtomicBool::new(false),
            handle: tokio::runtime::Handle::current(),
            wake: tokio::sync::Notify::new(),
        })
    }

    pub fn settings(&self) -> SyncthingSettings {
        lock(&self.persist).settings.clone()
    }

    /// The persisted state now (tests, diagnostics).
    pub fn persisted(&self) -> Persist {
        lock(&self.persist).clone()
    }

    fn save(&self, p: &Persist) {
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let dir = self.file.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
            std::fs::create_dir_all(dir)?;
            let tmp = self.file.with_extension("json.tmp");
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(p).map_err(std::io::Error::other)?)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.file)
        };
        if let Err(e) = write() {
            tracing::warn!("syncthing: {} not written: {e}", self.file.display());
        }
    }

    fn update_persist(&self, f: impl FnOnce(&mut Persist)) {
        let mut g = lock(&self.persist);
        let before = g.clone();
        f(&mut g);
        if *g != before {
            let p = g.clone();
            drop(g);
            self.save(&p);
        }
    }

    pub fn set_settings(&self, s: SyncthingSettings) {
        self.update_persist(|p| p.settings = s);
    }

    fn flagged(&self, stack: &str, id: &str) -> bool {
        lock(&self.persist).paused_by_rynas.get(stack).is_some_and(|s| s.contains(id))
    }

    fn flag(&self, stack: &str, id: &str, on: bool) {
        self.update_persist(|p| {
            let set = p.paused_by_rynas.entry(stack.to_string()).or_default();
            if on {
                set.insert(id.to_string());
            } else {
                set.remove(id);
            }
            if set.is_empty() {
                p.paused_by_rynas.remove(stack);
            }
        });
    }

    fn instances(&self) -> Vec<Arc<Instance>> {
        lock(&self.instances).values().cloned().collect()
    }

    fn instance(&self, stack: &str) -> Option<Arc<Instance>> {
        lock(&self.instances).get(stack).cloned()
    }

    /// Adds an instance without detection or loop (tests).
    pub fn add_instance(&self, inst: Arc<Instance>, running: bool) {
        lock(&self.detected).push(Detected {
            installed: SyncthingInstalled { stack: inst.stack.clone(), version: "2.0.0".into(), running },
            binds: inst.binds.clone(),
            api: Ok((inst.client.base.clone(), String::new())),
        });
        lock(&self.instances).insert(inst.stack.clone(), inst);
        self.detected_once.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Asks detection to run now (an app installed, started, stopped...).
    pub fn poke(&self) {
        self.wake.notify_one();
    }

    // -----------------------------------------------------------------------------------------
    // detection
    // -----------------------------------------------------------------------------------------

    async fn detect(self: &Arc<Self>, apps: &Arc<AppsSvc>) {
        let a = apps.clone();
        let found = tokio::task::spawn_blocking(move || {
            let Some(l) = a.docker.layout() else { return Vec::new() };
            a.on_disk()
                .into_iter()
                .filter(|d| d.meta.app == APP_ID)
                .map(|d| {
                    let binds = binds_of_env(&l.stack(&d.stack), &d.env);
                    (d.stack, d.meta.version.to_string(), binds, super::api_of_env(&d.env))
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        let running: BTreeSet<String> = match apps.docker.containers().await {
            Ok(cs) => cs.into_iter().filter(|c| c.c.state == ContainerState::Running).filter_map(|c| c.c.stack).collect(),
            // Docker did not answer: what ran keeps running for now
            Err(_) => lock(&self.detected).iter().filter(|d| d.installed.running).map(|d| d.installed.stack.clone()).collect(),
        };
        let mut det: Vec<Detected> = found
            .into_iter()
            .map(|(stack, version, binds, api)| Detected {
                installed: SyncthingInstalled { running: running.contains(&stack), stack, version },
                binds,
                api,
            })
            .collect();
        det.sort_by(|x, y| x.installed.stack.cmp(&y.installed.stack));
        // loops follow the running instances
        let want: BTreeMap<String, ((String, String), Vec<Bind>)> = det
            .iter()
            .filter(|d| d.installed.running)
            .filter_map(|d| Some((d.installed.stack.clone(), (d.api.clone().ok()?, d.binds.clone()))))
            .collect();
        {
            let mut loops = lock(&self.loops);
            // a new share set changes the binds only: the loop starts again with them, or a
            // folder on a newly added encrypted share would not be paused before its lock
            let stale: Vec<String> = {
                let insts = lock(&self.instances);
                loops
                    .iter()
                    .filter(|(k, l)| {
                        want.get(*k).is_none_or(|(sig, binds)| *sig != l.sig || insts.get(*k).is_none_or(|i| i.binds != *binds))
                    })
                    .map(|(k, _)| k.clone())
                    .collect()
            };
            for k in stale {
                if let Some(l) = loops.remove(&k) {
                    l.task.abort();
                    tracing::info!("syncthing: {k} stopped or changed: its event loop ends");
                }
                lock(&self.instances).remove(&k);
            }
            for (stack, (sig, binds)) in want {
                if loops.contains_key(&stack) {
                    continue;
                }
                let client = match Client::new(&sig.0, &sig.1) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("syncthing: {stack}: {e}");
                        continue;
                    }
                };
                let inst = Instance::new(&stack, client, binds);
                lock(&self.instances).insert(stack.clone(), inst.clone());
                let me = self.clone();
                let task = tokio::spawn(async move { me.run(inst).await });
                tracing::info!("syncthing: {stack} runs: event loop started");
                loops.insert(stack, Loop { sig, task });
            }
        }
        *lock(&self.detected) = det;
        self.detected_once.store(true, std::sync::atomic::Ordering::Release);
    }

    /// The instance's loop: sessions, with a growing pause after a failure.
    async fn run(self: Arc<Self>, inst: Arc<Instance>) {
        let mut pause = Duration::from_secs(2);
        loop {
            let started = std::time::Instant::now();
            if let Err(e) = self.session(&inst).await {
                if inst.with(|i| i.reachable) {
                    tracing::warn!("syncthing: {}: {e}", inst.stack);
                }
                inst.unreachable(&e);
            }
            if started.elapsed() > Duration::from_secs(120) {
                pause = Duration::from_secs(2);
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_secs(30));
        }
    }

    /// One connection: position, full read, then events until an error.
    async fn session(&self, inst: &Arc<Instance>) -> Result<(), String> {
        let c = inst.client.clone();
        let mut last = c.position().await?;
        self.full_read(inst, true).await?;
        self.reconcile(inst).await;
        let mut status_ms = now_ms();
        loop {
            // a restart quicker than a request (the long poll of the old process answered,
            // the new one numbers its events from 1 again): its start time changed
            if now_ms().saturating_sub(status_ms) >= STATUS_EVERY_MS {
                status_ms = now_ms();
                let started = st(&c.get("/rest/system/status").await?, "startTime");
                if !started.is_empty() && inst.with(|i| i.start_time != started) {
                    tracing::info!("syncthing: {}: Syncthing restarted: reading everything again", inst.stack);
                    last = c.position().await?;
                    self.full_read(inst, true).await?;
                    self.reconcile(inst).await;
                    continue;
                }
            }
            if now_ms().saturating_sub(inst.with(|i| i.device_stats_ms)) >= DEVICE_STATS_MS {
                let v = c.get("/rest/stats/device").await?;
                inst.with(|i| {
                    i.model.set_device_stats(&v);
                    i.device_stats_ms = now_ms();
                });
            }
            let evs = c.events(last, POLL_S, None).await?;
            let (resync, next) = model::check_ids(last, &evs);
            last = next;
            if resync {
                tracing::info!("syncthing: {}: events missed or Syncthing restarted: reading everything again", inst.stack);
                self.full_read(inst, true).await?;
                self.reconcile(inst).await;
                continue;
            }
            let now = now_ms();
            let mut full = false;
            let mut conflicts = Vec::new();
            inst.with(|i| {
                for ev in &evs {
                    let a = i.model.apply(ev, now);
                    full |= a.full_read;
                    conflicts.extend(a.conflicts);
                }
            });
            if !conflicts.is_empty() {
                let mut al = lock(&self.alerts);
                let a = al.entry(inst.stack.clone()).or_default();
                for (f, p) in &conflicts {
                    a.conflict(now, f, p);
                }
            }
            if full {
                self.full_read(inst, false).await?;
                self.reconcile(inst).await;
            }
        }
    }

    /// Reads configuration and state; `db/status` and completions for every folder with
    /// `all`, else only for folders not known yet.
    async fn full_read(&self, inst: &Instance, all: bool) -> Result<(), String> {
        let c = &inst.client;
        let status = c.get("/rest/system/status").await?;
        let version = c.get("/rest/system/version").await.map(|v| st(&v, "version")).unwrap_or_default();
        let folders = model::folders_of_config(&c.get("/rest/config/folders").await?);
        let devices = model::devices_of_config(&c.get("/rest/config/devices").await?);
        let conns = c.get("/rest/system/connections").await.ok();
        let dstats = c.get("/rest/stats/device").await.ok();
        let fstats = c.get("/rest/stats/folder").await.ok();
        // the counter needs the metrics endpoint (on by default); skipped when it is not there
        let counters = c.text("/metrics").await.ok().map(|t| model::conflict_counters(&t));
        let my_id = st(&status, "myID");
        let start_time = st(&status, "startTime");
        let known: BTreeSet<String> = inst.with(|i| i.model.folders.iter().map(|f| f.id.clone()).collect());
        let mut statuses = Vec::new();
        for f in folders.iter().filter(|f| !f.paused && (all || !known.contains(&f.id))) {
            let s = c.get(&format!("/rest/db/status?folder={}", enc(&f.id))).await.ok();
            let e = c.get(&format!("/rest/folder/errors?folder={}", enc(&f.id))).await.ok();
            let mut comp = Vec::new();
            for d in f.devices.iter().filter(|d| **d != my_id) {
                if let Ok(v) = c.get(&format!("/rest/db/completion?folder={}&device={}", enc(&f.id), enc(d))).await {
                    comp.push((d.clone(), v.get("completion").and_then(Value::as_f64).unwrap_or(0.0), v.get("needBytes").and_then(Value::as_u64).unwrap_or(0)));
                }
            }
            statuses.push((f.id.clone(), s, e, comp));
        }
        let now = now_ms();
        inst.with(|i| {
            let m = &mut i.model;
            m.my_id = my_id;
            m.version = version;
            m.replace_config(folders, devices);
            if let Some(v) = &conns {
                m.set_connections(v);
            }
            if let Some(v) = &dstats {
                m.set_device_stats(v);
                i.device_stats_ms = now;
            }
            if let Some(v) = &fstats {
                m.set_folder_stats(v);
            }
            for (id, s, e, comp) in statuses {
                let Some(f) = m.folder_mut(&id) else { continue };
                if let Some(s) = s {
                    f.set_status(&s);
                }
                if let Some(e) = e {
                    f.set_errors(e.get("errors").unwrap_or(&Value::Null));
                }
                for (d, pct, need) in comp {
                    f.completion.insert(d, (pct, need));
                }
            }
            if let Some(cn) = &counters {
                for f in &mut m.folders {
                    f.conflicts_base = cn.get(&f.id).copied().unwrap_or(0);
                    f.conflicts_seen = 0;
                }
            }
            m.read_ms = Some(now);
            i.start_time = start_time;
            i.reachable = true;
            i.error = None;
        });
        Ok(())
    }

    /// The encrypted shares; one being locked now counts as locked.
    async fn encrypted(&self) -> Vec<(String, bool)> {
        let f = self.facts.clone();
        let mut v = tokio::task::spawn_blocking(move || f.encrypted()).await.unwrap_or_default();
        let locking = lock(&self.locking).clone();
        for (p, locked) in &mut v {
            *locked |= locking.contains(p);
        }
        v
    }

    /// After a full read: folders inside a locked encrypted share are paused (and
    /// remembered), folders rynas paused inside an unlocked share are resumed when their
    /// marker is there; a remembered folder the user resumed is forgotten.
    async fn reconcile(&self, inst: &Instance) {
        let enc = self.encrypted().await;
        let folders = inst.with(|i| i.model.folders.clone());
        for f in folders {
            if !f.paused {
                // running again, whoever resumed it
                lock(&self.kept_paused).remove(&format!("{}/{}", inst.stack, f.id));
            }
            let flagged = self.flagged(&inst.stack, &f.id);
            let share = inst.host(&f.path).and_then(|host| enc.iter().filter(|(p, _)| config::is_within(&host, p)).max_by_key(|(p, _)| p.len()).cloned());
            match share {
                // locked, or being locked: never resumed, paused when running
                Some((p, true)) => {
                    if f.paused {
                        continue;
                    }
                    self.flag(&inst.stack, &f.id, true);
                    match inst.client.set_paused(&f.id, true).await {
                        Ok(()) => {
                            tracing::info!("syncthing: {}: folder {} paused: its encrypted share {p} is locked", inst.stack, f.id);
                            self.flag(&inst.stack, &f.id, true);
                            inst.with(|i| set_paused(&mut i.model, &f.id, true));
                        }
                        Err(e) => {
                            self.flag(&inst.stack, &f.id, false);
                            tracing::warn!("syncthing: {}: folder {} inside the locked share {p} not paused: {e}", inst.stack, f.id);
                        }
                    }
                }
                // a remembered folder found running: the user resumed it
                _ if flagged && !f.paused => self.flag(&inst.stack, &f.id, false),
                _ if flagged => self.resume_checked(inst, &f).await,
                _ => {}
            }
        }
    }

    /// Resumes a folder paused by rynas when its marker is on the NAS; else it stays
    /// paused, with a notification.
    async fn resume_checked(&self, inst: &Instance, f: &model::Folder) {
        let Some((host, share, _)) = host_of(&inst.binds, &f.path) else { return };
        let (root, h, name, id) = (self.root.clone(), host.clone(), f.marker_name.clone(), f.id.clone());
        let marker = tokio::task::spawn_blocking(move || super::marker_state(&root, &h, &name, &id)).await.unwrap_or(MarkerState::Unknown);
        if marker != MarkerState::Verified {
            // once per folder until it is resumed (full reads come back to it)
            if !lock(&self.kept_paused).insert(format!("{}/{}", inst.stack, f.id)) {
                return;
            }
            tracing::warn!("syncthing: {}: folder {} stays paused: its marker is not on {host}", inst.stack, f.id);
            let share = share.unwrap_or_else(|| host.clone());
            let mut ev = oneshot(
                NotifyCategory::App,
                NotifySeverity::Warning,
                SOURCE,
                format!("syncthing:kept-paused:{}:{}", inst.stack, f.id),
                format!("Syncthing folder {} stays paused", f.label),
            );
            ev.body = format!(
                "After the unlock of {share}, the folder's marker (.stfolder) was not found: rynas did not resume it. Check the folder, then resume it in Syncthing."
            );
            let ev = field(field(field(code(ev, "syncthing.folder_kept_paused"), "stack", &inst.stack), "folder", &f.label), "share", share);
            self.notifier.emit(ev);
            return;
        }
        match inst.client.set_paused(&f.id, false).await {
            Ok(()) => {
                lock(&self.kept_paused).remove(&format!("{}/{}", inst.stack, f.id));
                tracing::info!("syncthing: {}: folder {} resumed after the unlock", inst.stack, f.id);
                self.flag(&inst.stack, &f.id, false);
                inst.with(|i| set_paused(&mut i.model, &f.id, false));
            }
            Err(e) => tracing::warn!("syncthing: {}: folder {} not resumed: {e}", inst.stack, f.id),
        }
    }

    // -----------------------------------------------------------------------------------------
    // encrypted shares
    // -----------------------------------------------------------------------------------------

    /// Before the lock of the share at `share_path`: the share counts as locked for the
    /// event loops until [`Live::lock_finished`]; its running folders, in every instance, are
    /// paused at once (the ones the user paused are left alone) and remembered, then the lock
    /// waits until none scans or syncs, with one deadline for all ([`IDLE_WAIT`]). An
    /// instance that does not answer is skipped with a warning: its folders go to `error`
    /// once the plaintext is gone, which is safe.
    pub async fn pause_inside(&self, share_path: &str) {
        lock(&self.locking).insert(share_path.to_string());
        let deadline = tokio::time::Instant::now() + IDLE_WAIT;
        let insts = self.instances();
        let lists = futures_util::future::join_all(insts.iter().map(|i| async move {
            match tokio::time::timeout_at(deadline, i.client.get("/rest/config/folders")).await {
                Ok(Ok(v)) => model::folders_of_config(&v),
                Ok(Err(e)) => {
                    tracing::warn!("syncthing: {}: folders inside {share_path} not paused before the lock: {e}", i.stack);
                    Vec::new()
                }
                Err(_) => {
                    tracing::warn!("syncthing: {}: no answer before the lock of {share_path}: locking anyway", i.stack);
                    Vec::new()
                }
            }
        }))
        .await;
        let mut to_pause = Vec::new();
        let mut wait = Vec::new();
        for (inst, folders) in insts.iter().zip(lists) {
            for f in folders {
                if !inst.host(&f.path).is_some_and(|h| config::is_within(&h, share_path)) {
                    continue;
                }
                if !f.paused {
                    // remembered first: a pause cut short leaves a remembered running
                    // folder, which a later full read forgets, never a paused folder that
                    // nobody resumes
                    self.flag(&inst.stack, &f.id, true);
                    to_pause.push((inst.clone(), f.id));
                } else if self.flagged(&inst.stack, &f.id) {
                    wait.push((inst.clone(), f.id));
                }
            }
        }
        let results = futures_util::future::join_all(
            to_pause.iter().map(|(inst, id)| async move { tokio::time::timeout_at(deadline, inst.client.set_paused(id, true)).await }),
        )
        .await;
        for ((inst, id), r) in to_pause.into_iter().zip(results) {
            match r {
                Ok(Ok(())) => {
                    tracing::info!("syncthing: {}: folder {id} paused before the lock of {share_path}", inst.stack);
                    // again: an event loop may have read the folder running in between
                    self.flag(&inst.stack, &id, true);
                    inst.with(|i| set_paused(&mut i.model, &id, true));
                    wait.push((inst, id));
                }
                Ok(Err(e)) => {
                    self.flag(&inst.stack, &id, false);
                    tracing::warn!("syncthing: {}: folder {id} not paused before the lock: {e}", inst.stack);
                }
                // the answer may still come: kept remembered, sorted out by the next full read
                Err(_) => tracing::warn!("syncthing: {}: folder {id}: no answer to the pause before the lock", inst.stack),
            }
        }
        while !wait.is_empty() {
            let states = futures_util::future::join_all(
                wait.iter().map(|(inst, id)| async move { tokio::time::timeout_at(deadline, inst.client.folder_state(id)).await }),
            )
            .await;
            wait = wait.into_iter().zip(states).filter(|(_, s)| matches!(s, Ok(Ok(s)) if BUSY.contains(&s.as_str()))).map(|(w, _)| w).collect();
            if wait.is_empty() {
                break;
            }
            if tokio::time::Instant::now() + Duration::from_millis(500) >= deadline {
                let names: Vec<String> = wait.iter().map(|(i, id)| format!("{}/{id}", i.stack)).collect();
                tracing::warn!("syncthing: folders still busy after {} s, locking anyway: {}", IDLE_WAIT.as_secs(), names.join(", "));
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// The lock of the share at `share_path` ended: the event loops see its real state again;
    /// a failed lock resumes the folders [`Live::pause_inside`] paused (in the background).
    pub fn lock_finished(self: &Arc<Self>, share_path: &str, locked: bool) {
        lock(&self.locking).remove(share_path);
        if !locked {
            let (me, p) = (self.clone(), share_path.to_string());
            self.handle.spawn(async move { me.resume_inside(&p).await });
        }
    }

    /// After the unlock of the share at `share_path`: the folders rynas paused inside it are
    /// resumed when their marker is there.
    pub async fn resume_inside(&self, share_path: &str) {
        // a lock that failed after the unmount leaves it locked: nothing to resume yet
        if self.encrypted().await.iter().any(|(p, locked)| *locked && config::is_within(share_path, p)) {
            return;
        }
        for inst in self.instances() {
            let folders = match inst.client.get("/rest/config/folders").await {
                Ok(v) => model::folders_of_config(&v),
                Err(e) => {
                    // resumed at the next full read
                    tracing::warn!("syncthing: {}: folders inside {share_path} not resumed now: {e}", inst.stack);
                    continue;
                }
            };
            for f in folders {
                if !self.flagged(&inst.stack, &f.id) || !inst.host(&f.path).is_some_and(|h| config::is_within(&h, share_path)) {
                    continue;
                }
                if !f.paused {
                    self.flag(&inst.stack, &f.id, false);
                    continue;
                }
                self.resume_checked(&inst, &f).await;
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // alerts
    // -----------------------------------------------------------------------------------------

    /// Evaluates the alerts of every installed instance and emits the transitions.
    pub fn alerts_now(&self, now: u64) {
        let set = self.settings();
        if !self.detected_once.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let det = lock(&self.detected).clone();
        let mut al = lock(&self.alerts);
        // a stack that went away: what it had open is resolved, quietly
        let gone: Vec<String> = al.keys().filter(|k| !det.iter().any(|d| d.installed.stack == **k)).cloned().collect();
        for stack in gone {
            if let Some(mut a) = al.remove(&stack) {
                for alert in a.resolve_all() {
                    self.notifier.emit(quietly(event_of(&stack, alert)));
                }
            }
        }
        for d in &det {
            let stack = d.installed.stack.clone();
            let inst = self.instance(&stack);
            let snapshot = inst.as_ref().and_then(|i| i.with(|x| x.reachable.then(|| x.model.clone())));
            let seen = match (&snapshot, d.installed.running) {
                (_, false) => Seen::Stopped,
                (Some(m), true) => Seen::Reachable(m),
                (None, true) => Seen::Unreachable,
            };
            let a = al.entry(stack.clone()).or_default();
            for alert in a.evaluate(now, seen, set.device_unseen_days, set.conflicts_alert) {
                self.notifier.emit(event_of(&stack, alert));
            }
        }
        let kept: BTreeMap<String, Alerts> = al.iter().map(|(k, a)| (k.clone(), a.persisted())).collect();
        drop(al);
        self.update_persist(|p| p.alerts = kept);
    }

    // -----------------------------------------------------------------------------------------
    // the page
    // -----------------------------------------------------------------------------------------

    /// The state of the stack `want` (default: the first installed one).
    pub fn state(&self, want: Option<&str>) -> SyncthingState {
        let settings = self.settings();
        let det = lock(&self.detected).clone();
        let Some(d) = want.and_then(|w| det.iter().find(|d| d.installed.stack == w)).or_else(|| det.first()) else {
            return SyncthingState { settings, ..Default::default() };
        };
        let stack = d.installed.stack.clone();
        let mut out = SyncthingState {
            installed: Some(d.installed.clone()),
            other_stacks: det.iter().map(|x| x.installed.stack.clone()).filter(|s| *s != stack).collect(),
            settings,
            ..Default::default()
        };
        // versions come from the snapshots of the shares the app synchronizes
        let mut seen = BTreeSet::new();
        for b in &d.binds {
            if let Some(share) = &b.share
                && seen.insert(share.clone())
            {
                out.shares.push(SyncthingShareNote { share: share.clone(), snapshots_scheduled: self.facts.snapshot_scheduled(&b.host) });
            }
        }
        if !d.installed.running {
            out.error = Some("the app is stopped".into());
            return out;
        }
        if let Err(e) = &d.api {
            out.error = Some(e.clone());
            return out;
        }
        let Some(inst) = self.instance(&stack) else {
            out.error = Some("Syncthing has not answered yet".into());
            return out;
        };
        let enc = self.facts.encrypted();
        let flagged: BTreeSet<String> = lock(&self.persist).paused_by_rynas.get(&stack).cloned().unwrap_or_default();
        // conflict copies that no longer exist are forgotten
        let files: Vec<(String, Vec<String>)> = inst.with(|i| i.model.folders.iter().map(|f| (f.path.clone(), f.conflict_files.iter().cloned().collect())).collect());
        let gone: Vec<(String, String)> = files
            .iter()
            .filter_map(|(path, list)| Some((path, list, inst.host(path)?)))
            .flat_map(|(path, list, host)| {
                list.iter()
                    .filter(|p| !p.contains("..") && std::fs::symlink_metadata(super::real_in(&self.root, &config::join(&host, p))).is_err())
                    .map(|p| (path.clone(), p.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        inst.with(|i| {
            for (path, p) in &gone {
                if let Some(f) = i.model.folders.iter_mut().find(|f| f.path == *path) {
                    f.conflict_files.retain(|x| x != p);
                }
            }
            out.reachable = i.reachable;
            out.error = i.error.clone();
            let m = &i.model;
            out.read_ms = m.read_ms;
            if !m.my_id.is_empty() {
                out.device = Some(SyncthingSelf { id_short: model::short(&m.my_id), name: m.device_name(&m.my_id), version: m.version.clone() });
            }
            out.folders = m
                .folders
                .iter()
                .map(|f| {
                    let mapped = host_of(&inst.binds, &f.path);
                    let locked = mapped.as_ref().is_some_and(|(h, _, _)| enc.iter().any(|(p, l)| *l && config::is_within(h, p)));
                    SyncthingFolder {
                        id: f.id.clone(),
                        label: f.label.clone(),
                        path: f.path.clone(),
                        share: mapped.as_ref().and_then(|(_, s, _)| s.clone()),
                        sub_path: mapped.as_ref().map(|(_, _, r)| r.clone()),
                        folder_type: f.folder_type.clone(),
                        state: f.state.clone(),
                        paused: f.paused,
                        paused_by_rynas: flagged.contains(&f.id),
                        locked,
                        error: f.error.clone(),
                        errors: f.errors.iter().map(|(p, e)| SyncthingItemError { path: p.clone(), error: e.clone() }).collect(),
                        error_count: f.error_count,
                        need_bytes: f.need_bytes,
                        global_bytes: f.global_bytes,
                        in_sync_bytes: f.in_sync_bytes,
                        completion: f
                            .devices
                            .iter()
                            .filter(|d| **d != m.my_id)
                            .map(|d| {
                                let (pct, need) = f.completion.get(d).copied().unwrap_or((0.0, 0));
                                SyncthingCompletion {
                                    id_short: model::short(d),
                                    name: m.device_name(d),
                                    completion: pct,
                                    need_bytes: need,
                                    connected: m.devices.iter().any(|x| x.id == *d && x.connected),
                                }
                            })
                            .collect(),
                        last_scan_ms: f.last_scan_ms,
                        last_file: f.last_file.clone(),
                        conflicts: f.conflicts(),
                        conflict_files: f.conflict_files.iter().cloned().collect(),
                    }
                })
                .collect();
            out.devices = m
                .devices
                .iter()
                .filter(|d| d.id != m.my_id)
                .map(|d| SyncthingDevice {
                    id_short: model::short(&d.id),
                    name: m.device_name(&d.id),
                    connected: d.connected,
                    paused: d.paused,
                    address: d.address.clone(),
                    connection_type: d.connection_type.clone(),
                    last_seen_ms: d.last_seen_ms,
                    folders: m.folders.iter().filter(|f| f.devices.contains(&d.id)).map(|f| f.label.clone()).collect(),
                })
                .collect();
        });
        out
    }
}

fn set_paused(m: &mut Model, id: &str, paused: bool) {
    if let Some(f) = m.folder_mut(id) {
        f.paused = paused;
        f.state = if paused { "paused".into() } else { "unknown".into() };
        if paused {
            f.error = None;
        }
    }
}

/// The notification of an alert.
fn event_of(stack: &str, a: Alert) -> rynas_api_types::notify::NewEvent {
    let cat = NotifyCategory::App;
    match a {
        Alert::Conflict { folder, label, count, files } => {
            let list = files.join(", ");
            let mut ev = oneshot(
                cat,
                NotifySeverity::Warning,
                SOURCE,
                format!("syncthing:conflict:{stack}:{folder}:{}", now_ms()),
                format!("Conflict copies in {label}"),
            );
            ev.body = format!("New conflict copies in {label}: {count} ({list}). Keep the right version and delete the copies; the Syncthing page lists them.");
            field(field(field(field(code(ev, "syncthing.conflict"), "stack", stack), "folder", label), "count", count), "files", list)
        }
        Alert::FolderError { folder, label, error } => {
            let mut ev = firing(cat, NotifySeverity::Error, SOURCE, format!("syncthing:folder:{stack}:{folder}"), format!("Syncthing folder {label} is in error"));
            ev.body = format!("{error}. Nothing is synchronized in this folder until it is fixed.");
            field(field(field(code(ev, "syncthing.folder_error"), "stack", stack), "folder", label), "error", error)
        }
        Alert::FolderRecovered { folder, label } => {
            let ev = resolved(cat, SOURCE, format!("syncthing:folder:{stack}:{folder}"), format!("Syncthing folder {label} works again"));
            field(field(code(ev, "syncthing.folder_recovered"), "stack", stack), "folder", label)
        }
        Alert::DeviceUnseen { device, name, days } => {
            let mut ev = firing(
                cat,
                NotifySeverity::Warning,
                SOURCE,
                format!("syncthing:device:{stack}:{device}"),
                format!("Device {name} has not connected for {days} days"),
            );
            ev.body = format!("Syncthing has not seen {name} for {days} days: its changes do not reach the NAS.");
            field(field(field(code(ev, "syncthing.device_unseen"), "stack", stack), "device", name), "days", days)
        }
        Alert::DevicesUnseen { names, days } => {
            let (count, list) = (names.len(), names.join(", "));
            let mut ev = oneshot(
                cat,
                NotifySeverity::Warning,
                SOURCE,
                format!("syncthing:devices:{stack}"),
                format!("{count} devices have not connected for {days} days or more"),
            );
            ev.body = format!("Syncthing has not seen these devices for {days} days or more: {list}. Their changes do not reach the NAS.");
            field(field(field(field(code(ev, "syncthing.devices_unseen"), "stack", stack), "count", count), "devices", list), "days", days)
        }
        Alert::DeviceSeen { device, name } => {
            let ev = resolved(cat, SOURCE, format!("syncthing:device:{stack}:{device}"), format!("Device {name} connected again"));
            quietly(field(field(code(ev, "syncthing.device_seen"), "stack", stack), "device", name))
        }
        Alert::Down { minutes } => {
            let mut ev = firing(cat, NotifySeverity::Error, SOURCE, format!("syncthing:down:{stack}"), "Syncthing does not answer");
            ev.body = format!(
                "The app {stack} runs but its interface has not answered for {minutes} minutes: folders are not synchronized and rynas cannot see their state."
            );
            field(field(code(ev, "syncthing.down"), "stack", stack), "minutes", minutes)
        }
        Alert::Up { quiet } => {
            let ev = field(code(resolved(cat, SOURCE, format!("syncthing:down:{stack}"), "Syncthing answers again"), "syncthing.up"), "stack", stack);
            if quiet { quietly(ev) } else { ev }
        }
    }
}

/// Starts detection (Docker change hints, every minute) and the alerts.
pub fn spawn(live: Arc<Live>, apps: Arc<AppsSvc>) {
    let l = live.clone();
    tokio::spawn(async move {
        let mut hints = apps.docker.subscribe();
        loop {
            l.detect(&apps).await;
            tokio::select! {
                _ = tokio::time::sleep(DETECT_EVERY) => {}
                _ = l.wake.notified() => {}
                r = hints.recv() => {
                    if matches!(r, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        hints = apps.docker.subscribe();
                    }
                    // a compose up sends several hints
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    while hints.try_recv().is_ok() {}
                }
            }
        }
    });
    tokio::spawn(async move {
        let mut t = tokio::time::interval(ALERTS_EVERY);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            live.alerts_now(now_ms());
        }
    });
}

/// Before the lock of an encrypted share (crypt.rs): see [`Live::pause_inside`].
pub async fn before_lock(share_path: &str) {
    if let Some(l) = get() {
        l.pause_inside(share_path).await;
    }
}

/// After the lock of an encrypted share, whether it succeeded or not (crypt.rs): see
/// [`Live::lock_finished`].
pub fn lock_finished(share_path: &str, locked: bool) {
    if let Some(l) = get() {
        l.lock_finished(share_path, locked);
    }
}

/// After an encrypted share is unlocked (by hand or at start; callable from any thread): the
/// folders rynas paused are resumed in the background ([`Live::resume_inside`]).
pub fn after_unlock(share_path: &str) {
    if let Some(l) = get() {
        let (me, p) = (l.clone(), share_path.to_string());
        l.handle.spawn(async move { me.resume_inside(&p).await });
    }
}

// ---------------------------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub(super) struct StackQuery {
    stack: Option<String>,
}

fn unavailable(msg: impl Into<String>) -> Error {
    Error::new(StatusCode::SERVICE_UNAVAILABLE, rynas_api_types::error_codes::BACKEND_UNAVAILABLE, msg.into())
}

fn live() -> Res<Arc<Live>> {
    get().ok_or_else(|| unavailable("the Syncthing integration is not running in this rynasd"))
}

/// `GET /v1/syncthing[?stack=]`.
pub(super) async fn state_get(State(st): State<AppsState>, hd: HeaderMap, Query(q): Query<StackQuery>) -> Res<Json<SyncthingState>> {
    let a = super::svc(&st)?;
    super::admin(&a, &hd)?;
    let l = live()?;
    let v = tokio::task::spawn_blocking(move || l.state(q.stack.as_deref())).await.map_err(Error::internal)?;
    Ok(Json(v))
}

/// `POST /v1/syncthing/folders/{id}/pause|resume|rescan[?stack=]`.
pub(super) async fn folder_post(
    State(st): State<AppsState>,
    hd: HeaderMap,
    UrlPath((id, action)): UrlPath<(String, String)>,
    Query(q): Query<StackQuery>,
) -> Res<StatusCode> {
    let a = super::svc(&st)?;
    let who = super::admin(&a, &hd)?;
    if !matches!(action.as_str(), "pause" | "resume" | "rescan") {
        return Err(Error::not_found(format!("no action {action}")));
    }
    let l = live()?;
    let stack = match q.stack {
        Some(s) => s,
        None => lock(&l.detected).first().map(|d| d.installed.stack.clone()).ok_or_else(|| Error::not_found("Syncthing is not installed"))?,
    };
    let inst = l.instance(&stack).ok_or_else(|| crate::docker::conflict(format!("Syncthing ({stack}) is not running")))?;
    let folder = inst.with(|i| i.model.folder(&id).cloned()).ok_or_else(|| Error::not_found(format!("no Syncthing folder {id}")))?;
    let fail = |e: String| unavailable(e);
    match action.as_str() {
        "pause" => {
            inst.client.set_paused(&id, true).await.map_err(fail)?;
            inst.with(|i| set_paused(&mut i.model, &id, true));
        }
        "resume" => {
            let locked = match inst.host(&folder.path) {
                Some(h) => l.encrypted().await.iter().any(|(p, locked)| *locked && config::is_within(&h, p)),
                None => false,
            };
            if locked {
                return Err(crate::docker::conflict(format!("the encrypted share of {} is locked: unlock it first", folder.label)));
            }
            inst.client.set_paused(&id, false).await.map_err(fail)?;
            l.flag(&stack, &id, false);
            inst.with(|i| set_paused(&mut i.model, &id, false));
        }
        _ => inst.client.scan(&id).await.map_err(fail)?,
    }
    super::audit(&who, &format!("folder {action}"), &format!("{stack}/{id}"));
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/syncthing/settings`.
pub(super) async fn settings_get(State(st): State<AppsState>, hd: HeaderMap) -> Res<Json<SyncthingSettings>> {
    let a = super::svc(&st)?;
    super::admin(&a, &hd)?;
    Ok(Json(live()?.settings()))
}

/// `PUT /v1/syncthing/settings`.
pub(super) async fn settings_put(
    State(st): State<AppsState>,
    hd: HeaderMap,
    body: Result<Json<SyncthingSettings>, axum::extract::rejection::JsonRejection>,
) -> Res<Json<SyncthingSettings>> {
    let a = super::svc(&st)?;
    let who = super::admin(&a, &hd)?;
    let Json(s) = body.map_err(|e| Error::bad_request(e.body_text()))?;
    if s.device_unseen_days > MAX_UNSEEN_DAYS {
        return Err(Error::invalid("device_unseen_days", format!("at most {MAX_UNSEEN_DAYS} days (0: off)")));
    }
    let l = live()?;
    let save = l.clone();
    let s2 = s.clone();
    tokio::task::spawn_blocking(move || save.set_settings(s2)).await.map_err(Error::internal)?;
    super::audit(&who, "settings", &format!("unseen {} days, conflicts {}", s.device_unseen_days, s.conflicts_alert));
    Ok(Json(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::{get, patch, post};
    use serde_json::json;

    /// A fake Syncthing REST interface: folders in memory, every call logged.
    #[derive(Default)]
    struct Fake {
        folders: Vec<Value>,
        log: Vec<String>,
        /// db/status answers `scanning` this many times for a paused folder first.
        busy_polls: u32,
    }

    type FakeSt = Arc<Mutex<Fake>>;

    fn key_ok(hd: &HeaderMap) -> bool {
        hd.get("X-API-Key").and_then(|v| v.to_str().ok()) == Some("k3y")
    }

    async fn serve(fake: FakeSt) -> String {
        async fn folders(State(f): State<FakeSt>, hd: HeaderMap) -> Result<Json<Value>, StatusCode> {
            if !key_ok(&hd) {
                return Err(StatusCode::FORBIDDEN);
            }
            Ok(Json(Value::Array(f.lock().unwrap().folders.clone())))
        }
        async fn patch_folder(State(f): State<FakeSt>, hd: HeaderMap, UrlPath(id): UrlPath<String>, Json(b): Json<Value>) -> StatusCode {
            if !key_ok(&hd) {
                return StatusCode::FORBIDDEN;
            }
            let mut g = f.lock().unwrap();
            let p = b["paused"].as_bool().unwrap();
            g.log.push(format!("{} {id}", if p { "pause" } else { "resume" }));
            for x in &mut g.folders {
                if x["id"] == id {
                    x["paused"] = p.into();
                }
            }
            StatusCode::OK
        }
        async fn status(State(f): State<FakeSt>, Query(q): Query<BTreeMap<String, String>>) -> Json<Value> {
            let mut g = f.lock().unwrap();
            let id = q.get("folder").cloned().unwrap_or_default();
            g.log.push(format!("status {id}"));
            let paused = g.folders.iter().any(|x| x["id"] == id && x["paused"] == true);
            let state = if paused && g.busy_polls > 0 {
                g.busy_polls -= 1;
                "scanning"
            } else if paused {
                "paused"
            } else {
                "idle"
            };
            Json(json!({ "state": state }))
        }
        async fn scan(State(f): State<FakeSt>, Query(q): Query<BTreeMap<String, String>>) -> StatusCode {
            f.lock().unwrap().log.push(format!("scan {}", q.get("folder").cloned().unwrap_or_default()));
            StatusCode::OK
        }
        let app = Router::new()
            .route("/rest/config/folders", get(folders))
            .route("/rest/config/folders/{id}", patch(patch_folder))
            .route("/rest/db/status", get(status))
            .route("/rest/db/scan", post(scan))
            .with_state(fake);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    struct Facts(Mutex<Vec<(String, bool)>>);

    impl ShareFacts for Facts {
        fn encrypted(&self) -> Vec<(String, bool)> {
            self.0.lock().unwrap().clone()
        }
        fn snapshot_scheduled(&self, host_path: &str) -> bool {
            host_path == "/vol1/docs"
        }
    }

    fn folder(id: &str, path: &str, paused: bool) -> Value {
        json!({"id": id, "label": id.to_uppercase(), "path": path, "type": "sendreceive", "paused": paused, "devices": []})
    }

    fn marker(root: &Path, host: &str, id: &str) {
        let d = root.join(host.trim_start_matches('/')).join(".stfolder");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(config::marker_file_name(id)), "").unwrap();
    }

    #[test]
    fn binds_from_env() {
        let env = "WEB_PORT=8384\nSYNC_SHARES=docs=/vol1/docs!;photos=/vol2/photos\n";
        let b = binds_of_env("/vol1/docker/stacks/st", env);
        assert_eq!(b.len(), 2, "{b:?}");
        assert_eq!(host_of(&b, "/shares/docs/work/a"), Some(("/vol1/docs/work/a".into(), Some("docs".into()), "work/a".into())));
        assert_eq!(host_of(&b, "/shares/photos"), Some(("/vol2/photos".into(), Some("photos".into()), String::new())));
        assert_eq!(host_of(&b, "/shares/docsx"), None);
        assert_eq!(host_of(&b, "/elsewhere"), None);
    }

    #[tokio::test]
    async fn lock_pauses_and_unlock_resumes_only_what_rynas_paused() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let fake: FakeSt = Arc::new(Mutex::new(Fake {
            folders: vec![
                folder("work", "/shares/secret/work", false),
                folder("mine", "/shares/secret/paused-by-me", true),
                folder("gone", "/shares/secret/no-marker", false),
                folder("other", "/shares/docs", false),
            ],
            busy_polls: 2,
            ..Default::default()
        }));
        let base = serve(fake.clone()).await;
        let facts = Arc::new(Facts(Mutex::new(vec![("/vol1/secret".into(), false)])));
        let notes = Arc::new(rynas_notify::MemoryNotifier::default());
        let live = Live::new(root.path(), state.path(), facts.clone(), notes.clone());
        let binds = vec![
            Bind { container: "/shares/secret".into(), host: "/vol1/secret".into(), share: Some("secret".into()) },
            Bind { container: "/shares/docs".into(), host: "/vol1/docs".into(), share: Some("docs".into()) },
        ];
        let inst = Instance::new("sync", Client::new(&base, "k3y").unwrap(), binds);
        live.add_instance(inst, true);
        // lock: the share's running folders paused, the user's left alone, idle awaited
        live.pause_inside("/vol1/secret").await;
        facts.0.lock().unwrap()[0].1 = true;
        live.lock_finished("/vol1/secret", true);
        let log = fake.lock().unwrap().log.clone();
        let mut pauses: Vec<&String> = log.iter().filter(|l| l.starts_with("pause")).collect();
        pauses.sort();
        assert_eq!(pauses, ["pause gone", "pause work"]);
        assert!(log.iter().filter(|l| l.starts_with("status")).count() >= 3, "{log:?}");
        let pos_last_pause = log.iter().rposition(|l| l.starts_with("pause")).unwrap();
        let pos_first_status = log.iter().position(|l| l.starts_with("status")).unwrap();
        assert!(pos_last_pause < pos_first_status, "{log:?}");
        // remembered on disk, survives a new Live
        let p = live.persisted();
        assert_eq!(p.paused_by_rynas["sync"], BTreeSet::from(["work".to_string(), "gone".to_string()]));
        let again = Live::new(root.path(), state.path(), facts.clone(), notes.clone());
        assert_eq!(again.persisted(), p);
        let mode = std::fs::metadata(state.path().join("syncthing/state.json")).unwrap();
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777, 0o600);
        // unlock: markers back for `work` only
        facts.0.lock().unwrap()[0].1 = false;
        marker(root.path(), "/vol1/secret/work", "work");
        marker(root.path(), "/vol1/secret/paused-by-me", "mine");
        fake.lock().unwrap().log.clear();
        live.resume_inside("/vol1/secret").await;
        let log = fake.lock().unwrap().log.clone();
        assert_eq!(log.iter().filter(|l| l.starts_with("resume")).collect::<Vec<_>>(), ["resume work"]);
        assert_eq!(live.persisted().paused_by_rynas["sync"], BTreeSet::from(["gone".to_string()]));
        let ev = notes.take();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].code, "syncthing.folder_kept_paused");
        assert_eq!(ev[0].fields["share"], "secret");
        // the user's folder stays paused; the page shows who paused what
        let s = live.state(None);
        assert_eq!(s.installed.as_ref().unwrap().stack, "sync");
        assert_eq!(s.shares, vec![
            SyncthingShareNote { share: "secret".into(), snapshots_scheduled: false },
            SyncthingShareNote { share: "docs".into(), snapshots_scheduled: true },
        ]);
    }

    #[tokio::test]
    async fn reconcile_pauses_folders_of_locked_shares_at_start() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let fake: FakeSt = Arc::new(Mutex::new(Fake {
            folders: vec![folder("work", "/shares/secret/work", false), folder("other", "/shares/docs", false)],
            ..Default::default()
        }));
        let base = serve(fake.clone()).await;
        let facts = Arc::new(Facts(Mutex::new(vec![("/vol1/secret".into(), true)])));
        let live = Live::new(root.path(), state.path(), facts.clone(), Arc::new(rynas_notify::NullNotifier));
        let binds = vec![
            Bind { container: "/shares/secret".into(), host: "/vol1/secret".into(), share: Some("secret".into()) },
            Bind { container: "/shares/docs".into(), host: "/vol1/docs".into(), share: Some("docs".into()) },
        ];
        let inst = Instance::new("sync", Client::new(&base, "k3y").unwrap(), binds);
        live.add_instance(inst.clone(), true);
        let v = inst.client.get("/rest/config/folders").await.unwrap();
        inst.with(|i| i.model.replace_config(model::folders_of_config(&v), Vec::new()));
        live.reconcile(&inst).await;
        assert_eq!(fake.lock().unwrap().log, ["pause work"]);
        assert!(live.persisted().paused_by_rynas["sync"].contains("work"));
        // unlocked (the boot unlock ran while Syncthing was not watched): resumed at the next read
        facts.0.lock().unwrap()[0].1 = false;
        marker(root.path(), "/vol1/secret/work", "work");
        let v = inst.client.get("/rest/config/folders").await.unwrap();
        inst.with(|i| i.model.replace_config(model::folders_of_config(&v), Vec::new()));
        live.reconcile(&inst).await;
        assert_eq!(fake.lock().unwrap().log, ["pause work", "resume work"]);
        assert!(live.persisted().paused_by_rynas.is_empty());
        // a folder rynas paused that the user resumed meanwhile is forgotten, never touched
        live.flag("sync", "other", true);
        live.reconcile(&inst).await;
        assert!(live.persisted().paused_by_rynas.is_empty());
        assert_eq!(fake.lock().unwrap().log.len(), 2);
        let s = live.state(None);
        let work = s.folders.iter().find(|f| f.id == "work").unwrap();
        assert_eq!((work.share.as_deref(), work.sub_path.as_deref(), work.locked), (Some("secret"), Some("work"), false));
    }

    #[tokio::test]
    async fn a_lock_in_progress_is_locked_for_the_event_loop_and_a_failed_one_resumes() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let fake: FakeSt = Arc::new(Mutex::new(Fake { folders: vec![folder("work", "/shares/secret/work", false)], ..Default::default() }));
        let base = serve(fake.clone()).await;
        // the share is still mounted while the lock runs
        let facts = Arc::new(Facts(Mutex::new(vec![("/vol1/secret".into(), false)])));
        let live = Live::new(root.path(), state.path(), facts, Arc::new(rynas_notify::NullNotifier));
        let binds = vec![Bind { container: "/shares/secret".into(), host: "/vol1/secret".into(), share: Some("secret".into()) }];
        let inst = Instance::new("sync", Client::new(&base, "k3y").unwrap(), binds);
        live.add_instance(inst.clone(), true);
        marker(root.path(), "/vol1/secret/work", "work");
        live.pause_inside("/vol1/secret").await;
        // a full read during the lock (a ConfigSaved of the pause) never resumes it
        let v = inst.client.get("/rest/config/folders").await.unwrap();
        inst.with(|i| i.model.replace_config(model::folders_of_config(&v), Vec::new()));
        live.reconcile(&inst).await;
        assert_eq!(fake.lock().unwrap().log.iter().filter(|l| !l.starts_with("status")).collect::<Vec<_>>(), ["pause work"]);
        assert!(live.persisted().paused_by_rynas["sync"].contains("work"));
        // the lock failed: resumed in the background
        live.lock_finished("/vol1/secret", false);
        for _ in 0..50 {
            if fake.lock().unwrap().log.iter().any(|l| l == "resume work") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(fake.lock().unwrap().log.iter().any(|l| l == "resume work"));
        assert!(live.persisted().paused_by_rynas.is_empty());
    }

    #[tokio::test]
    async fn unreachable_instance_never_blocks_the_lock() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let facts = Arc::new(Facts(Mutex::new(Vec::new())));
        let live = Live::new(root.path(), state.path(), facts, Arc::new(rynas_notify::NullNotifier));
        // nothing listens on port 9 of localhost
        let inst = Instance::new("sync", Client::new("http://127.0.0.1:9", "k").unwrap(), Vec::new());
        live.add_instance(inst, true);
        let t = std::time::Instant::now();
        live.pause_inside("/vol1/secret").await;
        assert!(t.elapsed() < Duration::from_secs(10));
        assert!(live.persisted().paused_by_rynas.is_empty());
    }

    #[test]
    fn alert_events() {
        let ev = event_of("sync", Alert::Conflict { folder: "d".into(), label: "Docs".into(), count: 3, files: vec!["a".into(), "b".into()] });
        assert_eq!((ev.code.as_str(), ev.fields["files"].as_str(), ev.fields["count"].as_str()), ("syncthing.conflict", "a, b", "3"));
        let ev = event_of("sync", Alert::Up { quiet: true });
        assert!(ev.quiet);
        assert_eq!(ev.code, "syncthing.up");
        let ev = event_of("sync", Alert::DeviceUnseen { device: "ABC".into(), name: "phone".into(), days: 8 });
        assert_eq!(ev.key.as_deref(), Some("syncthing:device:sync:ABC"));
    }
}
