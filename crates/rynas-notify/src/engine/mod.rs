//! The notification engine (rynasd only).
//!
//! Flow: [`Notifier::emit`] queues an event (bounded channel) → the ingest loop runs the
//! state machine in one SQLite transaction ([`Engine::ingest`]) → matching rules put rows
//! in the persistent outbox (`next_ms` = now, the end of a grouping window or of quiet
//! hours) → the delivery loop sends due rows, grouped per target and group key, rate
//! limited per target, retried with exponential backoff, and drops them after
//! [`EngineOptions::max_attempts`]. The outbox survives restarts.
//!
//! State per key: a `firing` event opens the condition and is delivered; repeats while it
//! is open only bump `count`/`last_ms` (a higher severity is an escalation and is
//! delivered); `resolved` closes it (delivered, ignored when nothing was firing);
//! reminders go out every `repeat_interval_s` of the matching rules while it stays firing
//! and unacknowledged. `oneshot` events with the same key within
//! [`EngineOptions::oneshot_dedup`] are counted, not delivered again. Silenced keys are
//! stored but not delivered.
//!
//! Caps (M1): 5000 events / 90 days (open conditions kept), 10 000 keys (closed ones evicted
//! first, after 30 days), 2000 outbox rows, 500 silences, 100 targets, 200 rules, 120
//! events per minute per source (the excess is dropped and logged).

pub mod channels;
mod db;
pub mod matcher;
pub mod secrets;
pub mod template;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use matcher::{Compiled, quiet_until};
pub use matcher::{LocalTime, local_time, utc_time};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use rynas_api_types::notify::{
    EventAction, EventState, NewEvent, Notification, NotificationChangeKind, NotificationEvent,
    NotifyCategory, NotifyLanguage, NotifyRule, NotifySeverity, NotifyTarget, Silence,
    SilenceRequest, TargetConfig, TargetRequest, TargetStatus, TargetTestResult,
};
use secrets::SecretBox;
use template::{Message, Templates};
use tokio::sync::{Notify, broadcast, mpsc};

use crate::Notifier;

/// Milliseconds since the Unix epoch.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    })
}

pub struct EngineOptions {
    /// `<state>/notify`: `notify.db` and `secret.key`.
    pub dir: PathBuf,
    /// Shown in subjects ("[nas] ...").
    pub hostname: String,
    pub clock: Clock,
    pub local: fn(u64) -> LocalTime,
    pub oneshot_dedup: Duration,
    /// Messages per minute per target (bursts up to the same number).
    pub per_target_per_minute: u32,
    pub max_attempts: u32,
    pub retry_base: Duration,
    pub retry_max: Duration,
    /// How often reminders and housekeeping run.
    pub tick: Duration,
    /// Settings of other tools, each imported into targets and a rule the first time.
    pub imports: Vec<crate::ImportedSettings>,
    /// Events waiting for ingestion (then [`Notifier::emit`] drops them).
    pub queue: usize,
}

impl EngineOptions {
    pub fn new(dir: impl Into<PathBuf>, hostname: &str) -> Self {
        Self {
            dir: dir.into(),
            hostname: hostname.into(),
            clock: system_clock(),
            local: local_time,
            oneshot_dedup: Duration::from_secs(600),
            per_target_per_minute: 10,
            max_attempts: 8,
            retry_base: Duration::from_secs(30),
            retry_max: Duration::from_secs(3600),
            tick: Duration::from_secs(60),
            imports: Vec::new(),
            queue: 256,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum NotifyError {
    NotFound(String),
    Invalid(String),
    /// A name that already exists.
    Exists(String),
    /// The object is in use (a target named by rules).
    Conflict(String),
    Storage(String),
}

impl std::fmt::Display for NotifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) => write!(f, "{m} not found"),
            Self::Invalid(m) | Self::Exists(m) | Self::Conflict(m) => f.write_str(m),
            Self::Storage(m) => write!(f, "notification store: {m}"),
        }
    }
}

impl std::error::Error for NotifyError {}

impl From<rusqlite::Error> for NotifyError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Storage(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, NotifyError>;

/// Filters of `GET /notifications` (guidelines A17: newest first, `before` = id cursor).
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub limit: usize,
    pub before: Option<u64>,
    pub min_severity: Option<NotifySeverity>,
    pub category: Option<NotifyCategory>,
    pub state: Option<EventState>,
    pub acked: Option<bool>,
    pub key: Option<String>,
    pub source: Option<String>,
}

const MAX_EVENTS: i64 = 5000;
const EVENT_DAYS: u64 = 90;
const MAX_KEYS: i64 = 10_000;
const MAX_OUTBOX: i64 = 2000;
const MAX_SILENCES: i64 = 500;
const MAX_TARGETS: i64 = 100;
const MAX_RULES: i64 = 200;
const PER_SOURCE_PER_MINUTE: u32 = 120;
/// Rows of one target sent in one message at most.
const GROUP_MAX: usize = 50;
/// A target out of tokens retries this much later (not counted as an attempt).
const RATE_DELAY_MS: u64 = 30_000;
const DAY_MS: u64 = 86_400_000;

#[derive(Clone)]
pub struct Engine(Arc<Inner>);

struct Inner {
    o: EngineOptions,
    db: Mutex<Connection>,
    secrets: SecretBox,
    tpl: Templates,
    /// Built on the first HTTP delivery: OpenSSL and the CA store cost ~3 MB, which a
    /// system with only the web interface target never needs.
    http: std::sync::OnceLock<reqwest::Client>,
    rules: Mutex<Arc<Vec<Compiled>>>,
    changes: broadcast::Sender<Arc<str>>,
    wake: Notify,
    tx: mpsc::Sender<NewEvent>,
    rx: Mutex<Option<mpsc::Receiver<NewEvent>>>,
    /// Token bucket per target id: (tokens, last refill ms). Bounded by the targets.
    buckets: Mutex<HashMap<u64, (f64, u64)>>,
    /// Events per source in the current minute: (minute, count). At most 256 sources.
    sources: Mutex<HashMap<String, (u64, u32)>>,
    last_purge: Mutex<u64>,
}

impl Notifier for Engine {
    fn emit(&self, event: NewEvent) {
        if let Err(e) = self.0.tx.try_send(event) {
            let ev = match &e {
                mpsc::error::TrySendError::Full(ev) | mpsc::error::TrySendError::Closed(ev) => ev,
            };
            tracing::warn!(
                "notification queue full, dropped: {} ({})",
                ev.title,
                ev.source
            );
        }
    }
}

// ------------------------------------------------------------------------- sanitizing

fn clean_line(s: &str, max: usize) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    s.trim().chars().take(max).collect()
}

fn clean_text(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .take(max)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn safe_url(u: &str) -> bool {
    (u.starts_with('/') && !u.starts_with("//"))
        || u.starts_with("https://")
        || u.starts_with("http://")
}

/// Bounds and cleans what a source sent; refuses events without title, source or a key
/// where one is required.
pub fn sanitize(ev: NewEvent) -> Result<NewEvent> {
    let source = clean_line(&ev.source, 64);
    let title = clean_line(&ev.title, 200);
    if source.is_empty() || title.is_empty() {
        return Err(NotifyError::Invalid(
            "an event needs a source and a title".into(),
        ));
    }
    let key = ev
        .key
        .as_deref()
        .map(|k| clean_line(k, 200))
        .filter(|k| !k.is_empty());
    let key = match (key, ev.state) {
        (Some(k), _) => k,
        (None, EventState::Oneshot) => format!("{source}:{title}").chars().take(200).collect(),
        (None, _) => {
            return Err(NotifyError::Invalid(
                "firing and resolved events need a key".into(),
            ));
        }
    };
    let fields = ev
        .fields
        .iter()
        .take(32)
        .map(|(k, v)| (clean_line(k, 64), clean_line(v, 512)))
        .filter(|(k, _)| !k.is_empty())
        .collect();
    let actions = ev
        .actions
        .iter()
        .take(4)
        .map(|a| EventAction {
            label: clean_line(&a.label, 64),
            url: clean_line(&a.url, 512),
        })
        .filter(|a| !a.label.is_empty() && safe_url(&a.url))
        .collect();
    Ok(NewEvent {
        source,
        category: ev.category,
        severity: ev.severity,
        key: Some(key),
        state: ev.state,
        title,
        body: clean_text(&ev.body, 4000),
        fields,
        actions,
        // a malformed code is dropped (the title/body fallback still works), not refused
        code: if code_ok(&ev.code) {
            ev.code
        } else {
            String::new()
        },
        body_format: ev.body_format,
        quiet: ev.quiet,
    })
}

/// Event codes: dot-separated words of `[a-z0-9_]`, at most 64 characters (`hw.fan_stopped`).
pub fn code_ok(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 64
        && c.split('.').all(|w| {
            !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

fn secret_name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 32
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn http_url_ok(u: &str) -> bool {
    (u.starts_with("https://") || u.starts_with("http://"))
        && reqwest::Url::parse(u).is_ok_and(|u| u.host_str().is_some())
}

/// An enabled target: language, configuration, secrets.
type LoadedTarget = (NotifyLanguage, TargetConfig, BTreeMap<String, String>);

/// A group of outbox rows sent as one message.
struct Group {
    target_id: u64,
    language: NotifyLanguage,
    config: TargetConfig,
    secrets: BTreeMap<String, String>,
    rows: Vec<(u64, u32)>,
    events: Vec<(Notification, bool)>,
}

impl Engine {
    /// Opens (or creates) `<dir>/notify.db` and the secret key.
    pub fn open(o: EngineOptions) -> std::io::Result<Engine> {
        std::fs::create_dir_all(&o.dir)?;
        std::fs::set_permissions(&o.dir, std::fs::Permissions::from_mode(0o700))?;
        let secrets = SecretBox::open(&o.dir)?;
        let conn = db::open(&o.dir.join("notify.db")).map_err(std::io::Error::other)?;
        let (tx, rx) = mpsc::channel(o.queue.max(1));
        let (changes, _) = broadcast::channel(16);
        let e = Engine(Arc::new(Inner {
            tpl: Templates::new(&o.hostname),
            o,
            db: Mutex::new(conn),
            secrets,
            http: std::sync::OnceLock::new(),
            rules: Mutex::new(Arc::new(Vec::new())),
            changes,
            wake: Notify::new(),
            tx,
            rx: Mutex::new(Some(rx)),
            buckets: Mutex::new(HashMap::new()),
            sources: Mutex::new(HashMap::new()),
            last_purge: Mutex::new(0),
        }));
        e.reload_rules().map_err(std::io::Error::other)?;
        for imp in e.0.o.imports.clone() {
            match e.import_once(&imp) {
                Ok(0) => {}
                Ok(n) => tracing::info!("imported {n} notification targets ({})", imp.rule_name),
                Err(err) => tracing::warn!("{}: {err}", imp.rule_name),
            }
        }
        Ok(e)
    }

    fn http(&self) -> &reqwest::Client {
        self.0.http.get_or_init(channels::http_client)
    }

    fn now(&self) -> u64 {
        (self.0.o.clock)()
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.db.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Runs `f` on the blocking pool (SQLite).
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Engine) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let e = self.clone();
        tokio::task::spawn_blocking(move || f(&e))
            .await
            .map_err(|e| NotifyError::Storage(e.to_string()))?
    }

    /// Serialized [`NotificationEvent`]s (once per change, for every subscriber).
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.0.changes.subscribe()
    }

    fn publish(&self, c: &Connection, kind: NotificationChangeKind, n: Option<Notification>) {
        if self.0.changes.receiver_count() == 0 {
            return;
        }
        let unacked = unacked(c).unwrap_or(0);
        let ev = NotificationEvent {
            kind,
            notification: n,
            unacked,
        };
        if let Ok(j) = serde_json::to_string(&ev) {
            let _ = self.0.changes.send(j.into());
        }
    }

    fn reload_rules(&self) -> Result<()> {
        let docs: Vec<(i64, String)> = {
            let c = self.conn();
            let mut st = c.prepare("SELECT id, doc FROM rules ORDER BY id")?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut out = Vec::new();
        for (id, doc) in docs {
            let Ok(mut r) = serde_json::from_str::<NotifyRule>(&doc) else {
                tracing::error!("notification rule {id}: unreadable");
                continue;
            };
            r.id = id as u64;
            match Compiled::new(r) {
                Ok(c) => out.push(c),
                Err(e) => tracing::error!("notification rule {id}: {e}"),
            }
        }
        *self.0.rules.lock().unwrap() = Arc::new(out);
        Ok(())
    }

    fn rules_snapshot(&self) -> Arc<Vec<Compiled>> {
        self.0.rules.lock().unwrap().clone()
    }

    // --------------------------------------------------------------------- ingestion

    fn allow_source(&self, source: &str, now: u64) -> bool {
        let minute = now / 60_000;
        let mut m = self.0.sources.lock().unwrap();
        if m.len() >= 256 && !m.contains_key(source) {
            m.clear();
        }
        let e = m.entry(source.to_string()).or_insert((minute, 0));
        if e.0 != minute {
            *e = (minute, 0);
        }
        e.1 += 1;
        if e.1 == PER_SOURCE_PER_MINUTE + 1 {
            tracing::warn!(
                "notifications from {source}: more than {PER_SOURCE_PER_MINUTE} per minute, dropping the rest of this minute"
            );
        }
        e.1 <= PER_SOURCE_PER_MINUTE
    }

    /// Stores an event and queues its deliveries. Returns the stored notification (`None`
    /// when it changed nothing: a resolution of a key that was not firing, a dropped
    /// event). `force` bypasses the oneshot dedup window.
    pub fn ingest(&self, ev: NewEvent, force: bool) -> Result<Option<Notification>> {
        let ev = sanitize(ev)?;
        let now = self.now();
        if !self.allow_source(&ev.source, now) {
            return Ok(None);
        }
        let key = ev.key.clone().unwrap_or_default();
        let mut c = self.conn();
        let tx = c.transaction()?;
        let silenced = is_silenced(&tx, &key, now)?;
        let mut changes: Vec<(NotificationChangeKind, u64)> = Vec::new();
        let mut deliver: Vec<u64> = Vec::new();
        let state: Option<(bool, String, i64)> = tx
            .query_row(
                "SELECT firing, severity, event_id FROM keys WHERE key = ?1",
                [&key],
                |r| Ok((r.get::<_, i64>(0)? != 0, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let open = match &state {
            Some((true, sev, eid)) => db::get_event(&tx, *eid as u64)?
                .map(|n| (NotifySeverity::parse(sev).unwrap_or_default(), n)),
            _ => None,
        };
        let stored: Option<u64> = match ev.state {
            EventState::Firing => match open {
                Some((sev, n)) => {
                    tx.execute(
                        "UPDATE events SET count = count + 1, last_ms = ?2 WHERE id = ?1",
                        params![n.id as i64, now as i64],
                    )?;
                    if ev.severity != sev {
                        tx.execute(
                            "UPDATE events SET severity = ?2, title = ?3, body = ?4, fields = ?5,
                               code = ?6, body_format = ?7 WHERE id = ?1",
                            params![
                                n.id as i64,
                                ev.severity.as_str(),
                                ev.title,
                                ev.body,
                                serde_json::to_string(&ev.fields).unwrap_or_default(),
                                ev.code,
                                ev.body_format.as_str(),
                            ],
                        )?;
                        tx.execute(
                            "UPDATE keys SET severity = ?2 WHERE key = ?1",
                            params![key, ev.severity.as_str()],
                        )?;
                        // an escalation is news; a lower level is not
                        if ev.severity > sev && !n.silenced && !silenced {
                            deliver.push(n.id);
                        }
                    }
                    changes.push((NotificationChangeKind::Updated, n.id));
                    Some(n.id)
                }
                None => {
                    let id = insert_event(&tx, &ev, now, silenced, ev.severity)?;
                    tx.execute(
                        "INSERT INTO keys (key, firing, severity, event_id, since_ms, reminders)
                         VALUES (?1, 1, ?2, ?3, ?4, '{}')
                         ON CONFLICT(key) DO UPDATE SET firing = 1, severity = excluded.severity,
                           event_id = excluded.event_id, since_ms = excluded.since_ms, reminders = '{}'",
                        params![key, ev.severity.as_str(), id as i64, now as i64],
                    )?;
                    if !silenced {
                        deliver.push(id);
                    }
                    changes.push((NotificationChangeKind::Created, id));
                    Some(id)
                }
            },
            EventState::Resolved => match open {
                None => None,
                Some((sev, n)) => {
                    let quiet = silenced || n.silenced;
                    let id = insert_event(&tx, &ev, now, quiet, sev)?;
                    tx.execute(
                        "UPDATE events SET resolved_ms = ?2 WHERE id = ?1",
                        params![n.id as i64, now as i64],
                    )?;
                    tx.execute(
                        "UPDATE keys SET firing = 0, event_id = ?2 WHERE key = ?1",
                        params![key, id as i64],
                    )?;
                    if !quiet && !ev.quiet {
                        deliver.push(id);
                    }
                    changes.push((NotificationChangeKind::Updated, n.id));
                    changes.push((NotificationChangeKind::Created, id));
                    Some(id)
                }
            },
            EventState::Oneshot => {
                let recent: Option<i64> = if force {
                    None
                } else {
                    tx.query_row(
                        "SELECT id FROM events WHERE key = ?1 AND state = 'oneshot' AND last_ms >= ?2
                         ORDER BY id DESC LIMIT 1",
                        params![key, now.saturating_sub(self.0.o.oneshot_dedup.as_millis() as u64) as i64],
                        |r| r.get(0),
                    )
                    .optional()?
                };
                match recent {
                    Some(id) => {
                        tx.execute(
                            "UPDATE events SET count = count + 1, last_ms = ?2 WHERE id = ?1",
                            params![id, now as i64],
                        )?;
                        changes.push((NotificationChangeKind::Updated, id as u64));
                        Some(id as u64)
                    }
                    None => {
                        let id = insert_event(&tx, &ev, now, silenced, ev.severity)?;
                        if !silenced {
                            deliver.push(id);
                        }
                        changes.push((NotificationChangeKind::Created, id));
                        Some(id)
                    }
                }
            }
        };
        let mut queued = 0;
        for id in deliver {
            if let Some(n) = db::get_event(&tx, id)? {
                queued += self.enqueue(&tx, &n, false, now, &|_| true)?;
            }
        }
        tx.commit()?;
        let result = match stored {
            Some(id) => db::get_event(&c, id)?,
            None => None,
        };
        for (kind, id) in changes {
            let n = db::get_event(&c, id)?;
            self.publish(&c, kind, n);
        }
        drop(c);
        if queued > 0 {
            self.0.wake.notify_one();
        }
        Ok(result)
    }

    /// Outbox rows for the targets of the rules matching `n` (each target once).
    fn enqueue(
        &self,
        tx: &Transaction,
        n: &Notification,
        reminder: bool,
        now: u64,
        only: &dyn Fn(&Compiled) -> bool,
    ) -> Result<usize> {
        let rules = self.rules_snapshot();
        let mut seen = HashSet::new();
        let mut queued = 0;
        for r in rules
            .iter()
            .filter(|r| r.rule.enabled && only(r) && r.matches(n))
        {
            let quiet = if n.severity < NotifySeverity::Critical {
                quiet_until(&r.rule.quiet_hours, now, self.0.o.local)
            } else {
                None
            };
            let group = match quiet {
                Some(_) => Some(format!("q{}", r.rule.id)),
                None if r.rule.group_window_s > 0 => Some(format!("g{}", r.rule.id)),
                None => None,
            };
            for &t in &r.rule.targets {
                if !seen.insert(t) {
                    continue;
                }
                let target: Option<(i64, String)> = tx
                    .query_row(
                        "SELECT enabled, config FROM targets WHERE id = ?1",
                        [t as i64],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let Some((1, config)) = target else { continue };
                if config.contains("\"kind\":\"ui\"") {
                    continue;
                }
                let next = match (quiet, &group) {
                    (Some(until), _) => until,
                    (None, Some(g)) => tx
                        .query_row(
                            "SELECT MIN(next_ms) FROM outbox WHERE target_id = ?1 AND group_key = ?2 AND attempts = 0",
                            params![t as i64, g],
                            |r| r.get::<_, Option<i64>>(0),
                        )?
                        .map(|v| v as u64)
                        .unwrap_or(now + r.rule.group_window_s * 1000),
                    (None, None) => now,
                };
                tx.execute(
                    "INSERT INTO outbox (target_id, event_id, reminder, group_key, next_ms, created_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![t as i64, n.id as i64, reminder as i64, group, next as i64, now as i64],
                )?;
                queued += 1;
            }
        }
        if queued > 0 {
            let dropped = tx.execute(
                "DELETE FROM outbox WHERE id <= (SELECT id FROM outbox ORDER BY id DESC LIMIT 1 OFFSET ?1)",
                [MAX_OUTBOX],
            )?;
            if dropped > 0 {
                tracing::warn!("notification outbox full: {dropped} oldest messages dropped");
            }
        }
        Ok(queued)
    }

    // ---------------------------------------------------------------------- delivery

    /// When the next outbox row is due.
    pub fn next_due(&self) -> Result<Option<u64>> {
        let c = self.conn();
        Ok(c.query_row("SELECT MIN(next_ms) FROM outbox", [], |r| {
            r.get::<_, Option<i64>>(0)
        })?
        .map(|v| v as u64))
    }

    fn due_groups(&self, now: u64) -> Result<Vec<Group>> {
        let c = self.conn();
        let rows: Vec<(i64, i64, i64, bool, Option<String>, u32)> = {
            let mut st = c.prepare_cached(
                "SELECT id, target_id, event_id, reminder, group_key, attempts FROM outbox
                 WHERE next_ms <= ?1 ORDER BY target_id, group_key, id LIMIT 500",
            )?;
            st.query_map([now as i64], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, i64>(3)? != 0,
                    r.get(4)?,
                    r.get::<_, i64>(5)? as u32,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        let mut groups: Vec<Group> = Vec::new();
        let mut index: HashMap<(i64, String), usize> = HashMap::new();
        let mut gone: Vec<i64> = Vec::new();
        let mut targets: HashMap<i64, Option<LoadedTarget>> = HashMap::new();
        for (id, tid, eid, reminder, gk, attempts) in rows {
            let target = targets
                .entry(tid)
                .or_insert_with(|| self.load_target(&c, tid).ok().flatten())
                .clone();
            let Some((language, config, secrets)) = target else {
                gone.push(id);
                continue;
            };
            let Some(ev) = db::get_event(&c, eid as u64)? else {
                gone.push(id);
                continue;
            };
            let gkey = (tid, gk.unwrap_or_else(|| format!("#{id}")));
            let slot = match index.get(&gkey) {
                Some(&i) if groups[i].rows.len() < GROUP_MAX => i,
                _ => {
                    groups.push(Group {
                        target_id: tid as u64,
                        language,
                        config,
                        secrets,
                        rows: Vec::new(),
                        events: Vec::new(),
                    });
                    index.insert(gkey, groups.len() - 1);
                    groups.len() - 1
                }
            };
            groups[slot].rows.push((id as u64, attempts));
            groups[slot].events.push((ev, reminder));
        }
        for id in gone {
            c.execute("DELETE FROM outbox WHERE id = ?1", [id])?;
        }
        Ok(groups)
    }

    /// An enabled target with its secrets.
    fn load_target(&self, c: &Connection, id: i64) -> Result<Option<LoadedTarget>> {
        let row: Option<(i64, String, String, Option<Vec<u8>>)> = c
            .query_row(
                "SELECT enabled, language, config, secrets FROM targets WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((1, lang, config, sealed)) = row else {
            return Ok(None);
        };
        let config: TargetConfig =
            serde_json::from_str(&config).map_err(|e| NotifyError::Storage(e.to_string()))?;
        let secrets = match sealed {
            Some(s) => self.0.secrets.open_sealed(&s).map_err(|e| {
                tracing::error!("secrets of notification target {id}: {e}");
                NotifyError::Storage("secrets unreadable".into())
            })?,
            None => BTreeMap::new(),
        };
        Ok(Some((parse_lang(&lang), config, secrets)))
    }

    fn take_token(&self, target: u64, now: u64) -> bool {
        let cap = f64::from(self.0.o.per_target_per_minute.max(1));
        let mut b = self.0.buckets.lock().unwrap();
        let e = b.entry(target).or_insert((cap, now));
        let refill = (now.saturating_sub(e.1)) as f64 / 60_000.0 * cap;
        e.0 = (e.0 + refill).min(cap);
        e.1 = now;
        if e.0 >= 1.0 {
            e.0 -= 1.0;
            true
        } else {
            false
        }
    }

    fn backoff_ms(&self, attempts: u32) -> u64 {
        let base = self.0.o.retry_base.as_millis() as u64;
        let max = self.0.o.retry_max.as_millis() as u64;
        base.saturating_mul(1u64 << attempts.saturating_sub(1).min(20))
            .min(max)
    }

    fn render(&self, g: &Group) -> Message {
        let ctx = g
            .events
            .iter()
            .map(|(n, rem)| self.0.tpl.event_ctx(n, *rem, g.language, self.0.o.local))
            .collect();
        self.0.tpl.render(ctx, g.language)
    }

    /// Sends what is due now. Returns the number of messages sent.
    pub async fn deliver_due(&self) -> Result<usize> {
        let now = self.now();
        let groups = self.blocking(move |e| e.due_groups(now)).await?;
        let mut sent = 0;
        for g in groups {
            let ids: Vec<u64> = g.rows.iter().map(|r| r.0).collect();
            if !self.take_token(g.target_id, now) {
                self.blocking(move |e| {
                    let c = e.conn();
                    for id in ids {
                        c.execute(
                            "UPDATE outbox SET next_ms = ?2 WHERE id = ?1",
                            params![id as i64, (now + RATE_DELAY_MS) as i64],
                        )?;
                    }
                    Ok(())
                })
                .await?;
                continue;
            }
            let msg = self.render(&g);
            let dest = channels::Dest {
                config: &g.config,
                language: g.language,
                secrets: &g.secrets,
            };
            let r = channels::send(self.http(), &self.0.tpl, &dest, &msg).await;
            if r.is_ok() {
                sent += 1;
            }
            let attempts = g.rows.iter().map(|r| r.1).max().unwrap_or(0) + 1;
            let max = self.0.o.max_attempts;
            let backoff = self.backoff_ms(attempts);
            let tid = g.target_id;
            let done = self.now();
            self.blocking(move |e| {
                let c = e.conn();
                match &r {
                    Ok(()) => {
                        for id in &ids {
                            c.execute("DELETE FROM outbox WHERE id = ?1", [*id as i64])?;
                        }
                        c.execute(
                            "UPDATE targets SET last_ok_ms = ?2 WHERE id = ?1",
                            params![tid as i64, done as i64],
                        )?;
                    }
                    Err(err) => {
                        if attempts >= max {
                            tracing::error!(
                                "notification target {tid}: {} messages dropped after {attempts} attempts: {err}",
                                ids.len()
                            );
                            for id in &ids {
                                c.execute("DELETE FROM outbox WHERE id = ?1", [*id as i64])?;
                            }
                        } else {
                            tracing::warn!("notification target {tid}: attempt {attempts} failed: {err}");
                            for id in &ids {
                                c.execute(
                                    "UPDATE outbox SET attempts = ?2, next_ms = ?3, last_error = ?4 WHERE id = ?1",
                                    params![*id as i64, attempts, (done + backoff) as i64, err],
                                )?;
                            }
                        }
                        c.execute(
                            "UPDATE targets SET last_error_ms = ?2, last_error = ?3 WHERE id = ?1",
                            params![tid as i64, done as i64, err],
                        )?;
                    }
                }
                Ok(())
            })
            .await?;
        }
        Ok(sent)
    }

    // ------------------------------------------------------------- reminders, housekeeping

    /// Queues reminders of open, unacknowledged, unsilenced conditions. Returns how many
    /// outbox rows were added.
    pub fn reminders(&self, now: u64) -> Result<usize> {
        let rules = self.rules_snapshot();
        if !rules
            .iter()
            .any(|r| r.rule.enabled && r.rule.repeat_interval_s > 0)
        {
            return Ok(0);
        }
        let mut c = self.conn();
        let tx = c.transaction()?;
        let open: Vec<(String, i64, i64, String)> = {
            let mut st = tx.prepare(
                "SELECT key, event_id, since_ms, reminders FROM keys WHERE firing = 1 LIMIT 1000",
            )?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut queued = 0;
        for (key, eid, since, rem) in open {
            let Some(n) = db::get_event(&tx, eid as u64)? else {
                continue;
            };
            if n.silenced || n.acked_ms.is_some() || is_silenced(&tx, &key, now)? {
                continue;
            }
            let mut rem: BTreeMap<String, u64> = serde_json::from_str(&rem).unwrap_or_default();
            let mut changed = false;
            for r in rules
                .iter()
                .filter(|r| r.rule.enabled && r.rule.repeat_interval_s > 0 && r.matches(&n))
            {
                let id = r.rule.id;
                let last = rem.get(&id.to_string()).copied().unwrap_or(since as u64);
                if now.saturating_sub(last) >= r.rule.repeat_interval_s * 1000 {
                    queued += self.enqueue(&tx, &n, true, now, &|x| x.rule.id == id)?;
                    rem.insert(id.to_string(), now);
                    changed = true;
                }
            }
            if changed {
                tx.execute(
                    "UPDATE keys SET reminders = ?2 WHERE key = ?1",
                    params![key, serde_json::to_string(&rem).unwrap_or_default()],
                )?;
            }
        }
        tx.commit()?;
        drop(c);
        if queued > 0 {
            self.0.wake.notify_one();
        }
        Ok(queued)
    }

    /// Retention of events, keys and silences (at most hourly).
    pub fn purge(&self, now: u64) -> Result<()> {
        {
            let mut last = self.0.last_purge.lock().unwrap();
            if now.saturating_sub(*last) < 3_600_000 {
                return Ok(());
            }
            *last = now;
        }
        let c = self.conn();
        let cutoff = now.saturating_sub(EVENT_DAYS * DAY_MS) as i64;
        c.execute(
            "DELETE FROM events WHERE id NOT IN (SELECT event_id FROM keys WHERE firing = 1)
             AND (ts_ms < ?1 OR id <= (SELECT id FROM events ORDER BY id DESC LIMIT 1 OFFSET ?2))",
            params![cutoff, MAX_EVENTS],
        )?;
        c.execute(
            "DELETE FROM keys WHERE firing = 0 AND since_ms < ?1",
            [now.saturating_sub(30 * DAY_MS) as i64],
        )?;
        c.execute(
            "DELETE FROM keys WHERE key IN (SELECT key FROM keys ORDER BY firing ASC, since_ms ASC
             LIMIT max(0, (SELECT COUNT(*) FROM keys) - ?1))",
            [MAX_KEYS],
        )?;
        c.execute(
            "DELETE FROM silences WHERE until_ms IS NOT NULL AND until_ms <= ?1",
            [now as i64],
        )?;
        Ok(())
    }

    /// Reminders and housekeeping.
    pub async fn tick(&self) {
        let now = self.now();
        if let Err(e) = self
            .blocking(move |e| {
                e.reminders(now)?;
                e.purge(now)
            })
            .await
        {
            tracing::error!("notifications: {e}");
        }
    }

    /// The ingest and delivery loops; returns when every [`Engine`] handle is gone... never
    /// in practice (rynasd keeps one).
    pub async fn run(self) {
        let Some(mut rx) = self.0.rx.lock().unwrap().take() else {
            tracing::error!("notification engine already running");
            return;
        };
        let ingest = {
            let e = self.clone();
            async move {
                while let Some(ev) = rx.recv().await {
                    let x = e.clone();
                    match tokio::task::spawn_blocking(move || x.ingest(ev, false)).await {
                        Ok(Err(err)) => tracing::warn!("notification refused: {err}"),
                        Err(err) => tracing::error!("notification ingest: {err}"),
                        Ok(Ok(_)) => {}
                    }
                }
            }
        };
        let deliver = {
            let e = self.clone();
            async move {
                let tick = e.0.o.tick.as_millis() as u64;
                let mut last_tick = 0u64;
                loop {
                    let now = e.now();
                    if now.saturating_sub(last_tick) >= tick {
                        e.tick().await;
                        last_tick = now;
                    }
                    if let Err(err) = e.deliver_due().await {
                        tracing::error!("notification delivery: {err}");
                    }
                    let next = e
                        .blocking(|e| e.next_due())
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or(u64::MAX);
                    let now = e.now();
                    let wait = next
                        .saturating_sub(now)
                        .min((last_tick + tick).saturating_sub(now))
                        .clamp(200, 60_000);
                    tokio::select! {
                        _ = e.0.wake.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
                    }
                }
            }
        };
        tokio::join!(ingest, deliver);
    }

    // ------------------------------------------------------------------- notifications

    pub fn list(&self, q: &ListQuery) -> Result<Vec<Notification>> {
        let mut sql = format!("SELECT {} FROM events WHERE 1=1", db::EVENT_COLS);
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        let mut arg = |sql: &mut String, cond: &str, v: Box<dyn rusqlite::ToSql>| {
            args.push(v);
            sql.push_str(&format!(" AND {cond} ?{}", args.len()));
        };
        if let Some(b) = q.before {
            arg(&mut sql, "id <", Box::new(b as i64));
        }
        if let Some(c) = q.category {
            arg(&mut sql, "category =", Box::new(c.as_str()));
        }
        if let Some(s) = q.state {
            arg(&mut sql, "state =", Box::new(s.as_str()));
        }
        if let Some(k) = &q.key {
            arg(&mut sql, "key =", Box::new(k.clone()));
        }
        if let Some(s) = &q.source {
            arg(&mut sql, "source =", Box::new(s.clone()));
        }
        if let Some(min) = q.min_severity {
            let sev: Vec<&str> = NotifySeverity::ALL
                .iter()
                .filter(|s| **s >= min)
                .map(|s| s.as_str())
                .collect();
            sql.push_str(&format!(" AND severity IN ('{}')", sev.join("','")));
        }
        match q.acked {
            Some(true) => sql.push_str(" AND acked_ms IS NOT NULL"),
            Some(false) => sql.push_str(" AND acked_ms IS NULL"),
            None => {}
        }
        sql.push_str(&format!(
            " ORDER BY id DESC LIMIT {}",
            q.limit.clamp(1, 500)
        ));
        let c = self.conn();
        let mut st = c.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        Ok(st
            .query_map(refs.as_slice(), db::event_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn unacked(&self) -> Result<u64> {
        Ok(unacked(&self.conn())?)
    }

    pub fn ack(&self, id: u64, user: &str) -> Result<Notification> {
        let c = self.conn();
        let now = self.now() as i64;
        c.execute(
            "UPDATE events SET acked_ms = ?2, acked_by = ?3 WHERE id = ?1 AND acked_ms IS NULL",
            params![id as i64, now, user],
        )?;
        let n = db::get_event(&c, id)?
            .ok_or_else(|| NotifyError::NotFound(format!("notification {id}")))?;
        self.publish(&c, NotificationChangeKind::Updated, Some(n.clone()));
        Ok(n)
    }

    pub fn ack_all(&self, user: &str) -> Result<u64> {
        let c = self.conn();
        let now = self.now() as i64;
        let n = c.execute(
            "UPDATE events SET acked_ms = ?1, acked_by = ?2 WHERE acked_ms IS NULL",
            params![now, user],
        )? as u64;
        let last: Option<i64> = c
            .query_row("SELECT MAX(id) FROM events", [], |r| r.get(0))
            .optional()?
            .flatten();
        let latest = match last {
            Some(id) => db::get_event(&c, id as u64)?,
            None => None,
        };
        self.publish(&c, NotificationChangeKind::AckedAll, latest);
        Ok(n)
    }

    pub fn silence(&self, req: &SilenceRequest, user: &str) -> Result<Silence> {
        let key = clean_line(&req.key, 200);
        if key.is_empty() || key == "*" {
            return Err(NotifyError::Invalid(
                "key: the key of a notification, or a prefix ending with '*'".into(),
            ));
        }
        if req.duration_s.is_some_and(|d| d == 0 || d > 366 * 86_400) {
            return Err(NotifyError::Invalid(
                "duration_s: 1 second to a year, or null for no end".into(),
            ));
        }
        let now = self.now();
        let until = req.duration_s.map(|d| now + d * 1000);
        let c = self.conn();
        let count: i64 = c.query_row("SELECT COUNT(*) FROM silences", [], |r| r.get(0))?;
        if count >= MAX_SILENCES {
            return Err(NotifyError::Invalid(format!(
                "at most {MAX_SILENCES} silences: remove some first"
            )));
        }
        c.execute(
            "INSERT INTO silences (key, created_ms, created_by, until_ms) VALUES (?1, ?2, ?3, ?4)",
            params![key, now as i64, user, until.map(|u| u as i64)],
        )?;
        Ok(Silence {
            id: c.last_insert_rowid() as u64,
            key,
            created_ms: now,
            created_by: user.into(),
            until_ms: until,
        })
    }

    pub fn silences(&self) -> Result<Vec<Silence>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT id, key, created_ms, created_by, until_ms FROM silences
             WHERE until_ms IS NULL OR until_ms > ?1 ORDER BY id DESC",
        )?;
        Ok(st
            .query_map([self.now() as i64], |r| {
                Ok(Silence {
                    id: r.get::<_, i64>(0)? as u64,
                    key: r.get(1)?,
                    created_ms: r.get::<_, i64>(2)? as u64,
                    created_by: r.get(3)?,
                    until_ms: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_silence(&self, id: u64) -> Result<()> {
        let n = self
            .conn()
            .execute("DELETE FROM silences WHERE id = ?1", [id as i64])?;
        if n == 0 {
            return Err(NotifyError::NotFound(format!("silence {id}")));
        }
        Ok(())
    }

    // ------------------------------------------------------------------------- targets

    fn target_view(&self, c: &Connection, id: u64) -> Result<NotifyTarget> {
        type Row = (
            String,
            i64,
            String,
            String,
            String,
            Option<i64>,
            Option<i64>,
            Option<String>,
        );
        let row: Option<Row> = c
            .query_row(
                "SELECT name, enabled, language, config, secret_names, last_ok_ms, last_error_ms, last_error
                 FROM targets WHERE id = ?1",
                [id as i64],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .optional()?;
        let (name, enabled, lang, config, names, ok, err_ms, err) =
            row.ok_or_else(|| NotifyError::NotFound(format!("target {id}")))?;
        let queued: i64 = c.query_row(
            "SELECT COUNT(*) FROM outbox WHERE target_id = ?1",
            [id as i64],
            |r| r.get(0),
        )?;
        Ok(NotifyTarget {
            id,
            name,
            enabled: enabled != 0,
            language: parse_lang(&lang),
            config: serde_json::from_str(&config)
                .map_err(|e| NotifyError::Storage(e.to_string()))?,
            secrets: serde_json::from_str(&names).unwrap_or_default(),
            status: TargetStatus {
                last_ok_ms: ok.map(|v| v as u64),
                last_error_ms: err_ms.map(|v| v as u64),
                last_error: err,
                queued: queued as u64,
            },
        })
    }

    pub fn targets(&self) -> Result<Vec<NotifyTarget>> {
        let c = self.conn();
        let ids: Vec<i64> = {
            let mut st = c.prepare("SELECT id FROM targets ORDER BY id")?;
            st.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        ids.into_iter()
            .map(|id| self.target_view(&c, id as u64))
            .collect()
    }

    pub fn target(&self, id: u64) -> Result<NotifyTarget> {
        self.target_view(&self.conn(), id)
    }

    fn validate_target(&self, req: &TargetRequest) -> Result<()> {
        let bad = |m: &str| Err(NotifyError::Invalid(m.to_string()));
        let name = req.name.trim();
        if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
            return bad("name: 1 to 100 printable characters");
        }
        let line =
            |s: &str| !s.trim().is_empty() && s.len() <= 512 && !s.chars().any(char::is_control);
        match &req.config {
            TargetConfig::Smtp {
                host,
                port,
                from,
                to,
                username,
                ..
            } => {
                if !line(host) || *port == 0 {
                    return bad("smtp: host and port are required");
                }
                if from.parse::<lettre::message::Mailbox>().is_err() {
                    return bad("smtp: from is not a valid address");
                }
                if to.is_empty() || to.len() > 20 {
                    return bad("smtp: 1 to 20 recipients");
                }
                if let Some(t) = to
                    .iter()
                    .find(|t| t.parse::<lettre::message::Mailbox>().is_err())
                {
                    return Err(NotifyError::Invalid(format!(
                        "smtp: {t:?} is not a valid address"
                    )));
                }
                if username
                    .as_deref()
                    .is_some_and(|u| !u.is_empty() && !line(u))
                {
                    return bad("smtp: invalid username");
                }
            }
            TargetConfig::Ntfy { server, topic, .. } => {
                if !http_url_ok(server) {
                    return bad("ntfy: server must be an http(s) URL");
                }
                if !line(topic) || topic.contains('/') || topic.contains('?') {
                    return bad("ntfy: invalid topic");
                }
            }
            TargetConfig::Telegram { chat_id, api_url } => {
                if !line(chat_id) {
                    return bad("telegram: chat_id is required");
                }
                if api_url.as_deref().is_some_and(|u| !http_url_ok(u)) {
                    return bad("telegram: api_url must be an http(s) URL");
                }
            }
            TargetConfig::Gotify { server } => {
                if !http_url_ok(server) {
                    return bad("gotify: server must be an http(s) URL");
                }
            }
            TargetConfig::Webhook {
                url, headers, body, ..
            } => {
                if !line(url) {
                    return bad("webhook: url is required");
                }
                if headers.len() > 20 {
                    return bad("webhook: at most 20 headers");
                }
                for h in headers {
                    if h.name.is_empty()
                        || h.name.len() > 64
                        || !h
                            .name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    {
                        return Err(NotifyError::Invalid(format!(
                            "webhook: invalid header name {:?}",
                            h.name
                        )));
                    }
                }
                let tpls = std::iter::once(url.as_str())
                    .chain(headers.iter().map(|h| h.value.as_str()))
                    .chain(body.as_deref());
                for t in tpls {
                    if t.len() > 16 << 10 {
                        return bad("webhook: template over 16 KiB");
                    }
                    self.0
                        .tpl
                        .check_user(t)
                        .map_err(|e| NotifyError::Invalid(format!("webhook: {e}")))?;
                }
            }
            TargetConfig::Ui {} => {}
        }
        if req.secrets.len() > 16 {
            return bad("at most 16 secrets");
        }
        for (k, v) in &req.secrets {
            if !secret_name_ok(k) {
                return Err(NotifyError::Invalid(format!(
                    "secret name {k:?}: lowercase letters, digits and _"
                )));
            }
            if let Some(v) = v
                && (v.is_empty() || v.len() > 4096 || v.chars().any(|c| c.is_control()))
            {
                return Err(NotifyError::Invalid(format!(
                    "secret {k}: 1 to 4096 characters"
                )));
            }
        }
        Ok(())
    }

    fn write_target(&self, id: Option<u64>, req: &TargetRequest) -> Result<NotifyTarget> {
        self.validate_target(req)?;
        let mut c = self.conn();
        let tx = c.transaction()?;
        let name = req.name.trim();
        let clash: Option<i64> = tx
            .query_row("SELECT id FROM targets WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .optional()?;
        if clash.is_some_and(|x| Some(x as u64) != id) {
            return Err(NotifyError::Exists(format!(
                "a target named {name:?} exists"
            )));
        }
        let mut secrets = match id {
            Some(id) => {
                let sealed: Option<Option<Vec<u8>>> = tx
                    .query_row(
                        "SELECT secrets FROM targets WHERE id = ?1",
                        [id as i64],
                        |r| r.get(0),
                    )
                    .optional()?;
                match sealed {
                    None => return Err(NotifyError::NotFound(format!("target {id}"))),
                    Some(Some(s)) => self
                        .0
                        .secrets
                        .open_sealed(&s)
                        .map_err(|_| NotifyError::Storage("secrets unreadable".into()))?,
                    Some(None) => BTreeMap::new(),
                }
            }
            None => {
                let n: i64 = tx.query_row("SELECT COUNT(*) FROM targets", [], |r| r.get(0))?;
                if n >= MAX_TARGETS {
                    return Err(NotifyError::Invalid(format!(
                        "at most {MAX_TARGETS} targets"
                    )));
                }
                BTreeMap::new()
            }
        };
        for (k, v) in &req.secrets {
            match v {
                Some(v) => secrets.insert(k.clone(), v.clone()),
                None => secrets.remove(k),
            };
        }
        let sealed = if secrets.is_empty() {
            None
        } else {
            Some(
                self.0
                    .secrets
                    .seal(&secrets)
                    .map_err(NotifyError::Storage)?,
            )
        };
        let names = serde_json::to_string(&secrets.keys().collect::<Vec<_>>()).unwrap_or_default();
        let config =
            serde_json::to_string(&req.config).map_err(|e| NotifyError::Invalid(e.to_string()))?;
        let lang = lang_str(req.language);
        let id = match id {
            Some(id) => {
                tx.execute(
                    "UPDATE targets SET name = ?2, enabled = ?3, language = ?4, config = ?5, secrets = ?6,
                       secret_names = ?7 WHERE id = ?1",
                    params![id as i64, name, req.enabled, lang, config, sealed, names],
                )?;
                if !req.enabled {
                    tx.execute("DELETE FROM outbox WHERE target_id = ?1", [id as i64])?;
                }
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO targets (name, enabled, language, config, secrets, secret_names)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![name, req.enabled, lang, config, sealed, names],
                )?;
                tx.last_insert_rowid() as u64
            }
        };
        tx.commit()?;
        self.0.buckets.lock().unwrap().remove(&id);
        self.target_view(&c, id)
    }

    pub fn create_target(&self, req: &TargetRequest) -> Result<NotifyTarget> {
        self.write_target(None, req)
    }

    pub fn update_target(&self, id: u64, req: &TargetRequest) -> Result<NotifyTarget> {
        self.write_target(Some(id), req)
    }

    /// Refused while a rule names the target.
    pub fn delete_target(&self, id: u64) -> Result<()> {
        if let Some(r) = self
            .rules_snapshot()
            .iter()
            .find(|r| r.rule.targets.contains(&id))
        {
            return Err(NotifyError::Conflict(format!(
                "target {id} is used by rule {:?}",
                r.rule.name
            )));
        }
        let c = self.conn();
        if c.execute("DELETE FROM targets WHERE id = ?1", [id as i64])? == 0 {
            return Err(NotifyError::NotFound(format!("target {id}")));
        }
        c.execute("DELETE FROM outbox WHERE target_id = ?1", [id as i64])?;
        self.0.buckets.lock().unwrap().remove(&id);
        Ok(())
    }

    /// Sends a test message now (not through the outbox); records the outcome.
    pub async fn test_target(&self, id: u64, user: &str) -> Result<TargetTestResult> {
        let target = self
            .blocking(move |e| {
                let c = e.conn();
                let view = e.target_view(&c, id)?;
                // a disabled target can be tested too
                let sealed: Option<Vec<u8>> = c.query_row(
                    "SELECT secrets FROM targets WHERE id = ?1",
                    [id as i64],
                    |r| r.get(0),
                )?;
                let secrets = match sealed {
                    Some(s) => {
                        e.0.secrets
                            .open_sealed(&s)
                            .map_err(|_| NotifyError::Storage("secrets unreadable".into()))?
                    }
                    None => BTreeMap::new(),
                };
                Ok((view, secrets))
            })
            .await?;
        let (view, secrets) = target;
        let (title, body) = template::test_texts(view.language);
        let now = self.now();
        let n = Notification {
            ts_ms: now,
            last_ms: now,
            count: 1,
            source: "notifications".into(),
            category: NotifyCategory::System,
            severity: NotifySeverity::Notice,
            key: "notifications:test".into(),
            state: EventState::Oneshot,
            title: title.into(),
            body: format!("{body} ({user})"),
            ..Default::default()
        };
        let ctx = vec![
            self.0
                .tpl
                .event_ctx(&n, false, view.language, self.0.o.local),
        ];
        let msg = self.0.tpl.render(ctx, view.language);
        let dest = channels::Dest {
            config: &view.config,
            language: view.language,
            secrets: &secrets,
        };
        let started = std::time::Instant::now();
        let r = channels::send(self.http(), &self.0.tpl, &dest, &msg).await;
        let duration_ms = started.elapsed().as_millis() as u64;
        let done = self.now();
        let err = r.clone().err();
        self.blocking(move |e| {
            let c = e.conn();
            match &err {
                None => c.execute(
                    "UPDATE targets SET last_ok_ms = ?2 WHERE id = ?1",
                    params![id as i64, done as i64],
                )?,
                Some(m) => c.execute(
                    "UPDATE targets SET last_error_ms = ?2, last_error = ?3 WHERE id = ?1",
                    params![id as i64, done as i64, m],
                )?,
            };
            Ok(())
        })
        .await?;
        Ok(TargetTestResult {
            ok: r.is_ok(),
            error: r.err(),
            duration_ms,
        })
    }

    // --------------------------------------------------------------------------- rules

    pub fn rules(&self) -> Result<Vec<NotifyRule>> {
        Ok(self
            .rules_snapshot()
            .iter()
            .map(|c| c.rule.clone())
            .collect())
    }

    pub fn rule(&self, id: u64) -> Result<NotifyRule> {
        self.rules_snapshot()
            .iter()
            .find(|c| c.rule.id == id)
            .map(|c| c.rule.clone())
            .ok_or_else(|| NotifyError::NotFound(format!("rule {id}")))
    }

    fn write_rule(&self, id: Option<u64>, rule: &NotifyRule) -> Result<NotifyRule> {
        matcher::validate(rule).map_err(NotifyError::Invalid)?;
        let mut rule = rule.clone();
        rule.name = rule.name.trim().to_string();
        rule.targets.sort_unstable();
        rule.targets.dedup();
        let mut c = self.conn();
        let tx = c.transaction()?;
        for t in &rule.targets {
            let ok: Option<i64> = tx
                .query_row("SELECT id FROM targets WHERE id = ?1", [*t as i64], |r| {
                    r.get(0)
                })
                .optional()?;
            if ok.is_none() {
                return Err(NotifyError::Invalid(format!("targets: no target {t}")));
            }
        }
        let clash: Option<i64> = tx
            .query_row("SELECT id FROM rules WHERE name = ?1", [&rule.name], |r| {
                r.get(0)
            })
            .optional()?;
        if clash.is_some_and(|x| Some(x as u64) != id) {
            return Err(NotifyError::Exists(format!(
                "a rule named {:?} exists",
                rule.name
            )));
        }
        let id = match id {
            Some(id) => {
                rule.id = id;
                let doc = serde_json::to_string(&rule).unwrap_or_default();
                if tx.execute(
                    "UPDATE rules SET name = ?2, doc = ?3 WHERE id = ?1",
                    params![id as i64, rule.name, doc],
                )? == 0
                {
                    return Err(NotifyError::NotFound(format!("rule {id}")));
                }
                id
            }
            None => {
                let n: i64 = tx.query_row("SELECT COUNT(*) FROM rules", [], |r| r.get(0))?;
                if n >= MAX_RULES {
                    return Err(NotifyError::Invalid(format!("at most {MAX_RULES} rules")));
                }
                rule.id = 0;
                tx.execute(
                    "INSERT INTO rules (name, doc) VALUES (?1, ?2)",
                    params![rule.name, serde_json::to_string(&rule).unwrap_or_default()],
                )?;
                let id = tx.last_insert_rowid() as u64;
                rule.id = id;
                tx.execute(
                    "UPDATE rules SET doc = ?2 WHERE id = ?1",
                    params![id as i64, serde_json::to_string(&rule).unwrap_or_default()],
                )?;
                id
            }
        };
        tx.commit()?;
        drop(c);
        self.reload_rules()?;
        self.rule(id)
    }

    pub fn create_rule(&self, rule: &NotifyRule) -> Result<NotifyRule> {
        self.write_rule(None, rule)
    }

    pub fn update_rule(&self, id: u64, rule: &NotifyRule) -> Result<NotifyRule> {
        self.write_rule(Some(id), rule)
    }

    pub fn delete_rule(&self, id: u64) -> Result<()> {
        if self
            .conn()
            .execute("DELETE FROM rules WHERE id = ?1", [id as i64])?
            == 0
        {
            return Err(NotifyError::NotFound(format!("rule {id}")));
        }
        self.reload_rules()
    }

    // ------------------------------------------------------------------------- imports

    /// Imports another tool's settings once (targets + a rule); returns the number of
    /// targets created (0 when already imported or nothing is configured).
    pub fn import_once(&self, imp: &crate::ImportedSettings) -> Result<usize> {
        if imp.key.is_empty() || db::meta_get(&self.conn(), &imp.key)?.is_some() {
            return Ok(0);
        }
        let mut ids = Vec::new();
        for t in &imp.targets {
            match self.create_target(t) {
                Ok(v) => ids.push(v.id),
                Err(e) => tracing::warn!("{}: target {:?}: {e}", imp.rule_name, t.name),
            }
        }
        if !ids.is_empty() {
            let rule = NotifyRule {
                name: imp.rule_name.clone(),
                enabled: true,
                min_severity: imp.min_severity,
                targets: ids.clone(),
                ..Default::default()
            };
            if let Err(e) = self.create_rule(&rule) {
                tracing::warn!("{}: rule: {e}", imp.rule_name);
            }
        }
        db::meta_set(&self.conn(), &imp.key, &self.now().to_string())?;
        Ok(ids.len())
    }
}

fn unacked(c: &Connection) -> rusqlite::Result<u64> {
    c.query_row(
        "SELECT COUNT(*) FROM events WHERE acked_ms IS NULL",
        [],
        |r| r.get::<_, i64>(0),
    )
    .map(|v| v as u64)
}

fn is_silenced(c: &Connection, key: &str, now: u64) -> rusqlite::Result<bool> {
    let mut st =
        c.prepare_cached("SELECT key FROM silences WHERE until_ms IS NULL OR until_ms > ?1")?;
    let keys = st.query_map([now as i64], |r| r.get::<_, String>(0))?;
    for k in keys {
        let k = k?;
        let hit = match k.strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => k == key,
        };
        if hit {
            return Ok(true);
        }
    }
    Ok(false)
}

fn insert_event(
    tx: &Transaction,
    ev: &NewEvent,
    now: u64,
    silenced: bool,
    severity: NotifySeverity,
) -> rusqlite::Result<u64> {
    tx.execute(
        "INSERT INTO events (ts_ms, last_ms, count, source, category, severity, key, state, title,
           body, fields, actions, silenced, code, body_format)
         VALUES (?1, ?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            now as i64,
            ev.source,
            ev.category.as_str(),
            severity.as_str(),
            ev.key.as_deref().unwrap_or_default(),
            ev.state.as_str(),
            ev.title,
            ev.body,
            serde_json::to_string(&ev.fields).unwrap_or_default(),
            serde_json::to_string(&ev.actions).unwrap_or_default(),
            silenced,
            ev.code,
            ev.body_format.as_str(),
        ],
    )?;
    Ok(tx.last_insert_rowid() as u64)
}

fn parse_lang(s: &str) -> NotifyLanguage {
    match s {
        "it" => NotifyLanguage::It,
        _ => NotifyLanguage::En,
    }
}

fn lang_str(l: NotifyLanguage) -> &'static str {
    match l {
        NotifyLanguage::En => "en",
        NotifyLanguage::It => "it",
    }
}
